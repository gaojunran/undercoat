//! mixin / function / control flow: definitions, argument binding,
//! `@include`+content, `@if`/`@each`/`@for`/`@while`.
//!
//! Split out of `scss/mod.rs` (pure code move).

use super::*;

impl<'a> Converter<'a> {
    /// `@mixin`: only registers the definition, emits no CSS.
    pub(super) fn define_mixin(
        &mut self,
        at: &'a oxc_css_parser::ast::AtRule<'a>,
    ) -> Result<(), Box<CompileError>> {
        let Some(AtRulePrelude::SassMixin(mixin)) = &at.prelude else {
            return Err(self.err(at.span.start, "SCSS parse failure: @mixin"));
        };
        // Container validation is already done at parse time by `validate_structure`;
        // this is a runtime fallback.
        if self.control_depth > 0 {
            let message = if self.in_mixin {
                "Mixins may not contain mixin declarations."
            } else {
                "Mixins may not be declared in control directives."
            };
            return Err(self.err(at.span.start, message));
        }
        let Some(block) = &at.block else {
            return Err(self.err(at.span.start, "SCSS parse failure: @mixin"));
        };
        let (params, rest) = self.params(mixin.parameters.as_ref())?;
        let scope = self.eval.active_scope();
        let def = MixinDef {
            params,
            rest,
            body: &block.statements,
            body_span: block.span,
            scope,
            depth: self.eval.frame_depth(),
            has_content: has_content(&block.statements),
        };
        let name = eval::normalize_name(mixin.name.name);
        if let Some(frame) = self.mixins[scope].last_mut() {
            frame.insert(name.clone(), def);
        }
        self.eval.define_mixin(&name);
        Ok(())
    }

    /// Parameter list (duplicate names report dart's own message).
    fn params(
        &self,
        params: Option<&'a SassParameters<'a>>,
    ) -> Result<(Vec<Param<'a>>, Option<&'a str>), Box<CompileError>> {
        let Some(params) = params else {
            return Ok((Vec::new(), None));
        };
        let mut out: Vec<Param<'a>> = Vec::new();
        for param in &params.params {
            let name = param.name.name.name;
            if out
                .iter()
                .any(|p| eval::normalize_name(p.name) == eval::normalize_name(name))
            {
                return Err(self.err(param.span.start, "Duplicate argument."));
            }
            out.push(Param {
                name,
                default: param.default_value.as_ref().map(|d| &d.value),
            });
        }
        Ok((
            out,
            params.arbitrary_param.as_ref().map(|r| r.name.name.name),
        ))
    }

    /// mixin lookup: with a namespace, look in the module's exported member table;
    /// otherwise search outwards from the current frame (nearest first), and finally
    /// the modules pulled in by `@use ... as *` (later imports first). Mixins of a
    /// star module are visible in this scope only and are not exported.
    fn find_mixin(&self, name: &str, namespace: Option<&str>) -> Option<MixinDef<'a>> {
        let name = eval::normalize_name(name);
        if let Some(namespace) = namespace {
            let scope = self.eval.namespace_scope(namespace)?;
            return self.export_mixin(scope, &name, 0);
        }
        let frames = &self.mixins[self.eval.active_scope()];
        if let Some(def) = frames.iter().rev().find_map(|f| f.get(&name)).cloned() {
            return Some(def);
        }
        self.eval
            .star_scopes()
            .find_map(|scope| self.export_mixin(scope, &name, 0))
    }

    /// Mixin names and defining scopes among a module's exported members (used by the
    /// `@forward` duplicate check).
    pub(super) fn export_mixin_origins(
        &self,
        scope: usize,
        depth: usize,
        out: &mut HashMap<String, (usize, String)>,
    ) {
        const MAX_FORWARD_DEPTH: usize = 32;
        if depth > MAX_FORWARD_DEPTH {
            return;
        }
        if let Some(frame) = self.mixins[scope].first() {
            for name in frame.keys() {
                if !eval::is_private(name) {
                    out.entry(name.clone()).or_insert((scope, name.clone()));
                }
            }
        }
        for edge in self.eval.forwards(scope) {
            let mut inner = HashMap::new();
            self.export_mixin_origins(edge.scope, depth + 1, &mut inner);
            for (name, origin) in inner {
                let outward = format!("{}{}", edge.prefix, name);
                if edge.allows(eval::MemberKind::Member, &outward) {
                    out.entry(outward).or_insert(origin);
                }
            }
        }
    }

    /// A mixin among a module's exported members (its own global frame + the `@forward`
    /// chain; prefix/filter rules are the same as for variables).
    fn export_mixin(&self, scope: usize, name: &str, depth: usize) -> Option<MixinDef<'a>> {
        const MAX_FORWARD_DEPTH: usize = 32;
        if depth > MAX_FORWARD_DEPTH {
            return None;
        }
        if !eval::is_private(name) {
            if let Some(def) = self.mixins[scope].first().and_then(|f| f.get(name)) {
                return Some(def.clone());
            }
        }
        self.eval.forwards(scope).iter().rev().find_map(|edge| {
            if !edge.allows(eval::MemberKind::Member, name) {
                return None;
            }
            let inner = edge.strip(name)?;
            self.export_mixin(edge.scope, inner, depth + 1)
        })
    }

    /// Target name of `@include`: `m` or `ns.m`.
    fn mixin_name(
        &self,
        include: &SassInclude<'a>,
        at: usize,
    ) -> Result<(String, Option<String>), Box<CompileError>> {
        match &include.name {
            FunctionName::Ident(InterpolableIdent::Literal(ident)) => {
                Ok((ident.name.to_string(), None))
            }
            FunctionName::SassQualifiedName(q) => {
                let member = match &q.member {
                    SassModuleMemberName::Ident(ident) => ident.name.to_string(),
                    _ => {
                        return Err(self.err(
                            q.span.start,
                            "SCSS feature not implemented yet (S2): interpolated mixin name",
                        ));
                    }
                };
                Ok((member, Some(q.module.name.to_string())))
            }
            _ => Err(self.err(
                at,
                "SCSS feature not implemented yet (S2): interpolated mixin name",
            )),
        }
    }

    pub(super) fn include_rule(
        &mut self,
        at: &'a oxc_css_parser::ast::AtRule<'a>,
        parent: Option<&SelectorSet>,
        parent_span: Span,
        decls: &mut Vec<DeclItem>,
        nested: &mut Vec<Item>,
    ) -> Result<(), Box<CompileError>> {
        let Some(AtRulePrelude::SassInclude(include)) = &at.prelude else {
            return Err(self.err(at.span.start, "SCSS parse failure: @include"));
        };
        let at_pos = at.span.start;
        if include.content_block_params.is_some() && at.block.is_none() {
            // dart requires a block after `using (...)` (otherwise it is a syntax error).
            let end = include
                .content_block_params
                .as_ref()
                .map_or(at.span.end, |u| u.params.span.end);
            return Err(self.err(end, "expected \"{\"."));
        }
        let (name, namespace) = self.mixin_name(include, at_pos)?;
        if let Some(namespace) = &namespace {
            if !self.eval.has_namespace(namespace) {
                return Err(self.err(
                    at_pos,
                    format!("There is no module with the namespace \"{namespace}\"."),
                ));
            }
        }
        let Some(def) = self.find_mixin(&name, namespace.as_deref()) else {
            return Err(self.err(at_pos, "Undefined mixin."));
        };
        if self.mixin_depth >= MAX_MIXIN_DEPTH {
            return Err(self.err(
                at_pos,
                format!("Mixin nesting depth exceeded ({MAX_MIXIN_DEPTH})."),
            ));
        }
        let caller_scope = self.eval.active_scope();
        let outer = self.content.last().copied().flatten();
        let content_block = match (&at.block, &include.content_block_params) {
            (Some(block), using) => {
                if !def.has_content {
                    return Err(self.err(at_pos, "Mixin doesn't accept a content block."));
                }
                let (params, rest) = self.params(using.as_ref().map(|u| &u.params))?;
                Some(ContentBlock {
                    params,
                    rest,
                    stmts: &block.statements,
                    span: block.span,
                    scope: caller_scope,
                    head_len: if def.scope == caller_scope {
                        def.depth
                    } else {
                        self.eval.frame_depth()
                    },
                    tail: Vec::new(),
                    mixins_tail: Vec::new(),
                    cursor: self.cursor,
                    outer,
                })
            }
            (None, _) => None,
        };
        let inv = match &include.arguments {
            Some(args) => self
                .eval
                .eval_invocation(self.src(), &args.args)
                .map_err(|e| self.eval_err(e))?,
            None => Invocation::default(),
        };

        let saved_cursor = self.cursor;
        self.eval.enter(def.scope);
        // Same scope: truncate the frame stack to the definition depth; the extra part
        // is kept by the content block.
        let (mut frames_tail, mut mixins_tail) = if def.scope == caller_scope {
            self.detach_frames(def.scope, def.depth)
        } else {
            (Vec::new(), Vec::new())
        };
        self.eval.push_frame(false);
        self.mixins[def.scope].push(HashMap::new());

        let mut pushed = false;
        let mut rest_check = None;
        let result = match self.bind(&def.params, def.rest, inv, at_pos) {
            Err(e) => Err(e),
            Ok(check) => {
                rest_check = check;
                // Comments of the mixin body use the window at the definition site; the
                // cursor of the call site is restored afterwards.
                self.cursor = self.comment_cursor(def.body_span.start);
                let slot = content_block.map(|mut block| {
                    block.tail = std::mem::take(&mut frames_tail);
                    block.mixins_tail = std::mem::take(&mut mixins_tail);
                    self.content_blocks.push(block);
                    self.content_blocks.len() - 1
                });
                self.content.push(slot);
                pushed = true;
                self.mixin_depth += 1;
                let was_in_mixin = self.in_mixin;
                self.in_mixin = true;
                self.eval
                    .set_mixin_context(true, self.content.last().is_some_and(Option::is_some));
                let r = self.stmts(
                    def.body,
                    def.body_span.end,
                    parent,
                    parent_span,
                    decls,
                    nested,
                );
                self.in_mixin = was_in_mixin;
                self.eval.set_mixin_context(
                    self.in_mixin,
                    self.content.last().is_some_and(Option::is_some),
                );
                self.mixin_depth -= 1;
                self.eval.enter(def.scope);
                r
            }
        };
        // Changes to the caller's local variables made inside the content block are
        // carried back to the caller's environment.
        if pushed {
            if let Some(Some(index)) = self.content.pop() {
                let block = &mut self.content_blocks[index];
                frames_tail = std::mem::take(&mut block.tail);
                mixins_tail = std::mem::take(&mut block.mixins_tail);
            }
        }
        self.cursor = saved_cursor;
        self.mixins[def.scope].pop();
        self.eval.pop_frame();
        self.reattach_frames(def.scope, frames_tail, mixins_tail);
        self.eval.enter(caller_scope);
        // The error for extra named arguments comes after the function body runs
        // (observed with dart: no error when the body has read keywords()).
        result.and_then(|()| match rest_check {
            Some(check) => self.check_unknown_keywords(check, at_pos),
            None => Ok(()),
        })
    }

    /// Truncate the frame stack to `depth` (the definition environment of a lexical
    /// closure) and return the detached tail for later restoration.
    fn detach_frames(
        &mut self,
        scope: usize,
        depth: usize,
    ) -> (Vec<eval::Frame<'a>>, Vec<HashMap<String, MixinDef<'a>>>) {
        let mut frames = self.eval.take_frames(scope);
        let tail = frames.split_off(depth.min(frames.len()));
        self.eval.put_frames(scope, frames);
        let mut mixins = std::mem::take(&mut self.mixins[scope]);
        let mixins_tail = mixins.split_off(depth.min(mixins.len()));
        self.mixins[scope] = mixins;
        (tail, mixins_tail)
    }

    fn reattach_frames(
        &mut self,
        scope: usize,
        tail: Vec<eval::Frame<'a>>,
        mixins_tail: Vec<HashMap<String, MixinDef<'a>>>,
    ) {
        let mut frames = self.eval.take_frames(scope);
        frames.extend(tail);
        self.eval.put_frames(scope, frames);
        let mut mixins = std::mem::take(&mut self.mixins[scope]);
        mixins.extend(mixins_tail);
        self.mixins[scope] = mixins;
    }

    /// Argument binding (dart `ArgumentDeclaration.verify` + binding order):
    /// positional arguments fill parameters in order -> keywords fill in by name ->
    /// defaults are evaluated in declaration order (they may reference earlier
    /// parameters); `$rest...` collects extra positional arguments (observed with
    /// dart: rest does not take keyword arguments).
    /// Post-binding check: with `$rest`, extra named arguments are first collected into
    /// an ArgList, and whether to report an error is decided after the mixin body (or
    /// content block) has run -- observed with dart: no error as long as someone has
    /// read the keyword map (`keywords($args)`).
    /// The error for extra named arguments comes after the function body runs (observed
    /// with dart: no error when the body has read keywords()).
    fn check_unknown_keywords(&self, check: RestCheck, at: usize) -> Result<(), Box<CompileError>> {
        self.eval
            .check_keywords(check, at)
            .map_err(|e| self.eval_err(e))
    }

    /// `@content`: runs in the scope of the caller (the one that defined the content
    /// block).
    pub(super) fn content_rule(
        &mut self,
        at: &'a oxc_css_parser::ast::AtRule<'a>,
        parent: Option<&SelectorSet>,
        parent_span: Span,
        decls: &mut Vec<DeclItem>,
        nested: &mut Vec<Item>,
    ) -> Result<(), Box<CompileError>> {
        let args: &[ComponentValue<'a>] = match &at.prelude {
            Some(AtRulePrelude::SassContent(content)) => &content.args,
            _ => &[],
        };
        // Empty stack = not inside a mixin at all; `None` = the enclosing mixin call
        // got no content block (no output). Ordinary blocks (`@media` / `@if`) do not
        // change the stack, so bootstrap's `color-mode()` can pass content through.
        let Some(slot) = self.content.pop() else {
            return Err(self.err(
                at.span.start,
                "@content is only allowed within mixin declarations.",
            ));
        };
        let Some(index) = slot else {
            self.content.push(None);
            return Ok(());
        };
        // While executing, the "active" content block is swapped for the one captured
        // when the block was created (dart's `_contentBlock`).
        self.content.push(self.content_blocks[index].outer);
        let inv = self
            .eval
            .eval_invocation(self.src(), args)
            .map_err(|e| self.eval_err(e))?;

        let saved_active = self.eval.active_scope();
        let saved_cursor = self.cursor;
        let block = &mut self.content_blocks[index];
        let (params, rest, stmts, span, scope, head_len, cursor) = (
            std::mem::take(&mut block.params),
            block.rest,
            block.stmts,
            block.span,
            block.scope,
            block.head_len,
            block.cursor,
        );
        // Switch back to the caller's environment: detach the mixin's frames and
        // restore the segment held by the content block itself.
        self.eval.enter(scope);
        let (cur_tail, cur_mixins) = self.detach_frames(scope, head_len);
        let mut tail = std::mem::take(&mut self.content_blocks[index].tail);
        let mut mixins_tail = std::mem::take(&mut self.content_blocks[index].mixins_tail);
        self.reattach_frames(
            scope,
            std::mem::take(&mut tail),
            std::mem::take(&mut mixins_tail),
        );
        self.eval.push_frame(false);
        self.mixins[scope].push(HashMap::new());

        let mut rest_check = None;
        let result = match self.bind(&params, rest, inv, at.span.start) {
            Err(e) => Err(e),
            Ok(check) => {
                rest_check = check;
                self.cursor = cursor;
                let r = self.stmts(stmts, span.end, parent, parent_span, decls, nested);
                self.eval.enter(scope);
                r
            }
        };
        // Collect the changes of the caller's environment, then put the mixin's frame
        // stack back.
        self.eval.enter(scope);
        self.mixins[scope].pop();
        self.eval.pop_frame();
        let (new_tail, new_mixins) = self.detach_frames(scope, head_len);
        self.reattach_frames(scope, cur_tail, cur_mixins);
        let block = &mut self.content_blocks[index];
        block.params = params;
        block.tail = new_tail;
        block.mixins_tail = new_mixins;
        // Restore the stack: `@content` may appear multiple times in the same mixin
        // body.
        self.content.pop();
        self.content.push(Some(index));
        self.cursor = saved_cursor;
        self.eval.enter(saved_active);
        result.and_then(|()| match rest_check {
            Some(check) => self.check_unknown_keywords(check, at.span.start),
            None => Ok(()),
        })
    }

    /// `@if` / `@else if` / `@else`: only `false` and `null` are false. The body is a
    /// "semi-global" scope.
    pub(super) fn if_rule(
        &mut self,
        rule: &'a SassIfAtRule<'a>,
        parent: Option<&SelectorSet>,
        parent_span: Span,
        decls: &mut Vec<DeclItem>,
        nested: &mut Vec<Item>,
    ) -> Result<(), Box<CompileError>> {
        let mut chosen: Option<&'a SimpleBlock<'a>> = None;
        for clause in std::iter::once(&rule.if_clause).chain(rule.else_if_clauses.iter()) {
            let value = self
                .eval
                .eval_component(self.src(), &clause.condition)
                .map_err(|e| self.eval_err(e))?;
            if value.truthy() {
                chosen = Some(&clause.block);
                break;
            }
        }
        let block = match chosen.or(rule.else_clause.as_ref()) {
            Some(block) => block,
            None => return Ok(()),
        };
        self.control_depth += 1;
        let scope = self.push_scope(true);
        let result = self.stmts(
            &block.statements,
            block.span.end,
            parent,
            parent_span,
            decls,
            nested,
        );
        self.pop_scope(scope);
        self.control_depth -= 1;
        result
    }

    /// `@function`: only registers the definition, emits no CSS.
    pub(super) fn define_function(
        &mut self,
        at: &'a oxc_css_parser::ast::AtRule<'a>,
    ) -> Result<(), Box<CompileError>> {
        let (Some(AtRulePrelude::SassFunction(func)), Some(block)) = (&at.prelude, &at.block)
        else {
            return Err(self.err(at.span.start, "SCSS parse failure: @function"));
        };
        let (params, rest) = self.params(Some(&func.parameters))?;
        let def = eval::FunctionDef {
            params,
            rest,
            body: &block.statements,
            at: at.span.start,
            scope: self.eval.active_scope(),
            depth: self.eval.frame_depth(),
        };
        self.eval.define_function(func.name.name, def);
        Ok(())
    }

    /// One iteration of a loop body: replay the comment cursor (observed with dart: a
    /// `/* */` in a loop body is emitted once per iteration).
    fn loop_iteration(
        &mut self,
        block: &'a SimpleBlock<'a>,
        parent: Option<&SelectorSet>,
        parent_span: Span,
        decls: &mut Vec<DeclItem>,
        nested: &mut Vec<Item>,
        cursor: usize,
    ) -> Result<(), Box<CompileError>> {
        self.cursor = cursor;
        self.stmts(
            &block.statements,
            block.span.end,
            parent,
            parent_span,
            decls,
            nested,
        )
    }

    /// `@each $v in $list` / `@each $k, $v in $map`: one semi-global frame for the
    /// whole loop, with the bound values written into the current frame each
    /// iteration (loop variables are gone once the loop ends, observed with dart).
    pub(super) fn each_rule(
        &mut self,
        at: &'a oxc_css_parser::ast::AtRule<'a>,
        parent: Option<&SelectorSet>,
        parent_span: Span,
        decls: &mut Vec<DeclItem>,
        nested: &mut Vec<Item>,
    ) -> Result<(), Box<CompileError>> {
        let (Some(AtRulePrelude::SassEach(each)), Some(block)) = (&at.prelude, &at.block) else {
            return Err(self.err(at.span.start, "SCSS parse failure: @each"));
        };
        let src = self.src();
        let rows = self
            .eval
            .each_rows(src, &each.expr, each.bindings.len())
            .map_err(|e| self.eval_err(e))?;
        self.control_depth += 1;
        let scope = self.push_scope(true);
        let cursor = self.cursor;
        let mut result = Ok(());
        for row in rows {
            for (var, value) in each.bindings.iter().zip(row) {
                self.eval.declare_local(var.name.name, value);
            }
            if let Err(e) = self.loop_iteration(block, parent, parent_span, decls, nested, cursor) {
                result = Err(e);
                break;
            }
        }
        self.pop_scope(scope);
        self.control_depth -= 1;
        result
    }

    /// `@for $i from ... through|to ...`: when the lower bound exceeds the upper bound
    /// the loop runs **in reverse** (observed with dart; it is not an empty loop).
    pub(super) fn for_rule(
        &mut self,
        at: &'a oxc_css_parser::ast::AtRule<'a>,
        parent: Option<&SelectorSet>,
        parent_span: Span,
        decls: &mut Vec<DeclItem>,
        nested: &mut Vec<Item>,
    ) -> Result<(), Box<CompileError>> {
        let (Some(AtRulePrelude::SassFor(spec)), Some(block)) = (&at.prelude, &at.block) else {
            return Err(self.err(at.span.start, "SCSS parse failure: @for"));
        };
        let inclusive = matches!(spec.boundary.kind, SassForBoundaryKind::Inclusive);
        let src = self.src();
        let values = self
            .eval
            .for_values(src, &spec.start, &spec.end, inclusive)
            .map_err(|e| self.eval_err(e))?;
        self.control_depth += 1;
        let scope = self.push_scope(true);
        let cursor = self.cursor;
        let mut result = Ok(());
        for value in values {
            self.eval.declare_local(spec.binding.name.name, value);
            if let Err(e) = self.loop_iteration(block, parent, parent_span, decls, nested, cursor) {
                result = Err(e);
                break;
            }
        }
        self.pop_scope(scope);
        self.control_depth -= 1;
        result
    }

    /// `@while`: the condition is re-evaluated on every iteration.
    pub(super) fn while_rule(
        &mut self,
        at: &'a oxc_css_parser::ast::AtRule<'a>,
        parent: Option<&SelectorSet>,
        parent_span: Span,
        decls: &mut Vec<DeclItem>,
        nested: &mut Vec<Item>,
    ) -> Result<(), Box<CompileError>> {
        let (Some(AtRulePrelude::SassExpr(cond)), Some(block)) = (&at.prelude, &at.block) else {
            return Err(self.err(at.span.start, "SCSS parse failure: @while"));
        };
        self.control_depth += 1;
        let scope = self.push_scope(true);
        let cursor = self.cursor;
        let mut result = Ok(());
        loop {
            let cond = self.eval.eval_component(self.src(), cond);
            match cond {
                Err(e) => {
                    result = Err(self.eval_err(e));
                    break;
                }
                Ok(value) if !value.truthy() => break,
                Ok(_) => {}
            }
            if let Err(e) = self.loop_iteration(block, parent, parent_span, decls, nested, cursor) {
                result = Err(e);
                break;
            }
        }
        self.pop_scope(scope);
        self.control_depth -= 1;
        result
    }
}
