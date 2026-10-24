//! mixin / function / 控制流：定义、参数绑定、`@include`+content、`@if`/`@each`/`@for`/`@while`。
//!
//! 由 `scss/mod.rs` 拆分而来（纯搬运）。

use super::*;

impl<'a> Converter<'a> {
    /// `@mixin`：只登记定义，不产出 CSS。
    pub(super) fn define_mixin(
        &mut self,
        at: &'a oxc_css_parser::ast::AtRule<'a>,
    ) -> Result<(), Box<CompileError>> {
        let Some(AtRulePrelude::SassMixin(mixin)) = &at.prelude else {
            return Err(self.err(at.span.start, "SCSS 解析失败: @mixin"));
        };
        // 容器校验已由 `validate_structure` 在解析期完成；这里保留一道运行时兜底。
        if self.control_depth > 0 {
            let message = if self.in_mixin {
                "Mixins may not contain mixin declarations."
            } else {
                "Mixins may not be declared in control directives."
            };
            return Err(self.err(at.span.start, message));
        }
        let Some(block) = &at.block else {
            return Err(self.err(at.span.start, "SCSS 解析失败: @mixin"));
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

    /// 形参列表（重名报 dart 同款文案）。
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

    /// mixin 查找：命名空间取模块的对外成员表，否则从当前帧向外层找（最近的优先），
    /// 最后找 `@use ... as *` 引入的模块（后引入的优先）。星号模块的 mixin 只在
    /// 本作用域可见，不对外导出。
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

    /// 模块对外成员里的 mixin 名与定义处（`@forward` 重复检测用）。
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

    /// 模块对外成员里的 mixin（自己的全局帧 + `@forward` 链，前缀/过滤规则同变量）。
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

    /// `@include` 的目标名：`m` 或 `ns.m`。
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
                        return Err(
                            self.err(q.span.start, "SCSS 特性尚未实现（S2）: 插值 mixin 名")
                        );
                    }
                };
                Ok((member, Some(q.module.name.to_string())))
            }
            _ => Err(self.err(at, "SCSS 特性尚未实现（S2）: 插值 mixin 名")),
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
            return Err(self.err(at.span.start, "SCSS 解析失败: @include"));
        };
        let at_pos = at.span.start;
        if include.content_block_params.is_some() && at.block.is_none() {
            // dart 要求 `using (...)` 后面必须跟块（否则就是语法错误）。
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
        // 同 scope：把帧栈截到定义深度，多出来的部分交给 content 块保管。
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
                // mixin 体的注释用定义处的窗口，结束后恢复调用处的游标。
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
        // content 块里对调用方局部变量的修改带回调用方环境。
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
        // 多余命名实参的报错晚于函数体执行（dart 实测：体里读过 keywords() 就不报）。
        result.and_then(|()| match rest_check {
            Some(check) => self.check_unknown_keywords(check, at_pos),
            None => Ok(()),
        })
    }

    /// 把帧栈截到 `depth`（词法闭包的定义环境），返回被摘下的尾部，供还原。
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

    /// 实参绑定（dart `ArgumentDeclaration.verify` + 绑定顺序）：
    /// 位置实参按序填形参 → 关键字按名补位 → 默认值按声明顺序求值（可引用前面的形参）；
    /// `$rest...` 收集多余位置实参（dart 实测：rest 不吃关键字实参）。
    /// 绑定后的遗留检查：有 `$rest` 时，多余的命名实参先装进 ArgList，等 mixin 体（或 content
    /// 块）跑完再决定要不要报错——dart 实测：只要有人读过关键字 map（`keywords($args)`）就不报。
    /// 多余命名实参的报错晚于函数体执行（dart 实测：体里读过 keywords() 就不报）。
    fn check_unknown_keywords(&self, check: RestCheck, at: usize) -> Result<(), Box<CompileError>> {
        self.eval
            .check_keywords(check, at)
            .map_err(|e| self.eval_err(e))
    }

    /// `@content`：在调用方（定义 content 块的）作用域里执行。
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
        // 栈空 = 根本不在 mixin 里；`None` = 所在 mixin 调用没收到 content 块（无输出）。
        // 普通块（`@media` / `@if`）不改变栈，所以 bootstrap 的 `color-mode()` 能透传。
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
        // 执行期间「生效中」的 content 块换成块创建时捕获的那个（dart 的 `_contentBlock`）。
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
        // 换回调用方环境：摘掉 mixin 的帧，装回 content 块自己持有的那段。
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
        // 收回调用方环境的变动，再把 mixin 的帧栈装回去。
        self.eval.enter(scope);
        self.mixins[scope].pop();
        self.eval.pop_frame();
        let (new_tail, new_mixins) = self.detach_frames(scope, head_len);
        self.reattach_frames(scope, cur_tail, cur_mixins);
        let block = &mut self.content_blocks[index];
        block.params = params;
        block.tail = new_tail;
        block.mixins_tail = new_mixins;
        // 恢复栈：`@content` 可以在同一个 mixin 体里出现多次。
        self.content.pop();
        self.content.push(Some(index));
        self.cursor = saved_cursor;
        self.eval.enter(saved_active);
        result.and_then(|()| match rest_check {
            Some(check) => self.check_unknown_keywords(check, at.span.start),
            None => Ok(()),
        })
    }

    /// `@if` / `@else if` / `@else`：只有 `false` 与 `null` 为假。正文是「半全局」作用域。
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

    /// `@function`：只登记定义，不产出 CSS。
    pub(super) fn define_function(
        &mut self,
        at: &'a oxc_css_parser::ast::AtRule<'a>,
    ) -> Result<(), Box<CompileError>> {
        let (Some(AtRulePrelude::SassFunction(func)), Some(block)) = (&at.prelude, &at.block)
        else {
            return Err(self.err(at.span.start, "SCSS 解析失败: @function"));
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

    /// 循环体一轮：注释游标回放（dart 实测：循环体里的 `/* */` 每轮都输出一次）。
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

    /// `@each $v in $list` / `@each $k, $v in $map`：整个循环一个 semi-global 帧，
    /// 每轮把绑定值写进当前帧（循环变量出循环即失效，dart 实测）。
    pub(super) fn each_rule(
        &mut self,
        at: &'a oxc_css_parser::ast::AtRule<'a>,
        parent: Option<&SelectorSet>,
        parent_span: Span,
        decls: &mut Vec<DeclItem>,
        nested: &mut Vec<Item>,
    ) -> Result<(), Box<CompileError>> {
        let (Some(AtRulePrelude::SassEach(each)), Some(block)) = (&at.prelude, &at.block) else {
            return Err(self.err(at.span.start, "SCSS 解析失败: @each"));
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

    /// `@for $i from ... through|to ...`：下界大于上界时**倒序**（dart 实测，不是空循环）。
    pub(super) fn for_rule(
        &mut self,
        at: &'a oxc_css_parser::ast::AtRule<'a>,
        parent: Option<&SelectorSet>,
        parent_span: Span,
        decls: &mut Vec<DeclItem>,
        nested: &mut Vec<Item>,
    ) -> Result<(), Box<CompileError>> {
        let (Some(AtRulePrelude::SassFor(spec)), Some(block)) = (&at.prelude, &at.block) else {
            return Err(self.err(at.span.start, "SCSS 解析失败: @for"));
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

    /// `@while`：条件每轮重新求值。
    pub(super) fn while_rule(
        &mut self,
        at: &'a oxc_css_parser::ast::AtRule<'a>,
        parent: Option<&SelectorSet>,
        parent_span: Span,
        decls: &mut Vec<DeclItem>,
        nested: &mut Vec<Item>,
    ) -> Result<(), Box<CompileError>> {
        let (Some(AtRulePrelude::SassExpr(cond)), Some(block)) = (&at.prelude, &at.block) else {
            return Err(self.err(at.span.start, "SCSS 解析失败: @while"));
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
