//! less 编译器：作用域、规则展开、@import、打印。
//!
//! 语义对齐 less@4.1.3：
//! - 变量 lazy（同作用域后定义胜出，值在使用时求值）
//! - 声明与嵌套规则分离（父规则的声明先收集、合并输出，再展开子规则）
//! - 选择器组合顺序：子选择器在外层、父选择器在内层（less 实测）
//! - `&` 文本替换；`@import` 内联

pub(crate) mod comments;
pub(crate) mod emit;
pub(crate) mod env;
pub(crate) mod eval;
pub(crate) mod extend;
pub(crate) mod file_fn;
pub(crate) mod guard;
pub(crate) mod imports;
pub(crate) mod prelude;
pub(crate) mod selector;
pub(crate) mod value;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use oxc_css_parser::ast::*;
use oxc_css_parser::pos::Span;
use oxc_css_parser::{Allocator, ParserBuilder, Syntax};

use crate::doc::{self, Body, DeclItem, Item, Style};
use crate::merge::Merged;
use crate::{CompileError, Options, Output};

use comments::collect_definition_spans;
use emit::{ChildItem, Deferred, Piece, Sink};
use env::{Defs, Env, Inject, Roots, Vars};
pub(crate) use env::RuleSet;
use eval::collect_mixins;
use extend::{Need, Sel};
use imports::expand_imports;
use prelude::{has_plugin, is_charset, is_import};
use value::VarRef;

/// 去掉值文本末尾的 `!important`（来自变量值），返回去重后的文本。
pub(super) fn strip_embedded_important(text: &str) -> Option<String> {
    let trimmed = text.trim_end();
    let cut = trimmed.len().checked_sub("!important".len())?;
    if !trimmed[cut..].eq_ignore_ascii_case("!important") {
        return None;
    }
    Some(trimmed[..cut].trim_end().to_string())
}

pub(crate) struct Compiler<'a> {
    env: Env<'a>,
    /// 合并串与文件/段表（收尾交给 `doc::print`）。
    merged: &'a Merged,
    src: &'a str,
    /// 参与编译的源文件多于一个（`@import` 内联）；单文件保持既有 map 字节。
    multi_source: bool,
    file_path: PathBuf,
    /// 相对项目根的路径形态（用于错误信息/deps）。
    deps: Vec<String>,
    /// 被 @import 内联的文件绝对路径（去重）。
    seen: Vec<PathBuf>,
    /// 全部 mixin 定义（带括号与无括号），同名可多条（less 会全部匹配）。
    mixins: Defs<'a>,
    /// 当前所在的规则选择器 token 链（mixin 解析用）。
    path: Vec<String>,
    /// 与 `path` 等长：该层能否作为命名空间链路元素被零实参进入。
    path_linkable: Vec<bool>,
    /// 正在展开的 mixin 体与实参签名（防止无终止条件的自递归）。
    active: Vec<(*const SimpleBlock<'a>, String)>,
    /// 正在输出的规则体（less 的 `isRecursive`：同名规则不匹配自身）。
    rule_bodies: Vec<*const SimpleBlock<'a>>,
    /// 当前层是否处于 `!important` 调用展开的规则内（子规则继承）。
    force_important: bool,
    /// 块注释 span（`//` 行注释 less 丢弃，不收集）。
    comments: Vec<(usize, usize)>,
    /// 已随区块输出过的注释（避免根层 flush 重复输出）
    comment_done: Vec<bool>,
    comment_idx: usize,
    options: &'a Options,
    /// extend 求值器（选择器改写延后到收尾）
    extends: extend::Engine,
    /// 当前所在的媒体/at-rule 作用域
    scope: usize,
    /// 媒体块内延迟输出的嵌套媒体（less 会把条件合并后冒泡到同级）
    deferred: Vec<Vec<Deferred<'a>>>,
    /// 当前块所属 @media 的完整条件与所在层（非媒体块为 `None`）
    media: Vec<Option<(String, usize)>>,
    /// 是否已经输出过 @charset（less 只保留第一条）
    charset: bool,
    /// mixin / detached ruleset 的体区间（体内注释不归根层）
    def_spans: Vec<Span>,
    /// 被 `$prop` 引用过的声明（less 会在引用时重解析该声明，`!important` 丢掉前导空格）
    reparsed: std::collections::HashSet<usize>,
    /// 当前是否在 `(reference)` 导入体里输出（规则默认不可见、整块最终可能被删掉）
    hidden: bool,
    /// `(reference)` 块的产出区间：块里一条可见规则都没有时整段剪掉
    ref_ranges: Vec<(Vec<usize>, usize, usize)>,
}

pub(crate) fn compile(path: &Path, source: &str, opts: &Options) -> Result<Output, Box<CompileError>> {
    // less 把 `@import` 当作文本级内联：导入的文件与宿主共享作用域。
    // 入口文件也按规范路径登记，否则 `a -> b -> a` 的循环导入会把入口再内联一次
    let mut seen = vec![path.canonicalize().unwrap_or_else(|_| path.to_path_buf())];
    let mut deps = Vec::new();
    let mut merged = Merged::new();
    let part = expand_imports(
        path,
        source,
        opts,
        &mut seen,
        &mut Vec::new(),
        &mut deps,
        false,
        &mut merged,
    );
    merged.append_part(part);
    let source = merged.text.as_str();
    let alloc = Allocator::default();
    let mut parser = ParserBuilder::new(&alloc, source)
        .syntax(Syntax::Less)
        .comments()
        .build();
    let ast = parser.parse::<Stylesheet>().map_err(|e| {
        // 解析发生在合并串上，偏移经段表回到出错文件与原文位置
        let (file, local) = merged
            .locate_exact(e.span.start)
            .unwrap_or((0, e.span.start));
        let (path, line, column) = match merged.position_in(file, local) {
            Some((path, line, column)) => (Some(path), Some(line), Some(column)),
            None => (None, None, None),
        };
        Box::new(CompileError {
            message: format!("less 解析失败: {}", e.kind),
            path,
            line,
            column,
        })
    })?;

    let comments = parser
        .comments()
        .iter()
        .filter(|c| matches!(c.kind, oxc_css_parser::token::CommentKind::Block))
        .map(|c| (c.span.start, c.span.end))
        .collect::<Vec<_>>();

    let mut compiler = Compiler::new(path, &merged, opts);
    compiler.env.plugins = has_plugin(&ast.statements);
    compiler.comments = comments;
    compiler.comment_done = vec![false; compiler.comments.len()];
    compiler.seen = seen;
    compiler.deps = deps;
    let mut frames: Vec<Vars<'_>> = Vec::new();
    let mut conds: Vec<(&LessConditions<'_>, Inject<'_>)> = Vec::new();
    collect_mixins(
        &ast.statements,
        source,
        &mut frames,
        &mut Vec::new(),
        &mut Vec::new(),
        &mut conds,
        &mut compiler.mixins,
    );
    // mixin / detached ruleset 体内的块注释不归根层，由展开时逐次输出
    collect_definition_spans(&ast.statements, &mut compiler.def_spans);
    let mut root_layer = Layer::default();
    compiler.emit_block(
        &ast.statements,
        Head {
            sels: &[],
            spans: &[],
            extends: Vec::new(),
            root: true,
        },
        0,
        false,
        Span {
            start: 0,
            end: source.len(),
        },
        &mut root_layer,
    )?;
    compiler.flush_comments(source.len(), &mut root_layer);
    debug_assert!(root_layer.decls.is_empty(), "根层不产出裸声明");
    compiler.finish(root_layer.items)
}

impl<'a> Compiler<'a> {
    pub(super) fn new(path: &Path, merged: &'a Merged, opts: &'a Options) -> Self {
        let multi_source = merged.files.len() > 1;
        // globalVars（useCompilerPlugins 二元组 options）作为最外层文本变量
        let mut root = HashMap::new();
        for (k, v) in &opts.less.global_vars {
            root.insert(k.clone(), VarRef::Text(v.clone()));
        }
        let mut env = Env::new(root);
        // 文件类函数相对于入口 less 文件所在目录（less 的 `entryPath`）
        env.roots = Some(Rc::new(Roots {
            dir: path.parent().unwrap_or(Path::new(".")).to_path_buf(),
            paths: opts.paths.clone(),
        }));
        Self {
            env,
            merged,
            src: merged.text.as_str(),
            multi_source,
            file_path: path.to_path_buf(),
            deps: Vec::new(),
            seen: vec![path.to_path_buf()],
            mixins: Defs::default(),
            path: Vec::new(),
            path_linkable: Vec::new(),
            active: Vec::new(),
            rule_bodies: Vec::new(),
            force_important: false,
            comments: Vec::new(),
            comment_done: Vec::new(),
            comment_idx: 0,
            options: opts,
            extends: extend::Engine::new(),
            scope: 0,
            deferred: Vec::new(),
            media: Vec::new(),
            charset: false,
            def_spans: Vec::new(),
            reparsed: std::collections::HashSet::new(),
            hidden: false,
            ref_ranges: Vec::new(),
        }
    }

    /// 合并串切片（span 来自以合并串解析出的 AST）。
    pub(super) fn slice(&self, span: Span) -> &str {
        self.src.get(span.start..span.end).unwrap_or("")
    }

    /// 收尾：extend 改写选择器 → `(reference)` 剪枝 → 交给 `doc::print`。
    pub(super) fn finish(mut self, mut items: Vec<Item>) -> Result<Output, Box<CompileError>> {
        self.extends.run();
        rewrite(&mut items, &self.extends);
        // 剪枝要早于删占位符：区间下标是建树时的（删项会让下标失效）。
        prune(&mut items, &mut Vec::new(), &mut self.ref_ranges);
        drop_placeholders(&mut items);
        if let Some((message, span)) = self.env.misses.borrow().first().cloned() {
            let (path, line, column) = match self
                .merged
                .locate_exact(span.start)
                .and_then(|(file, local)| self.merged.position_in(file, local))
            {
                Some((path, line, column)) => (Some(path), Some(line), Some(column)),
                None => (None, None, None),
            };
            return Err(Box::new(CompileError {
                message,
                path,
                line,
                column,
            }));
        }
        let entry = self.file_path.to_string_lossy();
        // `sourcesContent` 只在多源时写（lessc 的 `outputSourceFiles` 跟内联走）。
        let style = Style {
            sources_content: self.multi_source,
            ..Style::LESS
        };
        let (css, map) = doc::print(&items, &style, &entry, self.merged, self.options);
        Ok(Output {
            css,
            map,
            deps: self.deps,
        })
    }

    /// 区块展开：变量收集 → 声明输出 → 子规则递归。
    ///
    /// `block_span` 是本块的 `{…}` 区间，用于把块注释归属到具体层级（less 里注释是所在
    /// 规则数组的一个节点）。块内注释包含「挂在子规则上的注释」，但不含子块内部的。
    pub(super) fn emit_block(
        &mut self,
        stmts: &'a [Statement<'a>],
        head: Head<'_>,
        level: usize,
        bare: bool,
        block_span: Span,
        layer: &mut Layer,
    ) -> Result<(), Box<CompileError>> {
        let saved_hidden = self.hidden;
        self.env.push();
        // 本块是否会输出带选择器的块（决定注释归属：块内注释进本块，根层注释留到 pass 4）
        let wrapped = !head.sels.is_empty() || bare;
        let mut pieces: Vec<Piece<'a>> = Vec::new();
        let mut children: Vec<ChildItem<'a>> = Vec::new();
        let mut needs: Vec<Need> = Vec::new();
        let mut sink = Sink {
            pieces: &mut pieces,
            children: &mut children,
            inject: None,
            needs: &mut needs,
        };
        self.collect_block(stmts, block_span, wrapped, &mut sink);
        // 规则登记要在子规则之前（less 的 extend 按树序收集）
        let rule = self.register_rule(&head, &needs);
        // 属性合并（`+:` / `+_:`）与 `$prop` 的属性表
        let pieces = self.merge_pieces(pieces);
        self.env.set_props(self.collect_props(&pieces));
        self.mark_reparsed_props(&pieces);
        let pieces = self.dedupe_pieces(pieces);

        // pass 3：输出本层规则（空块不输出；只有注释时也要包出选择器块）
        if !pieces.is_empty() && head.sels.is_empty() && bare {
            // @font-face / @page：块内是裸声明，没有选择器包裹
            for piece in &pieces {
                // `(reference)` 体顶层的注释是被屏蔽的节点，less 直接丢弃
                if self.hidden && matches!(piece, Piece::Comment(_)) {
                    continue;
                }
                let item = self.piece_item(piece);
                layer.decls.push(item);
            }
        } else if !pieces.is_empty() && !head.sels.is_empty() {
            // 选择器每行一个 `(String, Span)`：行文本逐字节等于 `a,\nb` 形态，
            // 续行的缩进与换行由 `doc::print` 按 `Style` 补。
            let selector = head
                .sels
                .iter()
                .enumerate()
                .map(|(i, sel)| {
                    let mut text = extend::render(sel);
                    if i + 1 < head.sels.len() {
                        text.push(',');
                    }
                    let span = head
                        .spans
                        .get(i)
                        .copied()
                        .unwrap_or(Span { start: 0, end: 0 });
                    (text, span)
                })
                .collect();
            let body = self.piece_body(&pieces);
            layer.items.push(Item::Rule(crate::doc::Rule {
                selector,
                body,
                group_end: false,
                extend_id: rule,
                placeholder: false,
            }));
        }

        // pass 4：子规则。根块里 `@charset` / `@import` 要先提出来（less `ruleset.genCSS` 的重排）。
        if head.root {
            for child in &children {
                if let Child::AtRule(at) = child.child
                    && is_charset(at)
                {
                    self.emit_child_at(at, child, level, head.sels, head.spans, layer)?;
                }
            }
            // less 把根层 `@import` 提到所有其它节点之前（`importNodeIndex`），
            // 但让位于它之前的注释。
            let front_limit = children
                .iter()
                .find_map(|c| match c.child {
                    Child::AtRule(at) if is_import(at) || is_charset(at) => None,
                    _ => Some(c.at),
                })
                .unwrap_or(usize::MAX);
            self.flush_comments(front_limit, layer);
            for child in &children {
                if let Child::AtRule(at) = child.child
                    && self.hoists(at, &child.inject)
                {
                    self.emit_child_at(at, child, level, head.sels, head.spans, layer)?;
                }
            }
        }
        for child in &children {
            if head.root
                && let Child::AtRule(at) = child.child
                && (is_charset(at) || self.hoists(at, &child.inject))
            {
                continue;
            }
            self.flush_comments(child.at, layer);
            match child.child {
                Child::Rule(rule) => self.emit_child_rule(
                    &rule.selector,
                    &rule.block,
                    child.inject.clone(),
                    child.important,
                    head.sels,
                    head.spans,
                    level,
                    layer,
                )?,
                Child::CondRule(rule) => self.emit_child_rule(
                    &rule.selector,
                    &rule.block,
                    child.inject.clone(),
                    child.important,
                    head.sels,
                    head.spans,
                    level,
                    layer,
                )?,
                Child::AtRule(at) => {
                    self.emit_child_at(at, child, level, head.sels, head.spans, layer)?
                }
                Child::Ref(block) => {
                    self.emit_reference(block, head.sels, head.spans, level, layer)?
                }
                Child::Keyframe(k) => self.emit_keyframe(k, level, layer)?,
                Child::Other(f) => self.emit_other(f, layer),
            }
        }

        self.env.pop();
        self.hidden = saved_hidden;
        Ok(())
    }

    /// `(reference)` 导入体的输出：内容照常展开（规则登记为不可见），
    /// 被 extend 命中的规则在收尾时以扩展者选择器输出，其余整块删掉。
    pub(super) fn emit_reference(
        &mut self,
        block: &'a SimpleBlock<'a>,
        parents: &[Sel],
        parent_spans: &[Span],
        level: usize,
        layer: &mut Layer,
    ) -> Result<(), Box<CompileError>> {
        let start = layer.items.len();
        let saved = self.hidden;
        self.hidden = true;
        // 导入体顶层的注释是「被屏蔽的节点」：less 丢弃，且不能从根层漏出来
        self.own_comments(block.span, &block.statements);
        let result = self.emit_block(
            &block.statements,
            Head {
                sels: parents,
                spans: parent_spans,
                extends: vec![Vec::new(); parents.len()],
                root: false,
            },
            level,
            false,
            block.span,
            layer,
        );
        self.hidden = saved;
        self.ref_ranges
            .push((layer.path.clone(), start, layer.items.len()));
        result
    }

    /// 根层裸函数调用：求值后整行输出（`e('…');` 输出内文）。
    /// 内文是随意的 CSS 文本，无法拆解成 IR 节点，只落 `Raw`（映射指向整个调用）。
    pub(super) fn emit_other(&mut self, f: &'a Function<'a>, layer: &mut Layer) {
        let text = value::eval_function_stmt(f, self.src, &self.env, 0);
        layer.items.push(Item::Raw(crate::doc::Raw {
            text,
            marks: vec![(0, f.span)],
        }));
    }

    /// `@{name}` / `${prop}` 插值替换。
    pub(super) fn interpolate(&self, text: &str) -> String {
        value::interpolate(text, self.src, &self.env, 0, None)
    }

    /// 带源位置的插值：未定义变量要按 less 报错（属性名/选择器/extend 等真正求值处）。
    pub(super) fn interpolate_at(&self, text: &str, span: Span) -> String {
        value::interpolate(text, self.src, &self.env, 0, Some(span))
    }

    /// 属性名（已插值，含 IE `*` hack 前缀/后缀）。
    /// 静默版：合并/去重/`$prop` 比对时调用，此时注入作用域未必就位。
    pub(super) fn prop_name(&self, d: &Declaration) -> String {
        let name = self.interpolate(self.slice(*d.name.span()));
        self.prop_name_with(d, name)
    }

    pub(super) fn prop_name_with(&self, d: &Declaration, name: String) -> String {
        match (d.name_prefix, d.name_suffix) {
            (None, None) => name,
            (prefix, suffix) => format!(
                "{}{name}{}",
                prefix.map(String::from).unwrap_or_default(),
                suffix.map(String::from).unwrap_or_default()
            ),
        }
    }
}

/// 一条已输出的规则头（选择器列表 + 选项）。
pub(super) struct Head<'s> {
    sels: &'s [Sel],
    /// 与 `sels` 等长：每条选择器在源里的区间（空：at-rule 透传/生成的选择器）
    spans: &'s [Span],
    /// 每条选择器自己挂的 `:extend(...)`（与 `sels` 等长）
    extends: Vec<Vec<Need>>,
    /// 最外层块：`@charset` / `@import` 要提到最前（less `ruleset.genCSS` 的重排）
    root: bool,
}

/// 一层的产出：子节点 `items` + （at-rule 体的）裸声明 `decls`。
///
/// `path` 是这一层在整棵树里的位置（祖先 `items` 下标），`(reference)` 剪枝按它
/// 回到对应的子项区间。非 at-rule 体的层 `decls` 恒空。
#[derive(Default)]
pub(super) struct Layer {
    path: Vec<usize>,
    items: Vec<Item>,
    decls: Vec<DeclItem>,
}

impl Layer {
    /// 第 `index` 个子项的体层。
    fn child(&self, index: usize) -> Layer {
        let mut path = self.path.clone();
        path.push(index);
        Layer {
            path,
            items: Vec::new(),
            decls: Vec::new(),
        }
    }

    /// 体：正常是 `Decls`；at-rule 体里同时有声明与子规则时（`@media screen { color: red;
    /// .a {…} }`），`Body` 的单一形态表达不了「先声明、后子规则」，所以声明整段按当前层
    /// 缩进拼成一块 `Raw` 放在子规则之前（已对 lessc 4.1.3 实测逐字节一致）。
    fn into_body(self, level: usize) -> Body {
        if self.items.is_empty() {
            return Body::Decls(self.decls);
        }
        if self.decls.is_empty() {
            return Body::Rules(self.items);
        }
        let pad = format!("\n{}", "  ".repeat(level));
        let text = self
            .decls
            .iter()
            .map(decl_text)
            .collect::<Vec<_>>()
            .join(&pad);
        let mut items = self.items;
        items.insert(
            0,
            Item::Raw(crate::doc::Raw {
                text,
                marks: Vec::new(),
            }),
        );
        Body::Rules(items)
    }
}

enum Child<'a> {
    Rule(&'a QualifiedRule<'a>),
    /// 带 guard 的规则（`.a when (...)`）
    CondRule(&'a LessConditionalQualifiedRule<'a>),
    AtRule(&'a AtRule<'a>),
    /// `@keyframes` 体里的 `0%` / `from` 块
    Keyframe(&'a KeyframeBlock<'a>),
    /// 根层裸函数调用（`e('text');`）
    Other(&'a Function<'a>),
    /// `(reference)` 导入体：内容照常展开但默认不输出（被 extend 命中才输出）
    Ref(&'a SimpleBlock<'a>),
}

/// extend 回写：把可见选择器换回 IR 的选择器行（每行带自己的源锚点）。
///
/// 不可见的规则只打 `placeholder`（`doc::print` 跳过它），不再事后剪文本。
fn rewrite(items: &mut [Item], engine: &extend::Engine) {
    for item in items.iter_mut() {
        match item {
            Item::Rule(rule) => {
                let Some(id) = rule.extend_id else { continue };
                let sels = engine.visible_selectors(id);
                if sels.is_empty() {
                    rule.placeholder = true;
                    continue;
                }
                let last = sels.len() - 1;
                rule.selector = sels
                    .iter()
                    .enumerate()
                    .map(|(i, (sel, span))| {
                        let mut text = extend::render(sel);
                        if i < last {
                            text.push(',');
                        }
                        (text, *span)
                    })
                    .collect();
            }
            Item::AtRule(at) => {
                if let Some(Body::Rules(nested)) = at.body.as_mut() {
                    rewrite(nested, engine);
                }
            }
            Item::Keyframe(_) | Item::Comment(_) | Item::Raw(_) => {}
        }
    }
}

/// 丢掉占位符规则（优先于打印，保证空 `@media` 一并消失）。
fn drop_placeholders(items: &mut Vec<Item>) {
    items.retain(|item| !matches!(item, Item::Rule(rule) if rule.placeholder));
    for item in items.iter_mut() {
        if let Item::AtRule(at) = item
            && let Some(Body::Rules(nested)) = at.body.as_mut()
        {
            drop_placeholders(nested);
        }
    }
}

/// `(reference)` 剪枝：区间里一条可见规则都没有时整段删除。
///
/// 区间用建树时的「层路径 + 子项下标」记录（等价于改动前的输出字节区间），
/// 嵌套 `@media` 是冒泡到壳外的兄弟节点，所以不会被误算成壳的内容。
fn prune(
    items: &mut Vec<Item>,
    path: &mut Vec<usize>,
    ranges: &mut Vec<(Vec<usize>, usize, usize)>,
) {
    for (i, item) in items.iter_mut().enumerate() {
        if let Item::AtRule(at) = item
            && let Some(Body::Rules(nested)) = at.body.as_mut()
        {
            path.push(i);
            prune(nested, path, ranges);
            path.pop();
        }
    }
    let mut mine: Vec<(usize, usize)> = ranges
        .iter()
        .filter(|(p, _, _)| p.as_slice() == path.as_slice())
        .map(|&(_, start, end)| (start, end))
        .collect();
    // 从后往前删，前面的下标不失效。
    mine.sort_unstable_by(|a, b| b.0.cmp(&a.0));
    for (start, end) in mine {
        if start < end && end <= items.len() && !items[start..end].iter().any(has_visible_rule) {
            items.drain(start..end);
        }
    }
}

/// `(reference)` 剪枝的保留判据：区间里是否有会产出字节的规则。
///
/// 注释、`Raw`（根层函数调用/裸注释）、`@import` 这类无体 at-rule 与只含声明的
/// 隐藏 at-rule（`(reference)` 里的 `@font-face`）都不算——与改动前按可见规则判定一致。
fn has_visible_rule(item: &Item) -> bool {
    match item {
        Item::Rule(rule) => !rule.placeholder,
        Item::Keyframe(_) => true,
        Item::AtRule(at) => match at.body.as_ref() {
            Some(Body::Rules(nested)) => nested.iter().any(has_visible_rule),
            Some(Body::Decls(_)) | None => false,
        },
        Item::Comment(_) | Item::Raw(_) => false,
    }
}

/// 一条声明的序列化文本（`Raw` 落点用，与 `doc::print` 的形态一致）。
fn decl_text(item: &DeclItem) -> String {
    match item {
        DeclItem::Decl(decl) => match decl.custom {
            true => format!("{}:{};", decl.name, decl.value),
            false => format!("{}: {};", decl.name, decl.value),
        },
        DeclItem::Comment(comment) => format!("/*{}*/", comment.text),
        DeclItem::AtRule(at) => match at.prelude.is_empty() {
            true => format!("@{};", at.name),
            false => format!("@{} {};", at.name, at.prelude),
        },
    }
}
