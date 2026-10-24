use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use oxc_css_parser::ast::*;
use oxc_css_parser::pos::Span;

use super::emit::merge_groups;
use super::eval::{callee_raw_tokens, param_name, params_of};
use super::guard::Val;
use super::selector::selector_tokens;
use super::value::{self, eval, Color, PropSource, VarRef};
use super::Compiler;

/// 一层变量表（mixin 闭包与展开注入用）。
pub(super) type Vars<'a> = HashMap<String, VarRef<'a>>;

/// mixin 展开时注入的作用域，由外到内：闭包帧、参数帧、体帧。
pub(super) type Inject<'a> = Rc<Vec<Vars<'a>>>;

/// 规则集的一段体：detached ruleset 的块，或一条 mixin 定义的块。
#[derive(Clone)]
pub(crate) struct RulePart<'a> {
    pub(crate) block: &'a SimpleBlock<'a>,
    /// 该体的捕获帧（外→内；不含体自身变量帧）
    pub(crate) frames: Inject<'a>,
}

/// detached ruleset（`@dr: {...}`）或作为规则集使用的 mixin 调用结果。
///
/// 同名 mixin 可以有多条定义：less 的 `MixinCall.eval` 把所有命中的体按定义序拼进
/// 同一个规则集，所以查找是合并查找——后定义优先，后定义缺的项回落到前定义。
#[derive(Clone)]
pub(crate) struct RuleSet<'a> {
    pub(crate) parts: Vec<RulePart<'a>>,
}

impl<'a> RuleSet<'a> {
    pub(super) fn single(block: &'a SimpleBlock<'a>, frames: Inject<'a>) -> Self {
        Self {
            parts: vec![RulePart { block, frames }],
        }
    }
}

/// 查找用的一条声明：声明本身 + 它所在体的作用域（已含体变量帧与属性表）。
pub(super) struct FlatRule<'a> {
    pub(super) decl: Option<&'a Declaration<'a>>,
    pub(super) var: Option<&'a LessVariableDeclaration<'a>>,
    pub(super) scope: Inject<'a>,
    pub(super) props: Rc<HashMap<String, Vec<PropSource<'a>>>>,
}

/// 命名空间查找的一级结果（`@p[key]` 可能是值，也可能是嵌套规则集）。
pub(super) enum Lookup<'a> {
    Text(String),
    Ruleset(RuleSet<'a>),
}

/// 一个 mixin 定义（带括号的定义或单个选择器的规则）。
#[derive(Clone)]
pub(super) struct MixinDef<'a> {
    /// 绝对选择器 token 路径（含自身），登记时按定义处作用域插值
    pub(super) tokens: Vec<String>,
    /// 自身占用的 token
    pub(super) own: Vec<String>,
    /// 与 `tokens` 等长：该层能否作为命名空间链路元素被零实参进入
    pub(super) chain: Vec<bool>,
    /// 无括号定义没有参数表
    pub(super) params: Option<&'a LessMixinParameters<'a>>,
    /// 定义自身的 guard（`when (...)`）
    pub(super) guard: Option<&'a LessConditions<'a>>,
    /// 命名空间路径上的 guard 及其求值作用域
    pub(super) conds: Vec<(&'a LessConditions<'a>, Inject<'a>)>,
    pub(super) block: &'a SimpleBlock<'a>,
    /// 定义处的变量作用域链（外→内），闭包
    pub(super) closure: Inject<'a>,
}

#[derive(Clone)]
pub(super) struct Frame<'a> {
    pub(super) vars: HashMap<String, VarRef<'a>>,
    /// 本层的属性声明（`$prop` 查找用）
    pub(super) props: HashMap<String, Vec<PropSource<'a>>>,
    /// 变量声明值末尾的块注释（less 把注释当值节点，引用处带出）
    pub(super) suffixes: HashMap<String, String>,
}

#[derive(Clone)]
pub(super) struct Env<'a> {
    pub(super) frames: Vec<Frame<'a>>,
    /// 文件类函数的查找根（入口文件目录 + `paths`），随作用域副本一起传递。
    pub(super) roots: Option<Rc<Roots>>,
    /// 求值期未命中的查找（less 在同样输入上抛错，这里收集起来统一报错）。
    pub(super) misses: Rc<RefCell<Vec<(String, Span)>>>,
    /// 源码里有 `@plugin` 时关闭未定义变量报错：插件可在求值期注入变量。
    pub(super) plugins: bool,
}

/// `data-uri` / `image-size` 的相对路径基准。
///
/// less 用 `currentFileInfo.entryPath`（rewriteUrls 关闭时）——即**入口文件**所在目录，
/// 而不是当前被内联的文件；`@import` 已经按文本内联，这份目录信息只能从这里带进求值层。
pub(super) struct Roots {
    pub(super) dir: PathBuf,
    pub(super) paths: Vec<PathBuf>,
}

impl<'a> Env<'a> {
    pub(super) fn new(root: HashMap<String, VarRef<'a>>) -> Self {
        Self {
            frames: vec![Frame {
                vars: root,
                props: HashMap::new(),
                suffixes: HashMap::new(),
            }],
            roots: None,
            misses: Rc::default(),
            plugins: false,
        }
    }
    pub(super) fn push(&mut self) {
        self.frames.push(Frame {
            vars: HashMap::new(),
            props: HashMap::new(),
            suffixes: HashMap::new(),
        });
    }
    pub(super) fn pop(&mut self) {
        self.frames.pop();
    }
    /// 压入 mixin 展开的注入作用域（闭包 + 参数 + 体变量）。
    pub(super) fn push_scope(&mut self, inject: &Option<Inject<'a>>) {
        if let Some(frames) = inject {
            self.frames.extend(frames.iter().map(|vars| Frame {
                vars: vars.clone(),
                props: HashMap::new(),
                suffixes: HashMap::new(),
            }));
        }
    }
    pub(super) fn pop_scope(&mut self, inject: &Option<Inject<'a>>) {
        if let Some(frames) = inject {
            self.frames.truncate(self.frames.len() - frames.len());
        }
    }
    /// 同上，并记下值末尾的块注释（引用处带出）。
    pub(super) fn set_var(&mut self, name: String, node: &'a ComponentValue<'a>, suffix: Option<String>) {
        if let Some(f) = self.frames.last_mut() {
            f.vars.insert(name.clone(), VarRef::Ast(node));
            if let Some(suffix) = suffix {
                f.suffixes.insert(name, suffix);
            }
        }
    }
    /// 写入已求值的文本变量（mixin 体变量外泄到调用块用）。
    pub(super) fn set_text(&mut self, name: String, value: String) {
        if let Some(f) = self.frames.last_mut() {
            f.vars.insert(name, VarRef::Text(value));
        }
    }
    /// 叠加注入帧后的作用域副本（在规则集体的作用域里求值）。
    pub(super) fn scoped(&self, extra: &Inject<'a>) -> Env<'a> {
        let mut frames = self.frames.clone();
        frames.extend(extra.iter().map(|vars| Frame {
            vars: vars.clone(),
            props: HashMap::new(),
            suffixes: HashMap::new(),
        }));
        Env {
            frames,
            roots: self.roots.clone(),
            misses: self.misses.clone(),
            plugins: self.plugins,
        }
    }
    /// 叠加一层属性表后的作用域副本（detached ruleset 体里 `$prop` 查同块声明）。
    pub(super) fn with_props(&self, props: HashMap<String, Vec<PropSource<'a>>>) -> Env<'a> {
        let mut frames = self.frames.clone();
        frames.push(Frame {
            vars: HashMap::new(),
            props,
            suffixes: HashMap::new(),
        });
        Env {
            frames,
            roots: self.roots.clone(),
            misses: self.misses.clone(),
            plugins: self.plugins,
        }
    }
    /// 未命中的变量引用：less 报 NameError，调用方据此统一报错（`@plugin` 存在时除外）。
    pub(super) fn report_var_miss(&self, name: &str, span: Span) {
        if !self.plugins {
            self.miss(format!("variable @{name} is undefined"), span);
        }
    }
    /// 记录一次求值期语义错误（未命中的变量/属性查找）。
    pub(super) fn miss(&self, message: String, span: Span) {
        self.misses.borrow_mut().push((message, span));
    }
    pub(super) fn set_props(&mut self, props: HashMap<String, Vec<PropSource<'a>>>) {
        if let Some(f) = self.frames.last_mut() {
            f.props = props;
        }
    }
    pub(super) fn get(&self, name: &str) -> Option<VarRef<'a>> {
        self.frames
            .iter()
            .rev()
            .find_map(|f| f.vars.get(name).cloned())
    }
    pub(super) fn get_prop(&self, name: &str) -> Option<Vec<PropSource<'a>>> {
        self.frames
            .iter()
            .rev()
            .find_map(|f| f.props.get(name).cloned())
    }
    pub(super) fn var_suffix(&self, name: &str) -> Option<String> {
        self.frames
            .iter()
            .rev()
            .find_map(|f| f.suffixes.get(name).cloned())
    }
    /// 当前可见变量帧的快照（外→内）。
    pub(super) fn snapshot(&self) -> Vec<Vars<'a>> {
        self.frames.iter().map(|f| f.vars.clone()).collect()
    }
}

impl<'a> value::Scope<'a> for Env<'a> {
    fn var(&self, name: &str) -> Option<VarRef<'a>> {
        self.get(name)
    }
    fn prop(&self, name: &str) -> Option<Vec<PropSource<'a>>> {
        self.get_prop(name)
    }
    fn var_suffix(&self, name: &str) -> Option<String> {
        self.var_suffix(name)
    }
    fn file_roots(&self) -> Option<(&Path, &[PathBuf])> {
        self.roots
            .as_deref()
            .map(|roots| (roots.dir.as_path(), roots.paths.as_slice()))
    }
    fn prop_miss(&self, name: &str, span: Span) {
        self.miss(format!("Property '${name}' is undefined"), span);
    }
    fn var_miss(&self, name: &str, span: Span) {
        self.report_var_miss(name, span);
    }
}

/// 体变量：只含体顶层声明（lazy，后定义覆盖），供展开时的注入帧使用。
pub(super) fn body_vars<'a>(block: &'a SimpleBlock<'a>) -> Vars<'a> {
    let mut vars = Vars::new();
    for st in &block.statements {
        if let Statement::LessVariableDeclaration(d) = st {
            vars.insert(d.name.name.name.to_string(), VarRef::Ast(&d.value));
        }
    }
    vars
}

/// 帧栈直接当作用域用（避免为每次查找克隆整栈）。
pub(super) struct Frames<'a, 'b>(pub(super) &'b [Vars<'a>]);

impl<'a> value::Scope<'a> for Frames<'a, '_> {
    fn var(&self, name: &str) -> Option<VarRef<'a>> {
        self.0.iter().rev().find_map(|f| f.get(name).cloned())
    }

    fn prop(&self, _: &str) -> Option<Vec<PropSource<'a>>> {
        None
    }
}

/// mixin 定义表：按自身名（末 token）分桶，避免每次调用都全表扫描。
#[derive(Default)]
pub(super) struct Defs<'a> {
    pub(super) list: Vec<MixinDef<'a>>,
    pub(super) index: HashMap<String, Vec<usize>>,
}

impl<'a> Defs<'a> {
    /// 登记一条定义；同一 (体, token 链) 重复时保留先登记的（闭包等价）。
    pub(super) fn insert(&mut self, def: MixinDef<'a>) {
        let bucket = self
            .index
            .entry(def.tokens.last().cloned().unwrap_or_default())
            .or_default();
        // 同一 (体, token 链) 重复登记时只刷新闭包：动态注册的定义随每次展开更新
        if let Some(&i) = bucket.iter().find(|&&i| {
            std::ptr::eq(self.list[i].block, def.block) && self.list[i].tokens == def.tokens
        }) {
            self.list[i].closure = def.closure;
            return;
        }
        bucket.push(self.list.len());
        self.list.push(def);
    }

    pub(super) fn candidates(&self, name: &str) -> &[usize] {
        self.index.get(name).map(Vec::as_slice).unwrap_or_default()
    }
}

/// 调用实参（值已求值）。
pub(super) struct CallArg<'a> {
    pub(super) name: Option<String>,
    pub(super) text: String,
    /// 空格分隔的值列表在该实参里的元素（`length(@arguments)` 依赖它）
    pub(super) items: Vec<String>,
    pub(super) val: Val,
    /// 颜色实参的浮点通道（mixin 形参绑定时保留，避免 `#rrggbb` 往返丢精度）
    pub(super) color: Option<Color>,
    /// 实参是规则集（detached ruleset / 无参 mixin 调用）时带出的体
    pub(super) ruleset: Option<Rc<RuleSet<'a>>>,
    /// 未计算的除法（math=parens-division 下保留为 Operation）：形参绑成 AST，
    /// 这样 `(@b)` 括号内能算出来、括号外仍原样输出
    pub(super) division: Option<&'a ComponentValue<'a>>,
}

/// 候选定义 + 已绑定的形参（guard 结果决定是否展开）。
pub(super) struct Candidate<'a> {
    pub(super) def: MixinDef<'a>,
    /// `default()` 两趟求值得到的分组
    pub(super) group: u8,
    pub(super) frame: Vars<'a>,
    pub(super) arguments: Vec<String>,
}

impl<'a> Compiler<'a> {
    /// 值节点 → 规则集（detached ruleset / 无参 mixin 调用 / 变量 / 嵌套查找）。
    pub(super) fn ruleset_of(&self, node: &'a ComponentValue<'a>, depth: usize) -> Option<RuleSet<'a>> {
        if depth > 8 {
            return None;
        }
        match node {
            ComponentValue::LessDetachedRuleset(d) => Some(self.ruleset_block(&d.block)),
            ComponentValue::LessMixinCall(call) => self.mixin_ruleset(call),
            ComponentValue::LessVariable(v) => self.var_ruleset(v.name.name, depth + 1),
            ComponentValue::LessVariableCall(c) => {
                self.var_ruleset(c.variable.name.name, depth + 1)
            }
            ComponentValue::LessNamespaceValue(ns) => match self.namespace_lookup(ns, depth + 1)? {
                Lookup::Ruleset(rs) => Some(rs),
                Lookup::Text(_) => None,
            },
            _ => None,
        }
    }

    /// 块级规则集：捕获当前作用域（detached ruleset 的环境就是求值到它的那一刻）。
    pub(super) fn ruleset_block(&self, block: &'a SimpleBlock<'a>) -> RuleSet<'a> {
        RuleSet::single(block, Rc::new(self.env.snapshot()))
    }

    pub(super) fn var_ruleset(&self, name: &str, depth: usize) -> Option<RuleSet<'a>> {
        match self.env.get(name)? {
            VarRef::Ruleset(rs) => Some((*rs).clone()),
            VarRef::Ast(node) => self.ruleset_of(node, depth + 1),
            _ => None,
        }
    }

    /// 无参 mixin 调用 → 其体对应的规则集（`each(.set(), …)` / `@p: .mk-map()` / `#ns[prop]`）。
    ///
    /// 全部命中定义按定义序合并（less `MixinCall.eval` 拼接所有候选的规则）。
    pub(super) fn mixin_ruleset(&self, call: &'a LessMixinCall<'a>) -> Option<RuleSet<'a>> {
        let tokens: Vec<String> = callee_raw_tokens(call, self.src)
            .iter()
            .flat_map(|t| selector_tokens(&self.interpolate(t)))
            .collect();
        let args = self.eval_args(call);
        let parts: Vec<RulePart<'a>> = self
            .call_defs(&tokens, &args)
            .into_iter()
            .map(|def| {
                // 实参按位置/名字绑到形参：体内的 `@param` 引用才能在查找时求值
                let (frame, arguments, _) = self.bind_call_args(&def, &args);
                RulePart {
                    block: def.block,
                    frames: self.make_inject(&def, frame, arguments),
                }
            })
            .collect();
        (!parts.is_empty()).then_some(RuleSet { parts })
    }

    /// 调用命中的定义：最内层帧的全部候选（过实参与 guard 筛选），保持定义序。
    pub(super) fn call_defs(&self, tokens: &[String], args: &[CallArg<'a>]) -> Vec<MixinDef<'a>> {
        let defs = self.find_mixins(tokens);
        let Some(split) = defs.iter().map(|(_, split)| *split).max() else {
            return Vec::new();
        };
        defs.into_iter()
            .filter(|(_, s)| *s == split)
            .map(|(def, _)| def)
            .filter(|def| self.match_args(def, args) && self.guards_pass(def, args))
            .collect()
    }

    /// 实参按位置/名绑到形参（命名空间查找只需文本与 guard 值）。
    pub(super) fn bind_call_args(
        &self,
        def: &MixinDef<'a>,
        args: &[CallArg<'a>],
    ) -> (Vars<'a>, Vec<String>, HashMap<String, Val>) {
        let params = params_of(def);
        let mut frame = Vars::new();
        let mut vals: HashMap<String, Val> = HashMap::new();
        let mut arguments = Vec::new();
        let mut positional = 0;
        for arg in args {
            let name = match &arg.name {
                Some(name) => Some(name.clone()),
                None => {
                    let name = params
                        .get(positional)
                        .and_then(param_name)
                        .map(str::to_string);
                    positional += 1;
                    arguments.push(arg.text.clone());
                    name
                }
            };
            if let Some(name) = name {
                frame.insert(name.clone(), VarRef::Text(arg.text.clone()));
                vals.insert(name, arg.val.clone());
            }
        }
        (frame, arguments, vals)
    }

    /// 一段体的求值作用域：捕获帧 + 体变量帧（体变量后声明胜出）。
    pub(super) fn part_scope(&self, part: &RulePart<'a>) -> Inject<'a> {
        let mut frames = (*part.frames).clone();
        frames.push(body_vars(part.block));
        Rc::new(frames)
    }

    /// 一段体的完整求值环境：作用域帧 + 本层属性表（`$prop` 查同块声明）。
    pub(super) fn rule_env(&self, rule: &FlatRule<'a>) -> Env<'a> {
        let mut env = self.env.scoped(&rule.scope);
        if let Some(frame) = env.frames.last_mut() {
            frame.props = (*rule.props).clone();
        }
        env
    }

    /// 一层块自身的属性表（`$prop` / `[]` 查找用）。
    pub(super) fn block_props(&self, block: &'a SimpleBlock<'a>) -> HashMap<String, Vec<PropSource<'a>>> {
        let mut decls: HashMap<String, Vec<&'a Declaration<'a>>> = HashMap::new();
        for st in &block.statements {
            if let Statement::Declaration(d) = st {
                decls.entry(self.prop_name(d)).or_default().push(d);
            }
        }
        decls
            .into_iter()
            .map(|(name, ds)| {
                let sources = if ds.iter().any(|d| d.less_property_merge.is_some()) {
                    let groups = merge_groups(&ds)
                        .into_iter()
                        .map(|g| g.into_iter().map(|i| ds[i]).collect())
                        .collect();
                    vec![PropSource::Merged(groups)]
                } else {
                    ds.into_iter().map(PropSource::Decl).collect()
                };
                (name, sources)
            })
            .collect()
    }

    /// 查找用的规则序列：嵌套的 mixin / detached ruleset 调用就地展开
    /// （less 在 `Ruleset.eval` 时已把 mixin 产物拼接进规则数组）。
    pub(super) fn flat_rules(&self, rs: &RuleSet<'a>, depth: usize) -> Vec<FlatRule<'a>> {
        let mut out = Vec::new();
        if depth > 8 {
            return out;
        }
        for part in &rs.parts {
            let scope = self.part_scope(part);
            let props = Rc::new(self.block_props(part.block));
            for st in &part.block.statements {
                match st {
                    Statement::Declaration(d) => out.push(FlatRule {
                        decl: Some(d),
                        var: None,
                        scope: scope.clone(),
                        props: props.clone(),
                    }),
                    Statement::LessVariableDeclaration(v) => out.push(FlatRule {
                        decl: None,
                        var: Some(v),
                        scope: scope.clone(),
                        props: props.clone(),
                    }),
                    Statement::LessMixinCall(call) => {
                        if let Some(nested) = self.mixin_ruleset(call) {
                            out.extend(self.flat_rules(&nested, depth + 1));
                        }
                    }
                    Statement::LessVariableCall(call) => {
                        if let Some(nested) = self.var_ruleset(call.variable.name.name, depth + 1) {
                            out.extend(self.flat_rules(&nested, depth + 1));
                        }
                    }
                    _ => {}
                }
            }
        }
        out
    }

    pub(super) fn namespace_lookup(&self, ns: &'a LessNamespaceValue<'a>, depth: usize) -> Option<Lookup<'a>> {
        self.namespace_lookup_checked(ns, depth).ok()
    }

    /// 命名空间查找（带错误消息）：未命中的变量/属性在 less 里是 NameError，
    /// 这里把消息带回调用方（以前一律当成空值静默输出）。
    pub(super) fn namespace_lookup_checked(
        &self,
        ns: &'a LessNamespaceValue<'a>,
        depth: usize,
    ) -> Result<Lookup<'a>, String> {
        if depth > 8 {
            return Err("namespace lookup is too deep".to_string());
        }
        let mut cur = match &ns.callee {
            LessNamespaceValueCallee::LessVariable(v) => {
                let name = v.name.name;
                if self.env.get(name).is_none() {
                    return Err(format!("variable @{name} is undefined"));
                }
                match self.var_ruleset(name, depth + 1) {
                    Some(rs) => Lookup::Ruleset(rs),
                    None => return Err(format!("Could not evaluate variable call @{name}")),
                }
            }
            LessNamespaceValueCallee::LessMixinCall(call) => match self.mixin_ruleset(call) {
                Some(rs) => Lookup::Ruleset(rs),
                None => {
                    return Err(format!(
                        "{} is undefined",
                        self.slice(call.callee.span).trim()
                    ));
                }
            },
        };
        for lookup in &ns.lookups.lookups {
            let Lookup::Ruleset(rs) = cur else {
                return Err("Could not evaluate variable call".to_string());
            };
            cur = self.lookup_in(&rs, lookup, depth + 1)?;
        }
        Ok(cur)
    }

    /// `@p[text]` / `#ns[prop]` 的一级查找（对齐 less `NamespaceValue.eval`）。
    pub(super) fn lookup_in(
        &self,
        rs: &RuleSet<'a>,
        lookup: &LessLookup<'a>,
        depth: usize,
    ) -> Result<Lookup<'a>, String> {
        let name = match &lookup.name {
            // 空 lookup（`@p[]`）取最后一条声明的值
            None => {
                return self
                    .ruleset_last_decl(rs, depth)
                    .map(Lookup::Text)
                    .ok_or_else(|| "property \"\" not found".to_string());
            }
            Some(name) => name,
        };
        let key = match name {
            LessLookupName::LessVariable(v) => format!("@{}", v.name.name),
            LessLookupName::LessVariableVariable(vv) => format!(
                "@{}",
                value::var_name(vv.variable.name.name, self.src, &self.env, 0, None)
            ),
            LessLookupName::LessPropertyVariable(p) => p.name.name.to_string(),
            LessLookupName::LessPropertyInterpolation(p) => {
                value::var_name(p.name.name, self.src, &self.env, 0, None)
            }
            LessLookupName::Ident(id) => id.name.to_string(),
        };
        match key.strip_prefix('@') {
            Some(var) => self
                .ruleset_var(rs, var, depth + 1)
                .ok_or_else(|| format!("variable @{var} not found")),
            None => {
                let prop = key.trim_start_matches('$');
                self.ruleset_prop(rs, prop, depth)
                    .ok_or_else(|| format!("property \"{prop}\" not found"))
            }
        }
    }

    pub(super) fn ruleset_var(&self, rs: &RuleSet<'a>, name: &str, depth: usize) -> Option<Lookup<'a>> {
        let rules = self.flat_rules(rs, depth + 1);
        let rule = rules
            .iter()
            .rev()
            .find(|r| r.var.is_some_and(|v| v.name.name.name == name))?;
        let node = &rule.var?.value;
        match self.ruleset_at(node, &rule.scope, depth + 1) {
            Some(inner) => Some(Lookup::Ruleset(inner)),
            None => {
                let env = self.rule_env(rule);
                Some(Lookup::Text(eval(node, self.src, &env, depth + 1)))
            }
        }
    }

    /// 变量声明值 → 规则集；detached ruleset 体用声明所在体的作用域（而不是调用处）。
    pub(super) fn ruleset_at(
        &self,
        node: &'a ComponentValue<'a>,
        scope: &Inject<'a>,
        depth: usize,
    ) -> Option<RuleSet<'a>> {
        match node {
            ComponentValue::LessDetachedRuleset(d) => {
                Some(RuleSet::single(&d.block, scope.clone()))
            }
            _ => self.ruleset_of(node, depth + 1),
        }
    }

    pub(super) fn ruleset_prop(&self, rs: &RuleSet<'a>, name: &str, depth: usize) -> Option<Lookup<'a>> {
        let rules = self.flat_rules(rs, depth + 1);
        let rule = rules.iter().rev().find(|r| {
            r.decl
                .is_some_and(|d| self.interpolate(self.slice(*d.name.span())) == name)
        })?;
        let decl = rule.decl?;
        let env = self.rule_env(rule);
        Some(Lookup::Text(value::eval_values(
            &decl.value,
            self.src,
            &env,
            depth + 1,
        )))
    }

    /// less `Ruleset.lastDeclaration`：最后一条声明（变量声明也算，`@r: 20px` 就是
    /// 一条 variable 声明），`[]` 空查找取它的值。
    pub(super) fn ruleset_last_decl(&self, rs: &RuleSet<'a>, depth: usize) -> Option<String> {
        let rules = self.flat_rules(rs, depth + 1);
        let rule = rules.last()?;
        let env = self.rule_env(rule);
        match (rule.decl, rule.var) {
            (Some(d), _) => Some(value::eval_values(&d.value, self.src, &env, depth + 1)),
            (None, Some(v)) => Some(eval(&v.value, self.src, &env, depth + 1)),
            _ => None,
        }
    }

    /// 命名空间查找的最终文本（规则集结果按 less 取最后一条声明）。
    pub(super) fn namespace_text(
        &self,
        ns: &'a LessNamespaceValue<'a>,
        depth: usize,
    ) -> Result<String, String> {
        match self.namespace_lookup_checked(ns, depth)? {
            Lookup::Text(text) => Ok(text),
            Lookup::Ruleset(rs) => self
                .ruleset_last_decl(&rs, depth + 1)
                .ok_or_else(|| "property \"\" not found".to_string()),
        }
    }
}
