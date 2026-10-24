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

/// Elements of a space-separated value list within that argument (any other argument counts as one element).
pub(super) fn space_items<'a, F>(node: &ComponentValue<'a>, text: &str, eval: F) -> Vec<String>
where
    F: Fn(&ComponentValue<'a>) -> String,
{
    match node {
        // A list with a delimiter (`5px/10px`) is an Operation in less, not a space list
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

/// The list bound to a variadic parameter: a single argument contributes its elements, several arguments each count as one.
pub(super) fn rest_items(rest: &[&CallArg<'_>]) -> Vec<String> {
    match rest {
        [one] => one.items.clone(),
        many => many.iter().map(|a| a.text.clone()).collect(),
    }
}

/// less `new Expression(items).eval()`: a single-element list flattens into the elements of that element.
pub(super) fn expression_list(items: Vec<String>) -> Vec<String> {
    if items.len() != 1 {
        return items;
    }
    let parts = value::split_items(&items[0]);
    if parts.is_empty() { items } else { parts }
}

/// Pre-scans all mixin definitions (calls may reference forward) and captures the closure scope for each definition.
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
                    // The guard itself also constrains nested definitions (`#top when (...) { #deep > .m() }`)
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
                // A namespace with required parameters cannot be linked with zero arguments (less `matchArgs(null)`)
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

/// Raw mixin name (keeps `@{...}`, interpolated again before matching).
pub(super) fn mixin_def_raw(name: &LessMixinName, src: &str) -> Option<String> {
    let span = match name {
        LessMixinName::ClassSelector(s) => s.span,
        LessMixinName::IdSelector(s) => s.span,
    };
    src.get(span.start..span.end)
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
}

/// Raw token chain of a call: `.a > .b()` -> `[".a", ".b"]` (combinators are ignored, as in less).
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

/// Chain mask: each outer prefix's enterability plus this level's `linkable` (padded to `len`).
pub(super) fn chain_mask(prefix: &[bool], linkable: bool, len: usize) -> Vec<bool> {
    let mut mask: Vec<bool> = prefix.to_vec();
    mask.resize(len, linkable);
    mask
}

/// Interpolation of a definition-side token chain: `&` is a scope placeholder token and is not split further.
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

/// The "division kept when math is off" in an argument (not inside parentheses): such values must bind as an AST,
/// otherwise the two uses `(@b)` and `@b` cannot be distinguished after conversion to text.
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

/// The guard has no `default()`, so it matches on both passes.
pub(super) const GROUP_NONE: u8 = 0;
/// Matches only when `default() == true`.
const GROUP_TRUE: u8 = 1;
/// Matches only when `default() == false`.
const GROUP_FALSE: u8 = 2;

/// Sink for expansion output (declarations / child rules / extends).
pub(super) struct Out<'a, 'c> {
    pub(super) pieces: &'c mut Vec<Piece<'a>>,
    pub(super) children: &'c mut Vec<ChildItem<'a>>,
    pub(super) needs: &'c mut Vec<Need>,
}

/// Context of one mixin body expansion.
pub(super) struct BodyCtx<'a> {
    /// Start of the call statement (for comment attribution)
    at: usize,
    inject: Inject<'a>,
    important: bool,
    depth: usize,
    /// Scope length at the call site (for dynamic registration of child rules)
    base: usize,
}

impl<'a> Compiler<'a> {
    /// Registers this block with the extend evaluator (returns the rule index).
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

    /// less `each(list, ruleset)` (`functions/list.js`): expands the ruleset item by item.
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
        // The bindings must enter the current scope (nested each / property names) and also follow child items deferred to the output stage
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
            // Variable declarations in the body leak into the calling block (less evaluates the each body in the current frame)
            for st in &block.statements {
                if let Statement::LessVariableDeclaration(d) = st {
                    self.note_var(d);
                }
            }
        }
    }

    /// `@dr();`: expands the body of a detached ruleset in the current scope.
    ///
    /// Scope order matches less: the caller frame is at the bottom, the ruleset capture frame and body variable frame above it, so
    /// caller variables resolve first and variables inside the ruleset shadow the outer ones; body variables do not leak into the calling block.
    pub(super) fn expand_ruleset_call(
        &mut self,
        call: &'a LessVariableCall<'a>,
        at: usize,
        out: &mut Out<'a, '_>,
    ) {
        let name = call.variable.name.name;
        let Some(rs) = self.var_ruleset(name, 0) else {
            // less: a missing variable is a NameError, while an existing non-detached-ruleset is an "evaluation failure"
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

    /// Iteration items of `each`: a ruleset splits into (property name, value) per declaration; falls back to an empty list when not found.
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
            // `each(.mixin(), …)` / `each(@p[key], …)`: ruleset recursion
            ComponentValue::LessMixinCall(_) | ComponentValue::LessNamespaceValue(_) => {
                match self.ruleset_of(node, 0) {
                    Some(rs) => self.each_items_ruleset(&rs),
                    None => Vec::new(),
                }
            } // `e("...")` is a single value; `range()` returns a list (less's Expression)
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

    /// Iteration items of an argument group: commas split the group into several items (each a space list), otherwise every value in the group is one item.
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
        // Comma-separated: each subgroup is one item (its elements joined by spaces)
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

    /// Iteration items of a ruleset: (property name, value) per declaration, evaluated in the ruleset scope.
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
        // `$prop` in the body references declarations of the same block (less treats the ruleset itself as a frame)
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

    /// Inlines the contents of a `&` rule into the parent rule's scope in place (variables do not leak).
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

    /// Whether a rule's guard (`when (...)`) passes.
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

    /// Definition-side guard (including guards on the namespace path): evaluated on a copy of the scope, without modifying the in-process env stack.
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

    /// Expands a mixin call: copies the target body (declarations + child rules) to the current position.
    ///
    /// Argument values are evaluated at the call site (less's evaluation timing); the mixin body is injected
    /// with closure + parameter frame + body variable frame, and declarations in the body keep their AST (lazy evaluation, as with ordinary declarations).
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
        // `!important` propagates to all output of nested calls (less's recursive makeImportant)
        let important = inherited_important || call.important.is_some();
        // Nearest frame wins: less searches from the innermost frame outward and stops once a frame has a match
        let mut frames: Vec<(usize, Vec<Candidate<'a>>)> = Vec::new();
        // When the arguments match but no guard passes, less stays silent and reports nothing
        let mut matched_args = false;
        for (def, split) in self.find_mixins(&tokens) {
            if !self.match_args(&def, &args) {
                continue;
            }
            matched_args = true;
            let Some((frame, arguments, params)) = self.bind_params(&def, &args) else {
                continue;
            };
            // less evaluates default() in two passes (false / true); the two results decide the candidate group
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
        // A matching non-default() candidate => default() candidates are not expanded (less's exclusion rule)
        let default_result = if cands.iter().any(|c| c.group == GROUP_NONE) {
            GROUP_FALSE
        } else {
            GROUP_TRUE
        };
        for cand in cands {
            if cand.group != GROUP_NONE && cand.group != default_result {
                continue;
            }
            // Terminating recursion: only the same body with the same argument signature in the active set counts as recursion
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
            // Variable declarations in the mixin body leak into the calling block (less copies the variable rules too)
            let leaked = self.leaked_vars(cand.def.block);
            self.env.pop_scope(&Some(inject));
            for (name, text) in leaked {
                self.env.set_text(name, text);
            }
            self.active.pop();
        }
    }

    /// An unmatched mixin call: less raises NameError / RuntimeError here (previously ignored silently).
    ///
    /// When a definition with the same name exists (including the case where a recursive self-call was filtered out) the message is "no matching definition";
    /// when the name cannot be found at all it is "<selector> is undefined".
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

    /// Candidate definitions: an absolute path or any nested scope slice (less's frame search).
    ///
    /// When the same (body, token chain) is registered repeatedly only the last one is taken: dynamically registered closures are updated as expansion proceeds.
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
                // Chain elements (except the last) must be enterable with zero arguments (less `matchArgs(null)`)
                if !def.chain[split..def.tokens.len() - 1].iter().all(|x| *x) {
                    return None;
                }
                // The prefix must lie on the current scope path
                (self.path.len() >= split
                    && (0..=self.path.len() - split)
                        .any(|lo| self.path[lo..lo + split] == def.tokens[..split]))
                .then_some(i)
            })
            .collect();
        // Keep only the most recent copy of a body: a static definition and a dynamically registered copy at the call site count once
        let mut seen: Vec<*const SimpleBlock<'a>> = Vec::new();
        matched
            .iter()
            .filter(|&&i| {
                let block = self.mixins.list[i].block as *const SimpleBlock<'a>;
                // less's `isRecursive`: a same-named rule currently being expanded as an outer rule does not take part in matching
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

    /// Whether the definition-side guard (including guards on the path) passes.
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

    /// A guard on the namespace path (variables at the call site are not visible).
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

    /// Call argument evaluation (preserving source order).
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
        // The less parser's `expand` flag is not reset: arguments after a `...` are expanded item by item
        // even without `...` (`and again` becomes the two arguments `and` and `again`)
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
                    // An empty variadic pass (`@rest` empty) does not exist as an argument (less #4352);
                    // a ruleset argument has empty text itself but must be kept
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
                // `@args...`: expands a list into several positional arguments
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

    /// Brings out its body when the argument is a ruleset (literal detached ruleset / variable / argument-less mixin call).
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
                // `@list...`: a comma- or space-separated list expands as a whole into several positional arguments
                // (less's Expression.value is an array and is pushed item by item; a delimited list such as
                // `1px/2px` is an Operation in less and counts as one)
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

    /// less `MixinDefinition.matchArgs`.
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
        // A literal parameter (`.m(9)`) is a pattern constraint: the evaluated argument text must be equal
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

    /// Namespace path elements (`#ns(@a)`) are filtered by zero arguments in the link search (less `matchArgs(null)`).
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

    /// Parameter binding: named arguments are placed first, positional ones fill the rest in order, and any remainder uses defaults;
    /// returns (parameter frame, @arguments, parameter values for guards).
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
        // Ruleset arguments/defaults: the parameter binds directly to a callable ruleset (expansion is deferred until `@rules()`)
        let mut rulesets: HashMap<String, Rc<RuleSet<'a>>> = HashMap::new();
        // Arguments with an unevaluated division: the parameter binds the AST (see `open_division`)
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
                // A pattern parameter consumes exactly one argument (equality was already checked in match_args)
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
                            // A space-separated argument stays a list in the parameter (`length(@value)` must see the element count)
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
                        // Detached ruleset default (`.m(@b: {default: works;})`): the body binds as a ruleset
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
        // Argument text -> color: the parameter binding keeps the floating-point channel (in less the parameter is a Color object)
        let colors: HashMap<&str, Color> = args
            .iter()
            .filter_map(|a| Some((a.text.as_str(), a.color?)))
            .collect();
        for (i, value) in bound.iter().enumerate() {
            if let (Some(name), Some(value)) = (param_name(&params[i]), value) {
                let var = if let Some(rs) = rulesets.get(name) {
                    // Ruleset parameter: `@rules()` / `@rules[key]` look up this body
                    VarRef::Ruleset(rs.clone())
                } else if let Some(node) = divisions.get(name) {
                    VarRef::Ast(node)
                } else if let Some(items) = lists.get(name) {
                    // A variadic parameter is a list: `length(@args)` must see the argument count
                    VarRef::List(expression_list(items.clone()))
                } else if let Some(c) = colors.get(value.as_str()) {
                    VarRef::Color(*c)
                } else {
                    VarRef::Text(value.clone())
                };
                frame.insert(name.to_string(), var);
            }
        }
        // less's @arguments follow parameter order (named arguments and defaults both occupy a slot)
        let arguments = match (slots.len(), args.len()) {
            (1, 1) => args[0].items.clone(),
            _ => slots.into_iter().flatten().collect(),
        };
        Some((frame, arguments, vals))
    }

    /// less `new Expression(items).eval()`: a single-element list flattens into the elements of that element.
    /// Defaults are evaluated in the mixin environment (visible closure plus already bound preceding parameters).
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

    /// Scope visible to the expanded body: definition closure -> parameter frame -> body variable frame (body variables win, as in less).
    pub(super) fn make_inject(
        &self,
        def: &MixinDef<'a>,
        mut frame: Vars<'a>,
        arguments: Vec<String>,
    ) -> Inject<'a> {
        frame.insert("arguments".to_string(), VarRef::List(arguments));
        let mut frames = (*def.closure).clone();
        frames.push(frame.clone());
        // A body variable with the same name as a parameter does not override the argument (less's frame order)
        let mut body = body_vars(def.block);
        for name in frame.keys() {
            body.remove(name);
        }
        frames.push(body);
        Rc::new(frames)
    }

    /// Expands a mixin body: declarations go to pieces, child rules to children (order preserved).
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
        // mixin/ruleset bodies likewise evaluate their body variables during expansion (less `Ruleset.eval`);
        // parameters live only in the inject frame, so evaluation must carry it, otherwise parameter references are misjudged as undefined.
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
                // `each(list, ruleset)` also expands into declarations and child rules inside a mixin body
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
                    // Once expanded, a rule from the mixin body becomes a callable mixin at the call site (less's frame visibility)
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
                // In-body definitions enter the call-site frame with the expansion (less copies the definition rules out too)
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

    /// Registers a rule met during expansion as a mixin callable from the current scope.
    /// `base` is the scope length at the call site: rules copied there are visible relative to it.
    ///
    /// Definitions inside that rule block are registered as well (less expands them in place during evaluation, so later statements see them immediately).
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

    /// A mixin definition met during expansion: registered into the call-site frame (same-named definitions follow nearest-frame priority).
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

    /// Registers one definition; `raw` and `own` are pre-interpolation tokens (evaluated per `closure`).
    ///
    /// `raw` is the first `prefix` segments of the current path plus `raw_own`, and `chain` records whether each level
    /// can be entered with zero arguments as a namespace chain element (less `matchArgs(null)`).
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

    /// Evaluates body variables in the current environment (with the parameter frame) for leaking into the calling block.
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

    /// less `_removeDuplicateRules`: within one rule block only the last of declarations with identical (property name, value) is kept.
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

    /// CSS text of a declaration (for dedup, same shape as `emit_declaration`).
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

    /// Evaluates the variable declaration values of a batch of statements under the given inject frame (mixin bodies: parameters exist only in the frame).
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

    /// less `Variable.eval`: a variable declaration's value is computed once when the enclosing ruleset is evaluated (even when unreferenced).
    /// A detached ruleset's value is the body itself, and internal references are evaluated only on call; a namespace lookup is verified only
    /// without conversion to text (the result of `@scheme: @cfg[@primary]` may be a ruleset, and text conversion would report false errors).
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

    /// Registers a variable declaration: the value plus a block comment trailing it (brought out at references); comments inside the value are marked as handled right away.
    pub(super) fn note_var(&mut self, d: &'a LessVariableDeclaration<'a>) {
        let suffix = self.trailing_comment(d.value.span().end);
        self.env
            .set_var(d.name.name.name.to_string(), &d.value, suffix);
        if let Some((start, end, _)) = self.region_from(d.colon_span.end) {
            self.mark_comments(Span { start, end });
        }
    }
}
