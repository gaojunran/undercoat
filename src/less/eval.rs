use std::collections::HashMap;
use std::rc::Rc;

use oxc_css_parser::ast::*;
use oxc_css_parser::pos::Span;

use super::emit::{ChildItem, Emit, Piece, Sink};
use super::env::{CallArg, Candidate, Defs, Frames, Inject, MixinDef, RuleSet, Vars, body_vars};
use super::extend::Need;
use super::guard::{self, Ctx, Val};
use super::prelude::is_reference;
use super::selector::{
    is_just_parent, rule_own_tokens, rule_selector_raw_lists, scope_tokens, selector_tokens,
    stmt_start,
};
use super::strip_embedded_important;
use super::value::{self, Color, VarRef};
use super::{Child, Compiler, Head};

/// 空格分隔的值列表在该实参里的元素（其余实参算一个元素）。
pub(super) fn space_items<'a, F>(node: &ComponentValue<'a>, text: &str, eval: F) -> Vec<String>
where
    F: Fn(&ComponentValue<'a>) -> String,
{
    match node {
        // 带定界符的列表（`5px/10px`）在 less 里是一个 Operation，不是空格列表
        ComponentValue::LessList(l)
            if l.comma_spans.is_none()
                && !l.elements.is_empty()
                && !l
                    .elements
                    .iter()
                    .any(|e| matches!(e, ComponentValue::Delimiter(_))) =>
        {
            l.elements.iter().map(&eval).collect()
        }
        _ => vec![text.to_string()],
    }
}

/// 变参形参绑定的列表：单个实参取其元素，多个实参各算一个。
pub(super) fn rest_items(rest: &[&CallArg<'_>]) -> Vec<String> {
    match rest {
        [one] => one.items.clone(),
        many => many.iter().map(|a| a.text.clone()).collect(),
    }
}

/// less `new Expression(items).eval()`：单元素时拍平成该元素本身的列表。
pub(super) fn expression_list(items: Vec<String>) -> Vec<String> {
    if items.len() != 1 {
        return items;
    }
    let parts = value::split_items(&items[0]);
    if parts.is_empty() { items } else { parts }
}

/// 预扫描全部 mixin 定义（调用可前向引用），同时为每个定义拍下闭包作用域。
pub(super) fn collect_mixins<'a>(
    stmts: &'a [Statement<'a>],
    src: &str,
    frames: &mut Vec<Vars<'a>>,
    path: &mut Vec<String>,
    link: &mut Vec<bool>,
    conds: &mut Vec<(&'a LessConditions<'a>, Inject<'a>)>,
    out: &mut Defs<'a>,
) {
    let mut vars = Vars::new();
    for st in stmts {
        if let Statement::LessVariableDeclaration(d) = st {
            vars.insert(d.name.name.name.to_string(), VarRef::Ast(&d.value));
        }
    }
    frames.push(vars);
    let saved = path.len();
    let saved_link = link.len();
    for st in stmts {
        match st {
            Statement::LessMixinDefinition(d) => {
                let own: Vec<String> = mixin_def_raw(&d.name, src).into_iter().collect();
                if !own.is_empty() {
                    // 自身 guard 也约束嵌套定义（`#top when (...) { #deep > .m() }`）
                    let outer: Vec<(&LessConditions<'a>, Inject<'a>)> = conds.clone();
                    if let Some(g) = &d.guard {
                        conds.push((g, Rc::new(frames.clone())));
                    }
                    let own = interp_tokens(&own, src, &Frames(frames));
                    let linkable = Compiler::allows_empty_args(&d.params.params);
                    let tokens = [&path[..], &own].concat();
                    out.insert(MixinDef {
                        chain: chain_mask(link, linkable, tokens.len()),
                        own: own.clone(),
                        tokens,
                        params: Some(&d.params),
                        guard: d.guard.as_ref(),
                        conds: outer,
                        block: &d.block,
                        closure: Rc::new(frames.clone()),
                    });
                    if d.guard.is_some() {
                        conds.pop();
                    }
                    link.extend(std::iter::repeat_n(linkable, own.len()));
                    path.extend(scope_tokens(own));
                }
                // 必填形参的命名空间无法以零实参链接（less `matchArgs(null)`）
                collect_mixins(&d.block.statements, src, frames, path, link, conds, out);
            }
            Statement::QualifiedRule(r) => {
                let lists = rule_selector_raw_lists(&r.selector, src);
                for own in &lists {
                    let own = interp_tokens(own, src, &Frames(frames));
                    let tokens = [&path[..], &own].concat();
                    out.insert(MixinDef {
                        chain: chain_mask(link, true, tokens.len()),
                        own,
                        tokens,
                        params: None,
                        guard: None,
                        conds: conds.clone(),
                        block: &r.block,
                        closure: Rc::new(frames.clone()),
                    });
                }
                let own = scope_tokens(rule_own_tokens(&r.selector, src, &Frames(frames)));
                link.extend(std::iter::repeat_n(true, own.len()));
                path.extend(own);
                collect_mixins(&r.block.statements, src, frames, path, link, conds, out);
            }
            Statement::LessConditionalQualifiedRule(r) => {
                let lists = rule_selector_raw_lists(&r.selector, src);
                for own in &lists {
                    let own = interp_tokens(own, src, &Frames(frames));
                    let tokens = [&path[..], &own].concat();
                    out.insert(MixinDef {
                        chain: chain_mask(link, true, tokens.len()),
                        own,
                        tokens,
                        params: None,
                        guard: Some(&r.guard),
                        conds: conds.clone(),
                        block: &r.block,
                        closure: Rc::new(frames.clone()),
                    });
                }
                conds.push((&r.guard, Rc::new(frames.clone())));
                let own = scope_tokens(rule_own_tokens(&r.selector, src, &Frames(frames)));
                link.extend(std::iter::repeat_n(true, own.len()));
                path.extend(own);
                collect_mixins(&r.block.statements, src, frames, path, link, conds, out);
                conds.pop();
            }
            Statement::AtRule(a) => {
                if let Some(block) = &a.block {
                    collect_mixins(&block.statements, src, frames, path, link, conds, out);
                }
            }
            _ => {}
        }
        path.truncate(saved);
        link.truncate(saved_link);
    }
    frames.pop();
}

/// mixin 名原文（保留 `@{...}`，匹配前再插值）。
pub(super) fn mixin_def_raw(name: &LessMixinName, src: &str) -> Option<String> {
    let span = match name {
        LessMixinName::ClassSelector(s) => s.span,
        LessMixinName::IdSelector(s) => s.span,
    };
    src.get(span.start..span.end)
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
}

/// 调用 token 链原文：`.a > .b()` → `[".a", ".b"]`（组合子与 less 一致忽略）。
pub(super) fn callee_raw_tokens(call: &LessMixinCall, src: &str) -> Vec<String> {
    call.callee
        .children
        .iter()
        .filter_map(|c| mixin_def_raw(&c.name, src))
        .collect()
}

pub(super) fn params_of<'a>(def: &MixinDef<'a>) -> &'a [LessMixinParameter<'a>] {
    def.params.map(|p| &p.params[..]).unwrap_or(&[])
}

pub(super) fn param_name<'a>(param: &'a LessMixinParameter<'a>) -> Option<&'a str> {
    match param {
        LessMixinParameter::Named(n) => parameter_name_ref(&n.name),
        LessMixinParameter::Variadic(v) => v.name.as_ref().and_then(parameter_name_ref),
        LessMixinParameter::Unnamed(_) => None,
    }
}

pub(super) fn parameter_name_ref<'a>(name: &'a LessMixinParameterName<'a>) -> Option<&'a str> {
    match name {
        LessMixinParameterName::Variable(v) => Some(v.name.name),
        LessMixinParameterName::PropertyVariable(p) => Some(p.name.name),
    }
}

pub(super) fn parameter_name(name: &LessMixinParameterName) -> Option<String> {
    parameter_name_ref(name).map(str::to_string)
}

/// 链路掩码：外层前缀各自的进入性 + 本层 `linkable`（补足到 `len`）。
pub(super) fn chain_mask(prefix: &[bool], linkable: bool, len: usize) -> Vec<bool> {
    let mut mask: Vec<bool> = prefix.to_vec();
    mask.resize(len, linkable);
    mask
}

/// 定义侧 token 链插值：`&` 是作用域占位 token，不再拆分。
pub(super) fn interp_tokens<'a>(
    raw: &[String],
    src: &'a str,
    scope: &dyn value::Scope<'a>,
) -> Vec<String> {
    raw.iter()
        .flat_map(|t| match t.as_str() {
            "&" => vec!["&".to_string()],
            _ => selector_tokens(&value::interpolate(t, src, scope, 0, None)),
        })
        .collect()
}

/// 实参里「math 关闭时保留的除法」（不在括号内）：这类值必须绑成 AST，
/// 否则文本化后 `(@b)` 与 `@b` 两种用法无法区分。
pub(super) fn open_division<'a>(cv: &'a ComponentValue<'a>) -> Option<&'a ComponentValue<'a>> {
    match cv {
        ComponentValue::LessBinaryOperation(op) => {
            if matches!(op.op.kind, LessOperationOperatorKind::Division) {
                return Some(cv);
            }
            open_division(&op.left).or_else(|| open_division(&op.right))
        }
        ComponentValue::LessNegativeValue(n) => open_division(&n.value),
        _ => None,
    }
}

/// guard 不含 `default()`，两趟都命中。
pub(super) const GROUP_NONE: u8 = 0;
/// 只有 `default() == true` 时命中。
const GROUP_TRUE: u8 = 1;
/// 只有 `default() == false` 时命中。
const GROUP_FALSE: u8 = 2;

/// 展开产物汇（声明 / 子规则 / extend）。
pub(super) struct Out<'a, 'c> {
    pub(super) pieces: &'c mut Vec<Piece<'a>>,
    pub(super) children: &'c mut Vec<ChildItem<'a>>,
    pub(super) needs: &'c mut Vec<Need>,
}

/// 一次 mixin 体展开的上下文。
pub(super) struct BodyCtx<'a> {
    /// 调用语句起点（注释归属用）
    at: usize,
    inject: Inject<'a>,
    important: bool,
    depth: usize,
    /// 调用处的作用域长度（动态登记子规则用）
    base: usize,
}

impl<'a> Compiler<'a> {
    /// 把本块登记进 extend 求值器（返回规则索引）。
    pub(super) fn register_rule(&mut self, head: &Head<'_>, needs: &[Need]) -> Option<usize> {
        if head.sels.is_empty() {
            return None;
        }
        let rule = self.extends.add_rule(
            head.sels.to_vec(),
            head.spans.to_vec(),
            self.scope,
            self.hidden,
        );
        if !needs.is_empty() {
            self.extends.mark_skip_all(rule);
        }
        for (path, sel) in head.sels.iter().enumerate() {
            let own = head.extends.get(path).map(Vec::as_slice).unwrap_or(&[]);
            if !own.is_empty() {
                self.extends.mark_skip(rule, path);
            }
            let span = head
                .spans
                .get(path)
                .copied()
                .unwrap_or(Span { start: 0, end: 0 });
            for (i, need) in own.iter().chain(needs.iter()).enumerate() {
                self.extends
                    .add_need(need, sel.clone(), span, rule, i == 0, !self.hidden);
            }
        }
        Some(rule)
    }

    /// less `each(list, ruleset)`（`functions/list.js`）：逐项展开规则集。
    pub(super) fn expand_each(&mut self, f: &'a Function<'a>, sink: &mut Sink<'a, '_>) {
        let groups = value::arg_groups(&f.args);
        let (Some(list), Some(body)) = (groups.first(), groups.get(1)) else {
            return;
        };
        let (names, block) = match body.first() {
            Some(ComponentValue::LessAnonymousMixin(m)) => (
                m.params.params.iter().filter_map(param_name).collect(),
                &m.block,
            ),
            Some(ComponentValue::LessDetachedRuleset(d)) => (Vec::new(), &d.block),
            _ => return,
        };
        let items = self.each_group_items(list);
        let bind = |i: usize, default: &str| -> String {
            names
                .get(i)
                .filter(|n| !n.is_empty())
                .map(|n| (*n).to_string())
                .unwrap_or_else(|| default.to_string())
        };
        let (value_name, key_name, index_name) =
            (bind(0, "value"), bind(1, "key"), bind(2, "index"));
        // 绑定既要进当前作用域（嵌套 each / 属性名），也要跟着子项延后到输出阶段
        let outer = sink.inject.clone();
        let base: Vec<Vars<'a>> = outer.as_ref().map(|f| (**f).clone()).unwrap_or_default();
        for (i, (key, item)) in items.into_iter().enumerate() {
            let key = key.unwrap_or_else(|| (i + 1).to_string());
            let index = (i + 1).to_string();
            self.env.push();
            self.env.set_text(value_name.clone(), item.clone());
            self.env.set_text(index_name.clone(), index.clone());
            self.env.set_text(key_name.clone(), key.clone());
            let mut bindings = Vars::new();
            bindings.insert(value_name.clone(), VarRef::Text(item));
            bindings.insert(index_name.clone(), VarRef::Text(index));
            bindings.insert(key_name.clone(), VarRef::Text(key));
            let mut frames = base.clone();
            frames.push(bindings);
            sink.inject = Some(Rc::new(frames));
            self.collect_block(&block.statements, block.span, false, sink);
            sink.inject = outer.clone();
            self.env.pop();
            // 体里的变量声明外泄到调用块（less 在当前帧里求值 each 体）
            for st in &block.statements {
                if let Statement::LessVariableDeclaration(d) = st {
                    self.note_var(d);
                }
            }
        }
    }

    /// `@dr();`：把 detached ruleset 的体在当前作用域展开。
    ///
    /// 作用域顺序同 less：调用方帧在下、规则集捕获帧与体变量帧在上，所以
    /// 调用方变量先解析、规则集内部的变量遮蔽外层；体变量不外泄到调用块。
    pub(super) fn expand_ruleset_call(
        &mut self,
        call: &'a LessVariableCall<'a>,
        at: usize,
        out: &mut Out<'a, '_>,
    ) {
        let name = call.variable.name.name;
        let Some(rs) = self.var_ruleset(name, 0) else {
            // less：变量不存在是 NameError，存在但不是 detached ruleset 是「求值失败」
            let message = match self.env.get(name) {
                None => format!("variable @{name} is undefined"),
                Some(_) => format!("Could not evaluate variable call @{name}"),
            };
            self.env.miss(message, call.span);
            return;
        };
        let saved_path = self.path.len();
        for part in &rs.parts {
            let inject = self.part_scope(part);
            self.env.push_scope(&Some(inject.clone()));
            self.expand_body(
                part.block,
                &BodyCtx {
                    at,
                    inject: inject.clone(),
                    important: self.force_important,
                    depth: 0,
                    base: saved_path,
                },
                out,
            );
            self.env.pop_scope(&Some(inject));
        }
    }

    /// `each` 的迭代项：规则集按声明拆成（属性名, 值）；找不到时退回空列表。
    pub(super) fn each_items(
        &self,
        node: Option<&'a ComponentValue<'a>>,
    ) -> Vec<(Option<String>, String)> {
        let Some(node) = node else {
            return Vec::new();
        };
        match node {
            ComponentValue::LessVariable(v) => self.each_items_var(v.name.name, *node.span()),
            ComponentValue::LessVariableCall(c) => {
                self.each_items_var(c.variable.name.name, *node.span())
            }
            ComponentValue::LessDetachedRuleset(d) => self.each_items_block(&d.block),
            // `each(.mixin(), …)` / `each(@p[key], …)`：规则集递归
            ComponentValue::LessMixinCall(_) | ComponentValue::LessNamespaceValue(_) => {
                match self.ruleset_of(node, 0) {
                    Some(rs) => self.each_items_ruleset(&rs),
                    None => Vec::new(),
                }
            } // `e("...")` 是单值；`range()` 返回的是列表（less 的 Expression）
            ComponentValue::Function(f) => {
                let name = value::function_name(f);
                if name.eq_ignore_ascii_case("range") {
                    value::split_items(&self.eval_text(node))
                        .into_iter()
                        .map(|t| (None, t))
                        .collect()
                } else {
                    vec![(None, self.eval_text(node))]
                }
            }
            ComponentValue::LessList(l) => l
                .elements
                .iter()
                .map(|e| (None, self.eval_text(e)))
                .collect(),
            _ => vec![(None, self.eval_text(node))],
        }
    }

    /// 实参组的迭代项：逗号把组分成多项（项内是空格列表），否则组内每个值是一项。
    pub(super) fn each_group_items(
        &self,
        group: &[&'a ComponentValue<'a>],
    ) -> Vec<(Option<String>, String)> {
        if group.len() == 1 {
            return self.each_items(group.first().copied());
        }
        let commas = group.iter().any(|n| {
            matches!(
                n,
                ComponentValue::Delimiter(d) if matches!(d.kind, DelimiterKind::Comma)
            )
        });
        if !commas {
            return group.iter().map(|n| (None, self.eval_text(n))).collect();
        }
        // 逗号分隔：每个子组算一项（子组内的元素空格拼接）
        let mut out = Vec::new();
        let mut cur: Vec<String> = Vec::new();
        for n in group {
            let comma = matches!(
                n,
                ComponentValue::Delimiter(d) if matches!(d.kind, DelimiterKind::Comma)
            );
            if comma {
                if !cur.is_empty() {
                    out.push((None, cur.join(" ")));
                    cur.clear();
                }
            } else {
                cur.push(self.eval_text(n));
            }
        }
        if !cur.is_empty() {
            out.push((None, cur.join(" ")));
        }
        out
    }

    pub(super) fn each_items_var(&self, name: &str, span: Span) -> Vec<(Option<String>, String)> {
        match self.env.get(name) {
            Some(VarRef::Ast(node)) => self.each_items(Some(node)),
            Some(VarRef::List(items)) => items.into_iter().map(|t| (None, t)).collect(),
            Some(VarRef::Text(text)) => value::split_items(&text)
                .into_iter()
                .map(|t| (None, t))
                .collect(),
            Some(VarRef::Color(c)) => vec![(None, c.to_css())],
            Some(VarRef::Ruleset(rs)) => self.each_items_ruleset(&rs),
            None => {
                self.env.report_var_miss(name, span);
                Vec::new()
            }
        }
    }

    /// 规则集的迭代项：每条声明的（属性名, 值），在规则集作用域里求值。
    pub(super) fn each_items_ruleset(&self, rs: &RuleSet<'a>) -> Vec<(Option<String>, String)> {
        self.flat_rules(rs, 0)
            .iter()
            .filter_map(|rule| {
                let decl = rule.decl?;
                let env = self.rule_env(rule);
                Some((
                    Some(self.prop_name(decl)),
                    value::eval_values(&decl.value, self.src, &env, 0),
                ))
            })
            .collect()
    }

    pub(super) fn each_items_block(
        &self,
        block: &'a SimpleBlock<'a>,
    ) -> Vec<(Option<String>, String)> {
        // 体里的 `$prop` 引同块声明（less 把规则集自身当成一个帧）
        let env = self.env.with_props(self.block_props(block));
        block
            .statements
            .iter()
            .filter_map(|st| match st {
                Statement::Declaration(d) => Some((
                    Some(self.prop_name(d)),
                    value::eval_values(&d.value, self.src, &env, 0),
                )),
                _ => None,
            })
            .collect()
    }

    /// 把 `&` 规则的内容原地悬入父规则的作用域（变量不外泄）。
    pub(super) fn inline_ruleset(&mut self, block: &'a SimpleBlock<'a>, out: &mut Out<'a, '_>) {
        self.env.push();
        for st in &block.statements {
            if let Statement::LessVariableDeclaration(d) = st {
                self.note_var(d);
            }
        }
        let mut sink = Sink {
            pieces: out.pieces,
            children: out.children,
            inject: Some(Rc::new(self.env.snapshot())),
            needs: out.needs,
        };
        self.collect_block(&block.statements, block.span, true, &mut sink);
        self.env.pop();
    }

    /// 规则上的 guard（`when (...)`）是否通过。
    pub(super) fn rule_guard_passes(&self, guard: &LessConditions<'a>) -> bool {
        let params = HashMap::new();
        let ctx = Ctx {
            src: self.src,
            scope: &self.env,
            params: &params,
            default: true,
        };
        guard::eval_conditions(guard, &ctx)
    }

    /// 定义侧 guard（含命名空间路径上的 guard）：用作用域副本求值，不改进程内的环境栈。
    pub(super) fn guards_pass(&self, def: &MixinDef<'a>, args: &[CallArg<'a>]) -> bool {
        let (frame, _arguments, vals) = self.bind_call_args(def, args);
        let mut frames = (*def.closure).clone();
        frames.push(frame);
        let env = self.env.scoped(&Rc::new(frames));
        let ctx = Ctx {
            src: self.src,
            scope: &env,
            params: &vals,
            default: true,
        };
        let own = def.guard.is_none_or(|g| guard::eval_conditions(g, &ctx));
        own && def.conds.iter().all(|(g, frames)| {
            let env = self.env.scoped(frames);
            guard::eval_conditions(
                g,
                &Ctx {
                    src: self.src,
                    scope: &env,
                    params: &HashMap::new(),
                    default: true,
                },
            )
        })
    }

    /// 展开 mixin 调用：把目标体（声明 + 子规则）复制到当前位置。
    ///
    /// 参数值在调用处求值（less 的求值时机），mixin 体以闭包 + 参数帧 + 体变量帧
    /// 注入，体里的声明仍保留 AST（懒求值，和普通声明一致）。
    pub(super) fn expand_mixin(
        &mut self,
        call: &'a LessMixinCall<'a>,
        at: usize,
        inherited_important: bool,
        out: &mut Out<'a, '_>,
        depth: usize,
    ) {
        if depth > 100 {
            return;
        }
        let raw = callee_raw_tokens(call, self.src);
        let tokens: Vec<String> = raw
            .iter()
            .flat_map(|t| selector_tokens(&self.interpolate(t)))
            .collect();
        if tokens.is_empty() {
            return;
        }
        let args = self.eval_args(call);
        let signature = args
            .iter()
            .map(|a| a.text.clone())
            .collect::<Vec<_>>()
            .join(",");
        // `!important` 会传递到嵌套调用的全部产物（less 的 makeImportant 递归）
        let important = inherited_important || call.important.is_some();
        // 最近帧优先：less 从最内层帧向外找，某帧一旦有命中就不再向外看
        let mut frames: Vec<(usize, Vec<Candidate<'a>>)> = Vec::new();
        // 实参匹配上了但 guard 全部未命中时 less 静默，不报错
        let mut matched_args = false;
        for (def, split) in self.find_mixins(&tokens) {
            if !self.match_args(&def, &args) {
                continue;
            }
            matched_args = true;
            let Some((frame, arguments, params)) = self.bind_params(&def, &args) else {
                continue;
            };
            // less 对 default() 求值两趟（false / true），两侧结果决定候选分组
            let pair = (
                self.guard_passes(&def, &params, false),
                self.guard_passes(&def, &params, true),
            );
            let group = match pair {
                (false, false) => continue,
                (true, true) => GROUP_NONE,
                (true, false) => GROUP_FALSE,
                (false, true) => GROUP_TRUE,
            };
            let cand = Candidate {
                def,
                group,
                frame,
                arguments,
            };
            match frames.iter_mut().find(|(d, _)| *d == split) {
                Some((_, list)) => list.push(cand),
                None => frames.push((split, vec![cand])),
            }
        }
        frames.sort_by_key(|(split, _)| std::cmp::Reverse(*split));
        let cands = match frames.into_iter().next() {
            Some((_, cands)) => cands,
            None => {
                if !matched_args {
                    self.miss_mixin(call, &tokens, &args);
                }
                return;
            }
        };
        // 有非 default() 且命中的候选 ⇒ default() 候选不展开（less 的互斥规则）
        let default_result = if cands.iter().any(|c| c.group == GROUP_NONE) {
            GROUP_FALSE
        } else {
            GROUP_TRUE
        };
        for cand in cands {
            if cand.group != GROUP_NONE && cand.group != default_result {
                continue;
            }
            // 可终止的递归：同一体的相同实参签名在活跃集里才算递归
            let key = (cand.def.block as *const SimpleBlock<'a>, signature.clone());
            if self.active.contains(&key) {
                continue;
            }
            let inject = self.make_inject(&cand.def, cand.frame, cand.arguments);
            self.active.push(key);
            self.env.push_scope(&Some(inject.clone()));
            let saved_path = self.path.len();
            let split = cand.def.tokens.len() - cand.def.own.len();
            self.path.extend(cand.def.tokens[split..].iter().cloned());
            self.path_linkable
                .extend(cand.def.chain[split..].iter().copied());
            self.expand_body(
                cand.def.block,
                &BodyCtx {
                    at,
                    inject: inject.clone(),
                    important,
                    depth,
                    base: saved_path,
                },
                out,
            );
            self.path.truncate(saved_path);
            self.path_linkable.truncate(saved_path);
            // mixin 体的变量声明会外泄到调用块（less 把变量规则一并复制）
            let leaked = self.leaked_vars(cand.def.block);
            self.env.pop_scope(&Some(inject));
            for (name, text) in leaked {
                self.env.set_text(name, text);
            }
            self.active.pop();
        }
    }

    /// 未命中的 mixin 调用：less 在这里报 NameError / RuntimeError（以前静默忽略）。
    ///
    /// 同名定义存在（含递归自调用被过滤的情况）时是「没有匹配的定义」，
    /// 完全找不到名字时是「<selector> is undefined」。
    pub(super) fn miss_mixin(
        &self,
        call: &'a LessMixinCall<'a>,
        tokens: &[String],
        args: &[CallArg<'a>],
    ) {
        let callee = self.slice(call.callee.span).trim().to_string();
        let known = tokens.last().is_some_and(|name| {
            self.mixins
                .candidates(name)
                .iter()
                .any(|&i| self.mixins.list[i].tokens == tokens)
        });
        let message = if known {
            let args = args
                .iter()
                .map(|a| a.text.clone())
                .collect::<Vec<_>>()
                .join(", ");
            format!("No matching definition was found for `{callee}({args})`")
        } else {
            format!("{callee} is undefined")
        };
        self.env.miss(message, call.span);
    }

    /// 候选定义：绝对路径或任一嵌套作用域切片（less 的帧搜索）。
    ///
    /// 同一 (体, token 链) 重复登记时只取最后一条：动态注册的闭包随展开更新。
    pub(super) fn find_mixins(&self, tokens: &[String]) -> Vec<(MixinDef<'a>, usize)> {
        let Some(name) = tokens.last() else {
            return Vec::new();
        };
        let matched: Vec<usize> = self
            .mixins
            .candidates(name)
            .iter()
            .filter_map(|&i| {
                let def = &self.mixins.list[i];
                let split = def.tokens.len().checked_sub(tokens.len())?;
                if def.tokens[split..] != *tokens || def.chain.len() != def.tokens.len() {
                    return None;
                }
                // 链路元素（非末位）须能零实参进入（less `matchArgs(null)`）
                if !def.chain[split..def.tokens.len() - 1].iter().all(|x| *x) {
                    return None;
                }
                // 前缀必须落在当前作用域路径上
                (self.path.len() >= split
                    && (0..=self.path.len() - split)
                        .any(|lo| self.path[lo..lo + split] == def.tokens[..split]))
                .then_some(i)
            })
            .collect();
        // 同一体只保留最近的一份：静态定义与调用处动态登记的副本只算一次
        let mut seen: Vec<*const SimpleBlock<'a>> = Vec::new();
        matched
            .iter()
            .filter(|&&i| {
                let block = self.mixins.list[i].block as *const SimpleBlock<'a>;
                // less 的 `isRecursive`：正作为外层规则展开的同名规则不参与匹配
                if seen.contains(&block) || self.rule_bodies.contains(&block) {
                    return false;
                }
                seen.push(block);
                true
            })
            .map(|&i| {
                let def = &self.mixins.list[i];
                (def.clone(), def.tokens.len() - tokens.len())
            })
            .collect()
    }

    /// 定义侧 guard（含路径上的 guard）是否通过。
    pub(super) fn guard_passes(
        &mut self,
        def: &MixinDef<'a>,
        params: &HashMap<String, Val>,
        default: bool,
    ) -> bool {
        let frame: Vars<'a> = params
            .iter()
            .map(|(name, v)| (name.clone(), VarRef::Text(guard::to_css(v))))
            .collect();
        let mut frames = (*def.closure).clone();
        frames.push(frame);
        let inject = Some(Rc::new(frames));
        self.env.push_scope(&inject);
        let ok = match def.guard {
            Some(g) => {
                let ctx = Ctx {
                    src: self.src,
                    scope: &self.env,
                    params,
                    default,
                };
                guard::eval_conditions(g, &ctx)
            }
            None => true,
        };
        self.env.pop_scope(&inject);
        ok && self.path_guards(def, default)
    }

    /// 命名空间路径上的 guard（调用处的变量不可见）。
    pub(super) fn path_guards(&mut self, def: &MixinDef<'a>, default: bool) -> bool {
        let params = HashMap::new();
        for (g, frames) in def.conds.clone() {
            let inject = Some(frames);
            self.env.push_scope(&inject);
            let ctx = Ctx {
                src: self.src,
                scope: &self.env,
                params: &params,
                default,
            };
            let ok = guard::eval_conditions(g, &ctx);
            self.env.pop_scope(&inject);
            if !ok {
                return false;
            }
        }
        true
    }

    /// 调用参数求值（保持源序）。
    pub(super) fn eval_args(&self, call: &'a LessMixinCall<'a>) -> Vec<CallArg<'a>> {
        let Some(args) = &call.args else {
            return Vec::new();
        };
        let no_params = HashMap::new();
        let ctx = Ctx {
            src: self.src,
            scope: &self.env,
            params: &no_params,
            default: true,
        };
        let mut out = Vec::new();
        // less 解析器的 `expand` 标志不复位：一个 `...` 之后的实参即使没写 `...`
        // 也按列表逐项展开（`and again` 变成 `and`、`again` 两个实参）
        let mut sticky_expand = false;
        for arg in &args.args {
            match arg {
                LessMixinArgument::Named(n) => {
                    let text = self.eval_text(&n.value);
                    out.push(CallArg {
                        name: parameter_name(&n.name),
                        val: guard::val_of(&n.value, &ctx, 0),
                        items: vec![text.clone()],
                        color: value::color_of(&n.value, self.src, &self.env),
                        ruleset: self.arg_ruleset(&n.value),
                        division: open_division(&n.value),
                        text,
                    });
                }
                LessMixinArgument::Value(v) => {
                    let text = self.eval_text(v);
                    let ruleset = self.arg_ruleset(v);
                    let items = space_items(v, &text, |e| self.eval_text(e));
                    if sticky_expand && items.len() > 1 {
                        out.extend(items.iter().map(|t| CallArg {
                            name: None,
                            text: t.clone(),
                            items: vec![t.clone()],
                            val: guard::from_text(t),
                            color: value::parse_color(t),
                            ruleset: None,
                            division: None,
                        }));
                        continue;
                    }
                    // 空的变参传递（`@rest` 为空）不会作为实参存在（less #4352）；
                    // 规则集实参本身的文本为空，但必须保留
                    if !text.is_empty() || ruleset.is_some() {
                        out.push(CallArg {
                            name: None,
                            val: guard::val_of(v, &ctx, 0),
                            items,
                            color: value::color_of(v, self.src, &self.env),
                            ruleset,
                            division: open_division(v),
                            text,
                        });
                    }
                }
                // `@args...`：把列表展开成多个位置参
                LessMixinArgument::Variadic(v) => {
                    sticky_expand = true;
                    out.extend(self.eval_expand(&v.name).into_iter().map(|text| CallArg {
                        name: None,
                        val: guard::from_text(&text),
                        items: vec![text.clone()],
                        color: value::parse_color(&text),
                        ruleset: None,
                        division: None,
                        text,
                    }));
                }
            }
        }
        out
    }

    /// 实参是规则集（字面量 detached ruleset / 变量 / 无参 mixin 调用）时带出它的体。
    pub(super) fn arg_ruleset(&self, v: &'a ComponentValue<'a>) -> Option<Rc<RuleSet<'a>>> {
        match v {
            ComponentValue::LessDetachedRuleset(_)
            | ComponentValue::LessVariable(_)
            | ComponentValue::LessMixinCall(_)
            | ComponentValue::LessNamespaceValue(_) => self.ruleset_of(v, 0).map(Rc::new),
            _ => None,
        }
    }

    pub(super) fn eval_expand(&self, name: &LessMixinParameterName<'a>) -> Vec<String> {
        let Some(name) = parameter_name(name) else {
            return Vec::new();
        };
        match self.env.get(&name) {
            Some(VarRef::Ast(node)) => match node {
                // `@list...`：逗号或空格分隔的列表整体展开成多个位置实参
                // （less 的 Expression.value 是数组，逐项 push；`1px/2px` 这种
                // 带定界符的列表在 less 里是一个 Operation，只算一个）
                ComponentValue::LessList(list)
                    if list.elements.len() > 1
                        && !list
                            .elements
                            .iter()
                            .any(|e| matches!(e, ComponentValue::Delimiter(_))) =>
                {
                    list.elements.iter().map(|v| self.eval_text(v)).collect()
                }
                _ => vec![self.eval_text(node)],
            },
            Some(VarRef::List(v)) => v.clone(),
            Some(VarRef::Text(t)) => vec![t],
            Some(VarRef::Color(c)) => vec![c.to_css()],
            Some(VarRef::Ruleset(_)) => Vec::new(),
            None => Vec::new(),
        }
    }

    /// less `MixinDefinition.matchArgs`。
    pub(super) fn match_args(&self, def: &MixinDef<'a>, args: &[CallArg<'a>]) -> bool {
        let params = params_of(def);
        let optional: Vec<&str> = params
            .iter()
            .filter_map(|p| match p {
                LessMixinParameter::Named(n) if n.value.is_some() => param_name(p),
                _ => None,
            })
            .collect();
        let required = params
            .iter()
            .filter(|p| !matches!(p, LessMixinParameter::Named(n) if n.value.is_some()))
            .count();
        let required_args = args
            .iter()
            .filter(|a| match &a.name {
                Some(name) => !optional.contains(&name.as_str()),
                None => true,
            })
            .count();
        let variadic = params
            .iter()
            .any(|p| matches!(p, LessMixinParameter::Variadic(_)));
        if variadic {
            if required_args + 1 < required {
                return false;
            }
        } else if required_args < required || args.len() > params.len() {
            return false;
        }
        // 字面量形参（`.m(9)`）是模式约束：实参求值文本必须相等
        let n = required_args.min(args.len()).min(params.len());
        for i in 0..n {
            if let LessMixinParameter::Unnamed(u) = &params[i] {
                if args[i].text != self.eval_text(&u.value) {
                    return false;
                }
            }
        }
        true
    }

    /// 命名空间路径元素（`#ns(@a)`）在链接搜索里按零实参筛选（less `matchArgs(null)`）。
    pub(super) fn allows_empty_args(params: &[LessMixinParameter]) -> bool {
        let required = params
            .iter()
            .filter(|p| !matches!(p, LessMixinParameter::Named(n) if n.value.is_some()))
            .count();
        if params
            .iter()
            .any(|p| matches!(p, LessMixinParameter::Variadic(_)))
        {
            required <= 1
        } else {
            required == 0
        }
    }

    /// 形参绑定：命名参先就位，位置参再依次填空，剩下的用默认值；
    /// 返回（参数帧, @arguments, guard 用的参数值）。
    pub(super) fn bind_params(
        &mut self,
        def: &MixinDef<'a>,
        args: &[CallArg<'a>],
    ) -> Option<(Vars<'a>, Vec<String>, HashMap<String, Val>)> {
        let params = params_of(def);
        let mut bound: Vec<Option<String>> = vec![None; params.len()];
        let mut vals: HashMap<String, Val> = HashMap::new();
        let mut slots: Vec<Option<String>> = vec![None; params.len()];
        let mut positional: Vec<&CallArg> = Vec::new();
        // 规则集实参/默认值：形参直接绑成可调用的规则集（延迟到 `@rules()` 才展开）
        let mut rulesets: HashMap<String, Rc<RuleSet<'a>>> = HashMap::new();
        // 带未计算除法的实参：形参绑 AST（见 `open_division`）
        let mut divisions: HashMap<String, &'a ComponentValue<'a>> = HashMap::new();
        for arg in args {
            let Some(name) = &arg.name else {
                positional.push(arg);
                continue;
            };
            let j = (0..params.len())
                .find(|&j| bound[j].is_none() && param_name(&params[j]) == Some(name.as_str()))?;
            bound[j] = Some(arg.text.clone());
            vals.insert(name.clone(), arg.val.clone());
            if let Some(rs) = &arg.ruleset {
                rulesets.insert(name.clone(), rs.clone());
            }
            if let Some(node) = arg.division {
                divisions.insert(name.clone(), node);
            }
            slots[j] = Some(arg.text.clone());
        }

        let mut pi = 0;
        let mut lists: HashMap<String, Vec<String>> = HashMap::new();
        for i in 0..params.len() {
            if bound[i].is_some() {
                continue;
            }
            match &params[i] {
                // 模式形参只消耗一个实参（相等性已在 match_args 里校验）
                LessMixinParameter::Unnamed(_) => pi += 1,
                LessMixinParameter::Variadic(v) => {
                    let rest = positional.get(pi..).unwrap_or(&[]);
                    for (k, arg) in rest.iter().enumerate() {
                        if i + k >= slots.len() {
                            slots.resize(i + k + 1, None);
                        }
                        slots[i + k] = Some(arg.text.clone());
                    }
                    if let Some(name) = v.name.as_ref().and_then(parameter_name_ref) {
                        let items = rest_items(rest);
                        bound[i] = Some(
                            rest.iter()
                                .map(|a| a.text.clone())
                                .collect::<Vec<_>>()
                                .join(" "),
                        );
                        lists.insert(name.to_string(), items);
                        vals.insert(
                            name.to_string(),
                            Val::List(rest.iter().map(|a| a.val.clone()).collect(), false),
                        );
                    }
                    pi = positional.len();
                }
                LessMixinParameter::Named(p) => {
                    if let Some(arg) = positional.get(pi).copied() {
                        bound[i] = Some(arg.text.clone());
                        if let Some(name) = param_name(&params[i]) {
                            vals.insert(name.to_string(), arg.val.clone());
                            // 空格分隔的实参在形参里仍是列表（`length(@value)` 要看元素数）
                            if arg.items.len() > 1 {
                                lists.insert(name.to_string(), arg.items.clone());
                            }
                            if let Some(rs) = &arg.ruleset {
                                rulesets.insert(name.to_string(), rs.clone());
                            }
                            if let Some(node) = arg.division {
                                divisions.insert(name.to_string(), node);
                            }
                        }
                        slots[i] = Some(arg.text.clone());
                        pi += 1;
                    } else if let Some(default) = &p.value {
                        // detached ruleset 默认值（`.m(@b: {default: works;})`）：体绑定成规则集
                        if let ComponentValue::LessDetachedRuleset(dr) = &default.value
                            && let Some(name) = param_name(&params[i])
                        {
                            rulesets.insert(
                                name.to_string(),
                                Rc::new(RuleSet::single(&dr.block, def.closure.clone())),
                            );
                            bound[i] = Some(String::new());
                            slots[i] = Some(String::new());
                            continue;
                        }
                        let text = self.eval_default(&default.value, def, &bound);
                        if let Some(name) = param_name(&params[i]) {
                            vals.insert(name.to_string(), guard::from_text(&text));
                        }
                        bound[i] = Some(text.clone());
                        slots[i] = Some(text);
                    } else {
                        return None;
                    }
                }
            }
        }

        let mut frame = Vars::new();
        // 实参文本 → 颜色：形参绑定保留浮点通道（less 的形参就是 Color 对象）
        let colors: HashMap<&str, Color> = args
            .iter()
            .filter_map(|a| Some((a.text.as_str(), a.color?)))
            .collect();
        for (i, value) in bound.iter().enumerate() {
            if let (Some(name), Some(value)) = (param_name(&params[i]), value) {
                let var = if let Some(rs) = rulesets.get(name) {
                    // 规则集形参：`@rules()` / `@rules[key]` 查这个体
                    VarRef::Ruleset(rs.clone())
                } else if let Some(node) = divisions.get(name) {
                    VarRef::Ast(node)
                } else if let Some(items) = lists.get(name) {
                    // 变参形参是列表：`length(@args)` 要看实参个数
                    VarRef::List(expression_list(items.clone()))
                } else if let Some(c) = colors.get(value.as_str()) {
                    VarRef::Color(*c)
                } else {
                    VarRef::Text(value.clone())
                };
                frame.insert(name.to_string(), var);
            }
        }
        // less 的 @arguments 按形参序号排列（命名实参与默认值都占位）
        let arguments = match (slots.len(), args.len()) {
            (1, 1) => args[0].items.clone(),
            _ => slots.into_iter().flatten().collect(),
        };
        Some((frame, arguments, vals))
    }

    /// less `new Expression(items).eval()`：单元素时拍平成该元素本身的列表。
    /// 默认值在 mixin 环境里求值（可见闭包与已绑定的前序参数）。
    pub(super) fn eval_default(
        &mut self,
        value: &'a ComponentValue<'a>,
        def: &MixinDef<'a>,
        bound: &[Option<String>],
    ) -> String {
        let params = params_of(def);
        let mut frame = Vars::new();
        for (i, value) in bound.iter().enumerate() {
            if let (Some(name), Some(value)) = (params.get(i).and_then(param_name), value) {
                frame.insert(name.to_string(), VarRef::Text(value.clone()));
            }
        }
        let mut frames = (*def.closure).clone();
        frames.push(frame);
        let inject = Some(Rc::new(frames));
        self.env.push_scope(&inject);
        let text = self.eval_text(value);
        self.env.pop_scope(&inject);
        text
    }

    /// 展开体可见的作用域：定义处闭包 → 参数帧 → 体变量帧（体变量优先级最高，同 less）。
    pub(super) fn make_inject(
        &self,
        def: &MixinDef<'a>,
        mut frame: Vars<'a>,
        arguments: Vec<String>,
    ) -> Inject<'a> {
        frame.insert("arguments".to_string(), VarRef::List(arguments));
        let mut frames = (*def.closure).clone();
        frames.push(frame.clone());
        // 与形参同名的体变量不覆盖实参（less 的帧顺序）
        let mut body = body_vars(def.block);
        for name in frame.keys() {
            body.remove(name);
        }
        frames.push(body);
        Rc::new(frames)
    }

    /// 展开 mixin 体：声明进 pieces，子规则进 children（保留顺序）。
    pub(super) fn expand_body(
        &mut self,
        block: &'a SimpleBlock<'a>,
        ctx: &BodyCtx<'a>,
        out: &mut Out<'a, '_>,
    ) {
        let BodyCtx {
            at,
            inject,
            important,
            depth,
            base,
        } = ctx;
        let (at, important, depth, base) = (*at, *important, *depth, *base);
        // mixin/规则集体同样在展开时求值体变量的值（less `Ruleset.eval`）；
        // 形参只在 inject 帧里，求值要带着帧，否则参数引用会被误判成未定义。
        self.eval_body_vars(&block.statements, inject);
        let own = self.mixin_comments(block);
        let mut next = 0;
        for st in &block.statements {
            let start = stmt_start(st).unwrap_or(usize::MAX);
            while next < own.len() && own[next].span.start < start {
                out.pieces.push(Piece::Comment(own[next].clone()));
                next += 1;
            }
            match st {
                Statement::Declaration(d) => out.pieces.push(Piece::Decl(Emit {
                    decl: d,
                    inject: Some(inject.clone()),
                    important,
                })),
                Statement::LessMixinCall(c) => {
                    self.expand_mixin(c, at, important, out, depth + 1);
                }
                Statement::LessVariableCall(c) => self.expand_ruleset_call(c, at, out),
                // `each(list, ruleset)` 在 mixin 体里同样展开成声明与子规则
                Statement::LessFunctionCall(f)
                    if value::function_name(f).eq_ignore_ascii_case("each") =>
                {
                    let mut sink = Sink {
                        pieces: &mut *out.pieces,
                        children: &mut *out.children,
                        inject: Some(inject.clone()),
                        needs: &mut *out.needs,
                    };
                    self.expand_each(f, &mut sink);
                }
                Statement::QualifiedRule(rule) => {
                    // mixin 体里的规则被展开后，在调用处成为可再调用的 mixin（less 的帧可见性）
                    self.register_dynamic(&rule.selector, &rule.block, base);
                    if is_just_parent(&rule.selector, self.src) {
                        self.inline_ruleset(&rule.block, out);
                    } else {
                        out.children.push(ChildItem {
                            at,
                            child: Child::Rule(rule),
                            inject: Some(inject.clone()),
                            important,
                        });
                    }
                }
                Statement::LessConditionalQualifiedRule(rule) => {
                    self.register_dynamic(&rule.selector, &rule.block, base);
                    if !self.rule_guard_passes(&rule.guard) {
                        continue;
                    }
                    if is_just_parent(&rule.selector, self.src) {
                        self.inline_ruleset(&rule.block, out);
                    } else {
                        out.children.push(ChildItem {
                            at,
                            child: Child::CondRule(rule),
                            inject: Some(inject.clone()),
                            important,
                        });
                    }
                }
                Statement::AtRule(a) if is_reference(a) => {
                    if let Some(block) = &a.block {
                        out.children.push(ChildItem {
                            at,
                            child: Child::Ref(block),
                            inject: Some(inject.clone()),
                            important,
                        });
                    }
                }
                Statement::AtRule(a) => out.children.push(ChildItem {
                    at,
                    child: Child::AtRule(a),
                    inject: Some(inject.clone()),
                    important,
                }),
                // 体内定义随展开进入调用处帧（less 把定义规则一并复制出去）
                Statement::LessMixinDefinition(d) => self.register_dynamic_def(d, base),
                Statement::LessExtendRule(rule) => out.needs.extend(self.needs_of(&rule.extend)),
                _ => {}
            }
        }
        while next < own.len() {
            out.pieces.push(Piece::Comment(own[next].clone()));
            next += 1;
        }
    }

    /// 把展开中遇到的规则登记为当前作用域可再调用的 mixin。
    /// `base` 是调用处的作用域长度：复制到调用处的规则相对于调用处可见。
    ///
    /// 同时把该规则块里的定义一并登记（less 在求值时就地展开，后续语句能立即看到）。
    pub(super) fn register_dynamic(
        &mut self,
        sel: &SelectorList,
        block: &'a SimpleBlock<'a>,
        base: usize,
    ) {
        let lists = rule_selector_raw_lists(sel, self.src);
        let closure = Rc::new(self.env.snapshot());
        let parent_only = is_just_parent(sel, self.src);
        for own in lists {
            self.push_def(base, own.clone(), None, block, closure.clone(), None);
            if !parent_only {
                let mut frames = (*closure).clone();
                frames.push(body_vars(block));
                let raw = [&self.path[..base], &own[..]].concat();
                let mut path = interp_tokens(&raw, self.src, &Frames(&frames));
                let mut conds = Vec::new();
                let mut link = self.path_linkable[..base].to_vec();
                link.extend(std::iter::repeat_n(true, path.len() - base));
                collect_mixins(
                    &block.statements,
                    self.src,
                    &mut frames,
                    &mut path,
                    &mut link,
                    &mut conds,
                    &mut self.mixins,
                );
            }
        }
    }

    /// 展开中遇到的 mixin 定义：登记到调用处的帧（同名定义按最近帧优先）。
    pub(super) fn register_dynamic_def(&mut self, d: &'a LessMixinDefinition<'a>, base: usize) {
        let Some(own) = mixin_def_raw(&d.name, self.src) else {
            return;
        };
        let own = vec![own];
        self.push_def(
            base,
            own,
            Some(&d.params),
            &d.block,
            Rc::new(self.env.snapshot()),
            d.guard.as_ref(),
        );
    }

    /// 登记一条定义；`raw` 与 `own` 是插值前的 token（按 `closure` 求值）。
    ///
    /// `raw` 由当前路径的前 `prefix` 段加 `raw_own` 组成，`chain` 记下各层能否被
    /// 命名空间链路零实参进入（less `matchArgs(null)`）。
    pub(super) fn push_def(
        &mut self,
        prefix: usize,
        raw_own: Vec<String>,
        params: Option<&'a LessMixinParameters<'a>>,
        block: &'a SimpleBlock<'a>,
        closure: Inject<'a>,
        guard: Option<&'a LessConditions<'a>>,
    ) {
        let scope = Frames(&closure);
        let linkable =
            Compiler::allows_empty_args(params.map(|p| &p.params[..]).unwrap_or_default());
        let raw = [&self.path[..prefix], &raw_own[..]].concat();
        let tokens = interp_tokens(&raw, self.src, &scope);
        self.mixins.insert(MixinDef {
            chain: chain_mask(&self.path_linkable[..prefix], linkable, tokens.len()),
            own: interp_tokens(&raw_own, self.src, &scope),
            tokens,
            params,
            guard,
            conds: Vec::new(),
            block,
            closure,
        });
    }

    /// 体变量当前环境（带参数帧）求值，供外泄到调用块。
    pub(super) fn leaked_vars(&mut self, block: &'a SimpleBlock<'a>) -> Vec<(String, String)> {
        block
            .statements
            .iter()
            .filter_map(|st| match st {
                Statement::LessVariableDeclaration(d) => {
                    Some((d.name.name.name.to_string(), self.eval_text(&d.value)))
                }
                _ => None,
            })
            .collect()
    }

    /// less `_removeDuplicateRules`：同一规则块里 (属性名, 值) 完全相同的声明只保留最后一条。
    pub(super) fn dedupe_pieces(&mut self, pieces: Vec<Piece<'a>>) -> Vec<Piece<'a>> {
        let mut seen: HashMap<String, usize> = HashMap::new();
        let mut keep = vec![true; pieces.len()];
        for (i, piece) in pieces.iter().enumerate().rev() {
            let Piece::Decl(e) = piece else {
                continue;
            };
            if seen.insert(self.decl_key(e), i).is_some() {
                keep[i] = false;
            }
        }
        pieces
            .into_iter()
            .zip(keep)
            .filter_map(|(piece, keep)| keep.then_some(piece))
            .collect()
    }

    /// 声明的 CSS 文本（去重用，与 `emit_declaration` 同形）。
    pub(super) fn decl_key(&mut self, e: &Emit<'a>) -> String {
        self.env.push_scope(&e.inject);
        let name = self.prop_name(e.decl);
        let mut key = format!("{name}: ");
        let mut prev_end: Option<usize> = None;
        for v in &e.decl.value {
            let span = *v.span();
            if let Some(end) = prev_end {
                if end <= span.start {
                    let gap = self.slice(Span {
                        start: end,
                        end: span.start,
                    });
                    key.push_str(gap);
                }
            }
            let text = self.eval_text(v);
            let text = strip_embedded_important(&text).unwrap_or(text);
            key.push_str(&text);
            prev_end = Some(span.end);
        }
        if e.decl.important.is_some() || e.important || self.force_important {
            key.push_str(" !important");
        }
        self.env.pop_scope(&e.inject);
        key
    }

    /// 在给定 inject 帧下求值一批语句里的变量声明值（mixin 体：形参只存在于帧里）。
    pub(super) fn eval_body_vars(&mut self, stmts: &'a [Statement<'a>], inject: &Inject<'a>) {
        let scoped = self.env.scoped(inject);
        let saved = std::mem::replace(&mut self.env, scoped);
        for st in stmts {
            if let Statement::LessVariableDeclaration(d) = st {
                self.eval_var_value(d);
            }
        }
        self.env = saved;
    }

    /// less `Variable.eval`：变量声明的值在所在规则集求值时算一次（未被引用也一样）。
    /// detached ruleset 的值是体本身，内部引用等调用时才求值；命名空间查找只验算
    /// 不转文本（`@scheme: @cfg[@primary]` 的结果可能是规则集，文本化会误报）。
    pub(super) fn eval_var_value(&mut self, d: &'a LessVariableDeclaration<'a>) {
        match &d.value {
            ComponentValue::LessDetachedRuleset(_) => {}
            ComponentValue::LessNamespaceValue(ns) => {
                if let Err(message) = self.namespace_lookup_checked(ns, 0) {
                    self.env.miss(message, ns.span);
                }
            }
            value => {
                self.eval_text(value);
            }
        }
    }

    /// 登记变量声明：值 + 值末尾的块注释（引用处带出）；值里的注释随即标为已处理。
    pub(super) fn note_var(&mut self, d: &'a LessVariableDeclaration<'a>) {
        let suffix = self.trailing_comment(d.value.span().end);
        self.env
            .set_var(d.name.name.name.to_string(), &d.value, suffix);
        if let Some((start, end, _)) = self.region_from(d.colon_span.end) {
            self.mark_comments(Span { start, end });
        }
    }
}
