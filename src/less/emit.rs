use std::collections::HashMap;

use oxc_css_parser::ast::*;
use oxc_css_parser::pos::Span;

use super::comments::strip_block_comments;
use super::env::Inject;
use super::eval::Out;
use super::extend::{self, Need, Sel};
use super::imports::{is_css_path, split_import_options, split_quoted_path};
use super::prelude::{
    at_rule_keeps_parents, is_hoisted_import, is_import, is_reference, is_remote,
    normalize_prelude, strip_escapes,
};
use super::selector::{
    is_just_parent, scope_tokens, selector_tokens, split_interpolated, split_selectors, stmt_start,
};
use super::value::{self, PropSource, eval};
use super::{Child, Compiler, Head, Layer, strip_embedded_important};
use crate::doc::{AtRule as OutAtRule, Body, Comment, Decl, DeclItem, Item, Keyframe, Raw};
use crate::{CompileError, Options};

/// 一条可输出声明：带 mixin 注入作用域与 `!important` 强制位。
#[derive(Clone)]
pub(super) struct Emit<'a> {
    pub(super) decl: &'a Declaration<'a>,
    pub(super) inject: Option<Inject<'a>>,
    pub(super) important: bool,
}

/// 延后到 pass 4 输出的子节点（普通子规则或 mixin 体里的子规则）。
pub(super) struct ChildItem<'a> {
    /// 触发它的语句起点（注释归属用）
    pub(super) at: usize,
    pub(super) child: Child<'a>,
    pub(super) inject: Option<Inject<'a>>,
    pub(super) important: bool,
}

/// 媒体块内延迟输出的嵌套媒体。
pub(super) struct Deferred<'a> {
    pub(super) at: &'a AtRule<'a>,
    pub(super) sels: Vec<Sel>,
    pub(super) spans: Vec<Span>,
    pub(super) level: usize,
    /// 外层媒体的完整条件（合并用）
    pub(super) outer: String,
    /// 冒泡后仍要用的作用域（`@ruleset()` 的形参帧在延迟输出时还活着）
    pub(super) inject: Option<Inject<'a>>,
}

/// `collect_block` 的输出汇与作用域。
pub(super) struct Sink<'a, 'c> {
    pub(super) pieces: &'c mut Vec<Piece<'a>>,
    pub(super) children: &'c mut Vec<ChildItem<'a>>,
    /// `&` 折叠块的局部作用域（普通块为 `None`）
    pub(super) inject: Option<Inject<'a>>,
    /// 本块的 `&:extend(...)` / `:extend(...)`
    pub(super) needs: &'c mut Vec<Need>,
}

pub(super) enum Piece<'a> {
    /// 块注释（IR 形态：文本已去掉 `/*` `*/`）
    Comment(Comment),
    Decl(Emit<'a>),
    /// 属性合并（`+:` / `+_:`）后的单条声明，占用同名首条声明的位置
    Merged(Vec<Emit<'a>>),
    /// 规则体内没有块的 at-rule（`@apply …;`）：less 把它当本层的节点
    AtRule(Span),
}

/// 合并组拆分：`+:` 开头另起一组（逗号），组内按空格拼接。返回下标分组。
pub(super) fn merge_groups(decls: &[&Declaration]) -> Vec<Vec<usize>> {
    let mut groups: Vec<Vec<usize>> = Vec::new();
    let mut space: Vec<usize> = Vec::new();
    for (i, d) in decls.iter().enumerate() {
        let comma = matches!(
            d.less_property_merge.as_ref().map(|m| &m.kind),
            Some(LessPropertyMergeKind::Comma)
        );
        if comma && !space.is_empty() {
            groups.push(std::mem::take(&mut space));
        }
        space.push(i);
    }
    groups.push(space);
    groups
}

/// 子块（会递归输出）的区间，用于把注释归属到最内层块。
/// 节点树里是否含命名空间查找（决定运算要不要走文本求值）。
pub(super) fn has_lookup(v: &ComponentValue) -> bool {
    match v {
        ComponentValue::LessNamespaceValue(_) => true,
        ComponentValue::LessBinaryOperation(op) => has_lookup(&op.left) || has_lookup(&op.right),
        ComponentValue::LessParenthesizedOperation(p) => has_lookup(&p.operation),
        _ => false,
    }
}

/// less `anonymousValue()` 的字符集：命中则整段值按源文本输出（不解析、不求值）。
pub(super) fn anonymous_value(raw: &str) -> bool {
    !raw.chars().any(|c| {
        matches!(
            c,
            '.' | '#'
                | '@'
                | '$'
                | '+'
                | '/'
                | '\''
                | '"'
                | '*'
                | '`'
                | '('
                | ';'
                | '{'
                | '}'
                | '-'
        )
    })
}

/// `@media` prelude 里的注释挂到下一个值上：在随后的 `:` 后插入 ` /*…*/`。
/// 返回找不到 `:` 的余下注释（由调用方移到体内）。
pub(super) fn attach_to_feature_values(
    text: &str,
    pending: &[(usize, Comment)],
) -> (String, Vec<Comment>) {
    let mut out = text.to_string();
    let mut shift = 0isize;
    let mut leftover = Vec::new();
    for (pos, comment) in pending {
        let from = (*pos as isize + shift).max(0) as usize;
        match out.get(from..).and_then(|rest| rest.find(':')) {
            Some(rel) => {
                let at = from + rel + 1;
                let inserted = format!(" /*{}*/ ", comment.text);
                out.insert_str(at, &inserted);
                shift += inserted.len() as isize;
            }
            None => leftover.push(comment.clone()),
        }
    }
    (out, leftover)
}

/// `(reference)` 导入体的包装 at-rule。
/// 无块 at-rule 里 `@name` 之后的文本（引子）。
pub(super) fn blockless_rest<'s>(at: &AtRule, src: &'s str) -> &'s str {
    let start = at.name.span().end;
    let end = at.span.end.max(start).min(src.len());
    src.get(start..end).unwrap_or("").trim()
}

/// 值里的 `$prop` 引用名（`$color` → `color`）。
pub(super) fn collect_prop_vars<'a>(cv: &ComponentValue<'a>, out: &mut Vec<String>) {
    match cv {
        ComponentValue::LessPropertyVariable(p) => out.push(p.name.name.to_string()),
        ComponentValue::LessBinaryOperation(op) => {
            collect_prop_vars(&op.left, out);
            collect_prop_vars(&op.right, out);
        }
        ComponentValue::LessParenthesizedOperation(p) => collect_prop_vars(&p.operation, out),
        ComponentValue::Function(f) => {
            for a in &f.args {
                collect_prop_vars(a, out);
            }
        }
        _ => {}
    }
}

impl<'a> Compiler<'a> {
    /// 无块 at-rule 的裸语句（`@nope;` / `@nope [foo];`）：less 把它当变量调用，
    /// 根层报错且不产出 CSS（以前会直接输出一行 `@nope;`）。
    pub(super) fn bare_at_rule(&mut self, at: &'a AtRule, wrapped: bool) {
        let rest = blockless_rest(at, self.src);
        if rest.is_empty() {
            self.env.miss(
                "Missing '[...]' lookup in variable call".to_string(),
                at.span,
            );
            return;
        }
        if wrapped {
            return;
        }
        let name = at.name.name;
        let message = match self.var_ruleset(name, 0) {
            // 变量是 detached ruleset：查找结果是一条声明，根层不允许
            Some(_) => {
                "Properties must be inside selector blocks. They cannot be in the root".to_string()
            }
            None if self.env.get(name).is_none() => format!("variable @{name} is undefined"),
            None => format!("Could not evaluate variable call @{name}"),
        };
        self.env.miss(message, at.span);
    }

    /// 根层 `@import` 是否按原样提前：prelude 要按子项的注入作用域插值后再判断
    /// （`.add_an_import("file.css")` 这类 mixin 里的 `@import "@{x}"`）。
    pub(super) fn hoists(&self, at: &AtRule, inject: &Option<Inject<'a>>) -> bool {
        if !is_import(at) {
            return false;
        }
        let Some(raw) = at
            .prelude
            .as_ref()
            .and_then(|p| self.src.get(p.span().start..p.span().end))
        else {
            return false;
        };
        let text = match inject {
            Some(inject) => {
                let env = self.env.scoped(inject);
                value::interpolate(raw, self.src, &env, 0, None)
            }
            None => self.interpolate(raw),
        };
        is_hoisted_import(&text)
    }

    /// 输出一条 at-rule 子项（带上 mixin 注入作用域与 `!important` 状态）。
    pub(super) fn emit_child_at(
        &mut self,
        at: &'a AtRule,
        child: &ChildItem<'a>,
        level: usize,
        parents: &[Sel],
        parent_spans: &[Span],
        layer: &mut Layer,
    ) -> Result<(), Box<CompileError>> {
        let saved = self.force_important;
        self.force_important |= child.important;
        self.env.push_scope(&child.inject);
        let result = self.emit_at_rule(
            at,
            parents,
            parent_spans,
            level,
            child.inject.clone(),
            layer,
        );
        self.env.pop_scope(&child.inject);
        self.force_important = saved;
        result
    }

    /// 展开一条子规则（选择器插值要在调用处作用域里求值）。
    #[allow(clippy::too_many_arguments)]
    pub(super) fn emit_child_rule(
        &mut self,
        sel: &SelectorList,
        block: &'a SimpleBlock<'a>,
        inject: Option<Inject<'a>>,
        important: bool,
        parents: &[Sel],
        parent_spans: &[Span],
        level: usize,
        layer: &mut Layer,
    ) -> Result<(), Box<CompileError>> {
        let saved = self.force_important;
        let saved_path = self.path.len();
        self.force_important |= important;
        // 选择器里的块注释：less 在 `selectors()` 里清空 commentStore，直接丢掉
        self.mark_comments(sel.span);
        self.env.push_scope(&inject);
        let own = self.selector_sels(sel);
        let mut combined: Vec<Sel> = Vec::new();
        let mut combined_spans: Vec<Span> = Vec::new();
        let mut extends: Vec<Vec<Need>> = Vec::new();
        for (child, needs, child_span) in &own {
            // 插值带出的逗号先按文本拆开，再与父选择器组合（less 的插值就是文本级替换）：
            // `.d@{.a,.b,.c}&:hover` → `.d.a`, `.b`, `.c&:hover`，各自再拼父选择器。
            let text = extend::render(child);
            let parts = split_selectors(&text);
            let pieces = match parts.len() {
                0 => vec![child.clone()],
                1 => vec![child.clone()],
                _ => parts.iter().map(|p| extend::parse(p)).collect(),
            };
            for piece in pieces {
                // 映射用来源区间取路径最外层选择器：嵌套规则的首个映射落在父规则行
                // （与 less 的 `Selector.elements[0].span` 一致）。
                let mut push = |product: Sel, span: Span| {
                    for sel in split_interpolated(product) {
                        combined.push(sel);
                        combined_spans.push(span);
                        extends.push(needs.clone());
                    }
                };
                let products = extend::combine(parents, &piece);
                // 映射按父路径取来源区间（顺序与 combine 的父选择器循环一致）；
                // 单文件/无父选择器沿用子选择器区间，与改动前的 map 字节一致。
                let spans: Vec<Span> = {
                    let root_span = |idx: usize| match self.multi_source {
                        true if !parents.is_empty() && products.len() == parents.len() => {
                            parent_spans.get(idx).copied().unwrap_or(*child_span)
                        }
                        true => parent_spans.first().copied().unwrap_or(*child_span),
                        false => *child_span,
                    };
                    (0..products.len()).map(root_span).collect()
                };
                for (product, span) in products.into_iter().zip(spans) {
                    push(product, span);
                }
            }
        }
        let tokens = own
            .first()
            .map(|(sel, _, _)| selector_tokens(&extend::render(sel)))
            .map(scope_tokens)
            .unwrap_or_default();
        self.path_linkable
            .extend(std::iter::repeat_n(true, tokens.len()));
        self.path.extend(tokens);
        self.rule_bodies.push(block);
        self.emit_block(
            &block.statements,
            Head {
                sels: &combined,
                spans: &combined_spans,
                extends,
                root: false,
            },
            level,
            false,
            block.span,
            layer,
        )?;
        self.rule_bodies.pop();
        self.path.truncate(saved_path);
        self.path_linkable.truncate(saved_path);
        self.env.pop_scope(&inject);
        self.force_important = saved;
        Ok(())
    }

    /// 收集一层块：pass 1 注册变量、pass 2 收集声明与子规则（保持源序）。
    pub(super) fn collect_block(
        &mut self,
        stmts: &'a [Statement<'a>],
        block_span: Span,
        wrapped: bool,
        sink: &mut Sink<'a, '_>,
    ) {
        let Sink {
            pieces,
            children,
            inject,
            needs,
        } = sink;
        for st in stmts {
            match st {
                Statement::LessVariableDeclaration(d) => {
                    self.note_var(d);
                }
                // (reference) 体里的变量照常可见
                Statement::AtRule(a) if is_reference(a) => {
                    for inner in a.block.iter().flat_map(|b| b.statements.iter()) {
                        if let Statement::LessVariableDeclaration(d) = inner {
                            self.note_var(d);
                        }
                    }
                }
                _ => {}
            }
        }
        // less 的 `Variable.eval` 会求值每条变量声明的值（即使没被引用），
        // 值里的未定义引用同样报错；此时同作用域的变量已收集齐（惰性求值仍成立）。
        for st in stmts {
            if let Statement::LessVariableDeclaration(d) = st {
                self.eval_var_value(d);
            }
        }
        let own = if wrapped {
            self.own_comments(block_span, stmts)
        } else {
            Vec::new()
        };
        let mut next = 0;
        for st in stmts {
            let start = stmt_start(st).unwrap_or(usize::MAX);
            while next < own.len() && own[next].span.start < start {
                pieces.push(Piece::Comment(own[next].clone()));
                next += 1;
            }
            match st {
                Statement::Declaration(d) => {
                    // 根层声明 less 报「properties in root」；我们保持宽松，但属性名里的
                    // 未定义插值仍按 less 的求值顺序报错。
                    if !wrapped {
                        let span = *d.name.span();
                        self.interpolate_at(self.slice(span), span);
                    }
                    pieces.push(Piece::Decl(Emit {
                        decl: d,
                        inject: inject.clone(),
                        important: false,
                    }))
                }
                Statement::LessMixinCall(call) => {
                    let mut out = Out {
                        pieces,
                        children,
                        needs,
                    };
                    self.expand_mixin(call, start, false, &mut out, 0);
                }
                // `@dr();`：detached ruleset 调用
                Statement::LessVariableCall(call) => {
                    let mut out = Out {
                        pieces,
                        children,
                        needs,
                    };
                    self.expand_ruleset_call(call, start, &mut out);
                }
                // `each(list, ruleset)` 是语句级函数：要展开出规则
                Statement::LessFunctionCall(f)
                    if value::function_name(f).eq_ignore_ascii_case("each") =>
                {
                    let mut sink = Sink {
                        pieces,
                        children,
                        inject: inject.clone(),
                        needs,
                    };
                    self.expand_each(f, &mut sink);
                }
                // 根层的裸函数调用（`e('text');`）：less 当普通节点输出
                Statement::LessFunctionCall(f) if !wrapped => children.push(ChildItem {
                    at: start,
                    child: Child::Other(f),
                    inject: inject.clone(),
                    important: false,
                }),
                Statement::QualifiedRule(rule) => {
                    // 单个 `&` 的嵌套规则被折进父规则（less `isJustParentSelector`）
                    if is_just_parent(&rule.selector, self.src) {
                        let mut out = Out {
                            pieces,
                            children,
                            needs,
                        };
                        self.inline_ruleset(&rule.block, &mut out);
                    } else {
                        children.push(ChildItem {
                            at: start,
                            child: Child::Rule(rule),
                            inject: inject.clone(),
                            important: false,
                        });
                    }
                }
                Statement::LessConditionalQualifiedRule(rule) => {
                    if !self.rule_guard_passes(&rule.guard) {
                        continue;
                    }
                    if is_just_parent(&rule.selector, self.src) {
                        let mut out = Out {
                            pieces,
                            children,
                            needs,
                        };
                        self.inline_ruleset(&rule.block, &mut out);
                    } else {
                        children.push(ChildItem {
                            at: start,
                            child: Child::CondRule(rule),
                            inject: inject.clone(),
                            important: false,
                        });
                    }
                }
                Statement::KeyframeBlock(k) => children.push(ChildItem {
                    at: start,
                    child: Child::Keyframe(k),
                    inject: inject.clone(),
                    important: false,
                }),
                // (reference) 导入体：规则登记为不可见，收尾时按 extend 命中情况输出/删除
                Statement::AtRule(at) if is_reference(at) => {
                    if let Some(block) = &at.block {
                        children.push(ChildItem {
                            at: start,
                            child: Child::Ref(block),
                            inject: inject.clone(),
                            important: false,
                        });
                    }
                }
                // `@plugin` 只在解析期加载插件，不产生输出（less 的 plugin manager 消费）
                Statement::AtRule(at) if at.name.name.eq_ignore_ascii_case("plugin") => {}
                // 裸标识符/变量语句：less 当变量调用求值，根层报错且不产出 CSS
                Statement::AtRule(at)
                    if at.block.is_none()
                        && (blockless_rest(at, self.src).is_empty()
                            || !wrapped && blockless_rest(at, self.src).starts_with('[')) =>
                {
                    self.bare_at_rule(at, wrapped);
                }
                // 规则体内没有块的 at-rule（`@apply x;`）：less 把它当本层的节点，留在块里
                Statement::AtRule(at) if wrapped && at.block.is_none() => {
                    pieces.push(Piece::AtRule(Span {
                        start: at.span.start,
                        end: at.span.end,
                    }))
                }
                Statement::AtRule(at) => children.push(ChildItem {
                    at: start,
                    child: Child::AtRule(at),
                    inject: inject.clone(),
                    important: false,
                }),
                Statement::LessExtendRule(rule) => needs.extend(self.needs_of(&rule.extend)),
                _ => {}
            }
        }
        while next < own.len() {
            pieces.push(Piece::Comment(own[next].clone()));
            next += 1;
        }
    }

    /// 把一条 piece 变成声明体里的节点（`Raw` 落点与紧凑输出共用）。
    pub(super) fn piece_item(&mut self, piece: &Piece<'a>) -> DeclItem {
        match piece {
            Piece::Comment(c) => DeclItem::Comment(c.clone()),
            Piece::Decl(e) => {
                self.env.push_scope(&e.inject);
                let decl = self.emit_declaration(e.decl, e.important);
                self.env.pop_scope(&e.inject);
                DeclItem::Decl(decl)
            }
            Piece::Merged(decls) => DeclItem::Decl(self.emit_merged(decls)),
            Piece::AtRule(span) => DeclItem::AtRule(self.blockless_at(*span)),
        }
    }

    /// 规则体的声明序列。
    ///
    /// `(reference)` 体是唯一例外：声明节点 `isVisible()` 为 undefined，less 的
    /// `ruleset.genCSS` 不插换行（`a: 1;b: 2;`），而 `Body::Decls` 的打印恒是一行一条，
    /// 只能整段落 `Raw`（声明照常求值，只失去逐条映射）。
    pub(super) fn piece_body(&mut self, pieces: &[Piece<'a>]) -> Body {
        let items = pieces.iter().map(|piece| self.piece_item(piece)).collect();
        if !self.hidden {
            return Body::Decls(items);
        }
        let mut text = String::new();
        let mut marks = Vec::new();
        for item in &items {
            match item {
                DeclItem::Comment(c) => text.push_str(&format!("/*{}*/", c.text)),
                DeclItem::Decl(decl) => {
                    marks.push((text.len(), decl.name_span));
                    text.push_str(&super::decl_text(item));
                }
                DeclItem::AtRule(_) => text.push_str(&super::decl_text(item)),
            }
        }
        Body::Rules(vec![Item::Raw(Raw { text, marks })])
    }

    /// 规则体内没有块的 at-rule（`@apply x;`）：less 原样回声，只拆出名字与引子。
    fn blockless_at(&self, span: Span) -> OutAtRule {
        let text = self.interpolate_at(self.slice(span).trim(), span);
        let body = text.strip_prefix('@').unwrap_or(&text);
        let (name, prelude) = match body.find(char::is_whitespace) {
            Some(i) => (
                body[..i].to_string(),
                body[i..]
                    .trim_start_matches(char::is_whitespace)
                    .to_string(),
            ),
            None => (body.to_string(), String::new()),
        };
        OutAtRule {
            name_span: Span {
                start: span.start + 1,
                end: span.start + 1 + name.len(),
            },
            name,
            prelude,
            body: None,
            group_end: false,
            hoist: false,
        }
    }

    /// 同名属性合并后的单条声明：`+:` 用逗号、`+_:` 用空格分隔（less `_mergeRules`）。
    pub(super) fn emit_merged(&mut self, items: &[Emit<'a>]) -> Decl {
        let refs: Vec<&Declaration<'a>> = items.iter().map(|e| e.decl).collect();
        self.env.push_scope(&items[0].inject);
        let name = self.prop_name_at(refs[0]);
        self.env.pop_scope(&items[0].inject);
        let mut value = String::new();
        let mut embedded = false;
        for (gi, group) in merge_groups(&refs).iter().enumerate() {
            if gi > 0 {
                value.push_str(", ");
            }
            for (di, &idx) in group.iter().enumerate() {
                let e = &items[idx];
                if di > 0 {
                    value.push(' ');
                }
                self.env.push_scope(&e.inject);
                embedded |= self.emit_value_list(&e.decl.value, &mut value);
                self.env.pop_scope(&e.inject);
            }
        }
        // less `_mergeRules`：`!important` 提升到合并后的整条声明末尾（只一次）
        if embedded
            || items
                .iter()
                .any(|e| e.decl.important.is_some() || e.important)
            || self.force_important
        {
            value.push_str(" !important");
        }
        Decl {
            name,
            name_span: *refs[0].name.span(),
            value_span: refs[0].value.first().map_or(refs[0].span, |v| *v.span()),
            value,
            custom: false,
            custom_strip: None,
        }
    }

    /// 单条声明：属性名带源锚点，值整段文本（值里的 token 不再单独映射）。
    pub(super) fn emit_declaration(
        &mut self,
        d: &'a Declaration<'a>,
        force_important: bool,
    ) -> Decl {
        let name = self.prop_name_at(d);
        let name_span = *d.name.span();
        let value_span = d.value.first().map_or(d.span, |v| *v.span());
        let forced = force_important || self.force_important;
        let region = self.decl_value_region(d);
        // 属性名到首个值 token 之间的注释：less 在 `ruleProperty()` 后清空 commentStore，直接丢掉
        let value_start = d.value.first().map_or(d.span.end, |v| v.span().start);
        self.mark_comments(Span {
            start: d.span.start,
            end: value_start,
        });
        // 值里的注释由 `emit_values_in` 原位输出（或按 less 丢弃），一律标为已处理
        if let Some((start, end, _)) = region {
            self.mark_comments(Span { start, end });
        }
        let mut value = String::new();
        // less `anonymousValue()`：值里没有 `. # @ $ + / ' " * ` ( ; { } -` 时整段原文输出
        if let Some((start, end, true)) = region {
            let raw = self.src[start..end].trim_start();
            if anonymous_value(raw) {
                // 被 `$prop` 重解析过的声明：`!important` 丢了前导空格（less 的就地变更）
                let text = match self.reparsed.contains(&d.span.start) {
                    true => raw.trim_end().strip_suffix("!important").map_or_else(
                        || raw.to_string(),
                        |body| format!("{}!important", body.trim_end()),
                    ),
                    false => raw.to_string(),
                };
                value.push_str(&text);
                if forced && !text.trim_end().ends_with("!important") {
                    value.push_str(" !important");
                }
                return Decl {
                    name,
                    name_span,
                    value,
                    value_span,
                    custom: false,
                    custom_strip: None,
                };
            }
        }
        let embedded = self.emit_values_in(&d.value, region, &mut value);
        let important = d.important.is_some() || forced || embedded;
        if important {
            // 被 `$prop` 重解析过的声明丢了前导空格（less 的就地变更）
            if self.reparsed.contains(&d.span.start) {
                value.push_str("!important");
            } else {
                value.push_str(" !important");
            }
        }
        Decl {
            name,
            name_span,
            value,
            value_span,
            custom: false,
            custom_strip: None,
        }
    }

    /// 声明的值区间：`:` 之后到本声明的终止符（`;` / `}` / `{` / EOF）。
    /// 返回 `(开始, 终止符位置, 是否以 `;` 结束)`。
    pub(super) fn decl_value_region(&self, d: &Declaration) -> Option<(usize, usize, bool)> {
        self.region_from(d.colon_span.end)
    }

    /// 从 `start` 扫到本语句的终止符（跳过引号/注释/括号内内容）。
    pub(super) fn region_from(&self, start: usize) -> Option<(usize, usize, bool)> {
        if start > self.src.len() {
            return None;
        }
        let bytes = self.src.as_bytes();
        let mut i = start;
        let mut depth = 0usize;
        let mut quote = 0u8;
        while i < bytes.len() {
            let c = bytes[i];
            if quote != 0 {
                if c == b'\\' {
                    i += 2;
                    continue;
                }
                if c == quote {
                    quote = 0;
                }
                i += 1;
                continue;
            }
            match c {
                b'"' | b'\'' => quote = c,
                b'/' if bytes.get(i + 1) == Some(&b'*') => {
                    i = self.src[i + 2..]
                        .find("*/")
                        .map_or(bytes.len(), |j| i + 2 + j + 2);
                    continue;
                }
                b'/' if bytes.get(i + 1) == Some(&b'/') => {
                    i = self.src[i..].find('\n').map_or(bytes.len(), |j| i + j);
                    continue;
                }
                b'(' | b'[' => depth += 1,
                b')' | b']' => depth = depth.saturating_sub(1),
                b';' | b'}' | b'{' if depth == 0 => {
                    return Some((start, i, c == b';'));
                }
                _ => {}
            }
            i += 1;
        }
        Some((start, bytes.len(), false))
    }

    /// 值列表：逐段求值；段间空白按 less `Expression` 归一为单个空格，
    /// 逗号两侧归一为 `, `，`/` 两侧保留源文本（less 的 `Operation` 保留 isSpaced）。
    ///
    /// 段间夹着块注释时保留注释原位（less 把注释当作值列表里的节点）。
    ///
    /// 返回是否在值里遇到了 `!important`（来自变量值），并把它从文本里摘掉——
    /// less 把整条声明的 `!important` 归一到末尾只输出一次。
    pub(super) fn emit_value_list(
        &mut self,
        values: &'a [ComponentValue<'a>],
        out: &mut String,
    ) -> bool {
        self.emit_values_in(values, None, out)
    }

    /// `emit_value_list` 带值区间：区间里最后一项之后到终止符之间还可能有注释。
    pub(super) fn emit_values_in(
        &mut self,
        values: &'a [ComponentValue<'a>],
        region: Option<(usize, usize, bool)>,
        out: &mut String,
    ) -> bool {
        let mut prev_end: Option<usize> = None;
        let mut after_solidus = false;
        let mut embedded = false;
        for v in values {
            let span = *v.span();
            let comma =
                matches!(v, ComponentValue::Delimiter(d) if matches!(d.kind, DelimiterKind::Comma));
            let solidus = matches!(
                v,
                ComponentValue::Delimiter(d) if matches!(d.kind, DelimiterKind::Solidus)
            );
            if let Some(end) = prev_end {
                if end <= span.start {
                    let raw = self.slice(Span {
                        start: end,
                        end: span.start,
                    });
                    let gap = if raw.contains("/*") {
                        // less 把注释当值列表里的节点：前后各留一个空格
                        let body = raw.split_whitespace().collect::<Vec<_>>().join(" ");
                        if comma {
                            String::new()
                        } else if solidus || after_solidus {
                            raw.to_string()
                        } else {
                            format!(" {body} ")
                        }
                    } else if comma {
                        String::new()
                    } else if solidus || after_solidus {
                        raw.to_string()
                    } else {
                        " ".to_string()
                    };
                    out.push_str(&gap);
                }
            }
            // `10px / 2 * 2`：oxc 把 `/` 切成分隔符，右段在 less 里是除法链的一部分
            // （math 关闭时整体原样输出），所以按源文本替换而不是求值。
            let text = match after_solidus {
                true if matches!(v, ComponentValue::LessBinaryOperation(_)) => {
                    let raw = self.slice(span).trim().to_string();
                    value::substitute(&raw, self.src, &self.env, 0, Some(span))
                }
                _ => self.eval_text(v),
            };
            let text = match strip_embedded_important(&text) {
                Some(stripped) => {
                    embedded = true;
                    stripped
                }
                None => text,
            };
            out.push_str(&text);
            after_solidus = solidus;
            prev_end = Some(span.end);
        }
        // 最后一项到终止符：普通空白丢掉（less 的 `Expression` 不留尾空白），
        // 但块注释要在原位保留。
        if let (Some(end), Some((_, stop, semi))) = (prev_end, region)
            && end <= stop
            && stop <= self.src.len()
        {
            let tail = &self.src[end..stop];
            if tail.contains("/*") {
                let comment_at = tail.find("/*").unwrap_or(0);
                // less 的 `Expression` 在值节点与注释之间留一个空格
                out.push(' ');
                out.push_str(tail[comment_at..].trim_end());
            } else if semi && !embedded && tail.trim().is_empty() {
                out.push_str(tail);
            }
        }
        embedded
    }

    /// 单段求值（变量在 Scope 里查，文本变量直接返回）。
    pub(super) fn eval_text(&self, v: &'a ComponentValue<'a>) -> String {
        match v {
            // `@p[text]` / `#ns[$prop]`：命名空间查找在取值时才发生（延迟解析）
            ComponentValue::LessNamespaceValue(ns) => match self.namespace_text(ns, 0) {
                Ok(text) => text,
                Err(message) => {
                    self.env.miss(message, ns.span);
                    String::new()
                }
            },
            // 查找结果参与运算（`@sizes[base] * 2`）
            ComponentValue::LessBinaryOperation(op)
                if has_lookup(&op.left) || has_lookup(&op.right) =>
            {
                let left = self.eval_text(&op.left);
                let right = self.eval_text(&op.right);
                value::arithmetic(&op.op.kind, &left, &right)
                    .unwrap_or_else(|| format!("{left} {right}"))
            }
            _ => eval(v, self.src, &self.env, 0),
        }
    }

    /// 真正输出属性名时用：未定义变量按 less 报错。
    pub(super) fn prop_name_at(&self, d: &Declaration) -> String {
        let span = *d.name.span();
        let name = self.interpolate_at(self.slice(span), span);
        self.prop_name_with(d, name)
    }

    /// 把 `+:` / `+_:` 的同名声明合并到首条位置（less `_mergeRules`）。
    pub(super) fn merge_pieces(&self, pieces: Vec<Piece<'a>>) -> Vec<Piece<'a>> {
        let mut out: Vec<Piece<'a>> = Vec::with_capacity(pieces.len());
        let mut slots: HashMap<String, usize> = HashMap::new();
        for piece in pieces {
            match piece {
                Piece::Decl(e) if e.decl.less_property_merge.is_some() => {
                    let name = self.prop_name(e.decl);
                    match slots.get(&name) {
                        Some(&i) => {
                            if let Piece::Merged(items) = &mut out[i] {
                                items.push(e);
                            }
                        }
                        None => {
                            slots.insert(name, out.len());
                            out.push(Piece::Merged(vec![e]));
                        }
                    }
                }
                other => out.push(other),
            }
        }
        out
    }

    /// less 的 `ruleset.parseValue` 会就地重解析被 `$prop` 引用的声明，重解析后的
    /// 声明 `important` 字段是 `!important`（无前导空格）——输出时没有空格。
    pub(super) fn mark_reparsed_props(&mut self, pieces: &[Piece<'a>]) {
        let mut names: Vec<String> = Vec::new();
        for piece in pieces {
            if let Piece::Decl(e) = piece {
                for v in &e.decl.value {
                    collect_prop_vars(v, &mut names);
                }
            }
        }
        for name in names {
            for src in self.env.get_prop(&name).unwrap_or_default() {
                match src {
                    PropSource::Decl(d) => {
                        if d.important.is_some() {
                            self.reparsed.insert(d.span.start);
                        }
                    }
                    PropSource::Merged(groups) => {
                        for d in groups.iter().flatten() {
                            if d.important.is_some() {
                                self.reparsed.insert(d.span.start);
                            }
                        }
                    }
                }
            }
        }
    }

    /// 本层属性表：同名多条保留全部（`$prop` 取最后一条或合并结果）。
    pub(super) fn collect_props(
        &self,
        pieces: &[Piece<'a>],
    ) -> HashMap<String, Vec<PropSource<'a>>> {
        let mut map: HashMap<String, Vec<PropSource<'a>>> = HashMap::new();
        for piece in pieces {
            match piece {
                Piece::Decl(e) => {
                    let name = self.prop_name(e.decl);
                    map.entry(name).or_default().push(PropSource::Decl(e.decl));
                }
                Piece::Merged(items) => {
                    let name = self.prop_name(items[0].decl);
                    let refs: Vec<&Declaration<'a>> = items.iter().map(|e| e.decl).collect();
                    let groups = merge_groups(&refs)
                        .into_iter()
                        .map(|g| g.into_iter().map(|i| refs[i]).collect())
                        .collect();
                    map.entry(name)
                        .or_default()
                        .push(PropSource::Merged(groups));
                }
                Piece::Comment(_) | Piece::AtRule(_) => {}
            }
        }
        map
    }

    pub(super) fn emit_at_rule(
        &mut self,
        at: &'a AtRule,
        parents: &[Sel],
        parent_spans: &[Span],
        level: usize,
        inject: Option<Inject<'a>>,
        layer: &mut Layer,
    ) -> Result<(), Box<CompileError>> {
        let name = at.name.name.to_string();
        // 单文件保持既有 map：at-rule 透传的选择器不单独映射。
        let parent_spans = if self.multi_source { parent_spans } else { &[] };
        if name == "import" {
            return self.emit_import(at, layer);
        }
        // less 的 `@charset` 只输出第一条（后续的丢弃）
        if name.eq_ignore_ascii_case("charset") {
            if self.charset {
                return Ok(());
            }
            self.charset = true;
        }
        // less `JoinSelectorVisitor.visitAtRule`：声明式 at-rule（@font-face/@keyframes/@page…）
        // 的体是 root ruleset，父选择器不往里传播（只有 @document/@supports 会继承）。
        let (parents, parent_spans) = if at_rule_keeps_parents(&name) {
            (parents, parent_spans)
        } else {
            (&[][..], &[][..])
        };
        // 媒体块内再出现 @media：条件是合并且冒泡到同级（less 的 media bubble）
        if name.eq_ignore_ascii_case("media")
            && let Some((outer, outer_level)) = self.media.last().cloned().flatten()
        {
            self.deferred
                .last_mut()
                .expect("media defer stack")
                .push(Deferred {
                    at,
                    sels: parents.to_vec(),
                    spans: parent_spans.to_vec(),
                    level: outer_level,
                    outer,
                    inject,
                });
            return Ok(());
        }
        self.emit_at_rule_inner(at, parents, parent_spans, level, None, layer)
    }

    /// `@keyframes` 体里的百分比/关键字块（oxc 把它归为独立语句，不是规则）。
    pub(super) fn emit_keyframe(
        &mut self,
        k: &'a KeyframeBlock<'a>,
        level: usize,
        layer: &mut Layer,
    ) -> Result<(), Box<CompileError>> {
        let start = layer.items.len();
        let raw = self
            .slice(Span {
                start: k.span.start,
                end: k.block.span.start,
            })
            .trim()
            .to_string();
        let text = self.interpolate_at(
            &raw,
            Span {
                start: k.span.start,
                end: k.block.span.start,
            },
        );
        let sels: Vec<Sel> = split_selectors(&text)
            .iter()
            .map(|s| {
                vec![extend::Element {
                    combinator: String::new(),
                    value: s.clone(),
                }]
            })
            .collect();
        self.emit_block(
            &k.block.statements,
            Head {
                sels: &sels,
                spans: &[],
                extends: vec![Vec::new(); sels.len()],
                root: false,
            },
            level,
            false,
            k.block.span,
            layer,
        )?;
        // `@keyframes` 体内的块是 keyframe 而不是 style rule：彼此之间没有空行。
        let frames = layer.items.split_off(start);
        for frame in frames {
            layer.items.push(match frame {
                Item::Rule(rule) => Item::Keyframe(Keyframe {
                    selector: rule.selector,
                    body: rule.body,
                }),
                other => other,
            });
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn emit_at_rule_inner(
        &mut self,
        at: &'a AtRule,
        parents: &[Sel],
        parent_spans: &[Span],
        level: usize,
        outer: Option<String>,
        layer: &mut Layer,
    ) -> Result<(), Box<CompileError>> {
        let name = at.name.name.to_string();
        let (prelude, moved) = self.at_prelude(at);
        let is_media = name.eq_ignore_ascii_case("media");
        let hoist = outer.is_some();
        let prelude = match outer {
            Some(outer) if !prelude.is_empty() => {
                if outer.is_empty() {
                    prelude
                } else {
                    format!("{outer} and {prelude}")
                }
            }
            other => other.unwrap_or(prelude),
        };
        // 先占位：壳在 `items` 里的下标就是 `(reference)` 剪枝的区间起点。
        let index = layer.items.len();
        layer.items.push(Item::AtRule(OutAtRule {
            name,
            name_span: *at.name.span(),
            prelude: prelude.clone(),
            body: None,
            group_end: false,
            hoist,
        }));
        match &at.block {
            Some(block) => {
                let scope = self.extends.push_scope(Some(self.scope));
                let saved = self.scope;
                self.scope = scope;
                self.media.push(is_media.then(|| (prelude.clone(), level)));
                self.deferred.push(Vec::new());
                let has_body = !block.statements.is_empty();
                let mut body = layer.child(index);
                // 空块里的 prelude 注释：@keyframes 这类标识符型 at-rule 靠注释撑出块，
                // @media 的空块整条丢掉（less `to-css-visitor` 的 prune）。
                if has_body || !moved.is_empty() && !is_media {
                    body.items.extend(moved.into_iter().map(Item::Comment));
                }
                self.emit_block(
                    &block.statements,
                    Head {
                        sels: parents,
                        spans: parent_spans,
                        extends: vec![Vec::new(); parents.len()],
                        root: false,
                    },
                    level + 1,
                    true,
                    block.span,
                    &mut body,
                )?;
                let deferred = self.deferred.pop().unwrap_or_default();
                self.media.pop();
                self.scope = saved;
                let body = body.into_body(level + 1);
                // 空块不输出（less 丢弃空规则集/空媒体块）
                if body.is_empty() {
                    layer.items.pop();
                } else if let Item::AtRule(shell) = &mut layer.items[index] {
                    shell.body = Some(body);
                    // 壳的区间到此为止：冒泡出去的嵌套媒体在壳外，不算壳的内容
                    if self.hidden {
                        self.ref_ranges.push((layer.path.clone(), index, index + 1));
                    }
                }
                // 块内的嵌套媒体现在追在块后面（less 的输出顺序）
                for item in deferred {
                    self.env.push_scope(&item.inject);
                    self.emit_at_rule_inner(
                        item.at,
                        &item.sels,
                        &item.spans,
                        item.level,
                        Some(item.outer),
                        layer,
                    )?;
                    self.env.pop_scope(&item.inject);
                }
            }
            None => layer.items.extend(moved.into_iter().map(Item::Comment)),
        }
        Ok(())
    }

    /// `@import "x.less";` 内联。
    pub(super) fn emit_import(
        &mut self,
        at: &'a AtRule,
        layer: &mut Layer,
    ) -> Result<(), Box<CompileError>> {
        let Some(prelude) = &at.prelude else {
            return Ok(());
        };
        let raw = self.slice(*prelude.span()).trim().to_string();
        // 注释不算路径/媒体条件：less 把它们移到语句之外（仍作为块注释输出）
        let raw = strip_block_comments(&raw);
        let raw = self.interpolate_at(&raw, *prelude.span());
        let (options, _) = split_import_options(&raw);
        let optional = options.iter().any(|o| o == "optional");
        let inline = options.iter().any(|o| o == "inline");
        let forced_less = options.iter().any(|o| o == "less");
        // 媒体条件里的变量在当前位置求值（less 在 prelude 层求值）
        let raw = value::substitute(&raw, self.src, &self.env, 0, Some(*prelude.span()));
        let (_, rest) = split_import_options(&raw);
        let Some((href, media)) = split_quoted_path(&rest) else {
            // 无路径 → 原样输出
            self.push_at(layer, at, raw.trim());
            return Ok(());
        };
        let media = media.trim().to_string();

        // 远程 / .css / .wxss / `?css`（未标 (less)）→ 保留为 CSS import（不内联）
        if !forced_less
            && !inline
            && (is_remote(&href) || is_css_path(&href) || href.ends_with(".wxss"))
        {
            // 媒体条件按 less 的归一化输出（`(min-width:100px)` → `(min-width: 100px)`）
            let text = match raw.ends_with(&media) && !media.is_empty() {
                true => format!(
                    "{} {}",
                    raw[..raw.len() - media.len()].trim_end(),
                    normalize_prelude(&strip_escapes(&media))
                ),
                false => raw.clone(),
            };
            self.push_at(layer, at, &text);
            return Ok(());
        }

        // 规范化路径：emit 期内联要与文本内联共用同一套 import-once 去重键
        let Some(resolved) = self
            .resolve_import(&href)
            .map(|p| p.canonicalize().unwrap_or(p))
        else {
            if optional {
                return Ok(());
            }
            return Err(CompileError::new(format!(
                "@import 未找到: {href}（from {}）",
                self.file_path.display()
            ))
            .into());
        };
        if inline && !forced_less {
            // less 的 (inline)：原文写出，不参与解析；带媒体条件时整体包进 @media
            let text = std::fs::read_to_string(&resolved)
                .map_err(|e| Box::new(CompileError::new(e.to_string())))?;
            self.deps.push(resolved.to_string_lossy().to_string());
            let text = if media.is_empty() {
                text
            } else {
                format!(
                    "@media {} {{\n  {text}\n}}",
                    normalize_prelude(&strip_escapes(&media))
                )
            };
            self.push_raw(layer, text);
            return Ok(());
        }
        if self.seen.contains(&resolved) {
            return Ok(());
        }
        self.seen.push(resolved.clone());
        self.deps.push(resolved.to_string_lossy().to_string());

        let source = std::fs::read_to_string(&resolved).map_err(|e| {
            Box::new(CompileError::new(format!(
                "读取 {} 失败: {e}",
                resolved.display()
            )))
        })?;

        // 第一版：@import 用独立编译实例内联（变量作用域不跨文件，map 不跨文件）。
        // less 的 import 其实是文本级合并（共享作用域），这是已知偏差，后续补。
        let sub_opts = Options {
            paths: self.options.paths.clone(),
            less: crate::LessOptions {
                global_vars: self.options.less.global_vars.clone(),
            },
            source_map: false,
            project_root: None,
        };
        let sub = crate::less::compile(&resolved, &source, &sub_opts)?;
        // 子编译产物自带尾随换行；`Raw` 的项间换行由 `doc::print` 补。
        let css = sub.css.strip_suffix('\n').unwrap_or(&sub.css);
        self.push_raw(layer, css.to_string());
        self.deps.extend(sub.deps);
        Ok(())
    }

    /// 回显一条保留为 CSS 的 `@import`（不内联）。
    ///
    /// less 回显的是求值后的引子（路径 + 媒体条件），所以按 `AtRule` 落项：名字映射到
    /// `@`，引子原样出（与改动前的字节一致）。
    fn push_at(&self, layer: &mut Layer, at: &AtRule, prelude: &str) {
        layer.items.push(Item::AtRule(OutAtRule {
            name: at.name.name.to_string(),
            name_span: *at.name.span(),
            prelude: prelude.to_string(),
            body: None,
            group_end: false,
            hoist: false,
        }));
    }

    /// 整段原样落一个 `Raw` 子项（`(inline)` 文件内容、子编译产物）。
    ///
    /// 空文本不落项：IR 的项间换行会凭空多出一个空行。
    fn push_raw(&self, layer: &mut Layer, text: String) {
        if text.is_empty() {
            return;
        }
        layer.items.push(Item::Raw(Raw {
            text,
            marks: Vec::new(),
        }));
    }
}
