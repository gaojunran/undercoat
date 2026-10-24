//! SassScript 求值：表达式解析（含 `/` 重分组）→ 类型化值 → 变量/作用域。
//!
//! 解析器不直接用 oxc 的表达式树：oxc 的优先级与 dart-sass 的「斜杠语法」不同
//! （`1 / 2 * 3` 会被折成 `1 / (2 * 3)`）。这里把 oxc 的表达式节点**摊平成
//! token 序列**（`SassBinaryExpression` 递归还原成中缀顺序），再用 dart-sass 的
//! 优先级重新分组：
//!
//! - 优先级：`or` < `and` < `== !=` < `< <= > >=` < `+ -` < `* / %` < 一元；
//! - `/` 是普通二元除法，但「整棵子树只由 `/` 与字面数字组成」时结果保留
//!   `a/b` 的书写形态（dart 的 `SassNumber.withSlash`）；
//! - 参与其它算术、变量赋值、括号内、作为其它运算的操作数 → 丢掉书写形态。

use std::borrow::Cow;
use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use oxc_css_parser::ast::*;
use oxc_css_parser::pos::Span;
use oxc_css_parser::{Allocator, ParserBuilder, Syntax};

use crate::scss::builtins::Args;
use crate::scss::modules::{self, Builtin};
use crate::scss::value::{self, List, Map, Number, SassStr, Sep, Value};

pub(crate) struct EvalError {
    /// 合并文本里的绝对偏移（用于换算文件行列）。
    pub(crate) at: usize,
    pub(crate) message: String,
}

impl EvalError {
    pub(crate) fn new(at: usize, message: impl Into<String>) -> Self {
        Self {
            at,
            message: message.into(),
        }
    }
}

type R<T> = Result<T, EvalError>;

/// 用户函数递归上限（dart 自身在深层递归时也会栈溢出）。
///
/// 取 64 而不是和 mixin 一样的 256：函数求值的栈帧更重（debug 构建里 128 层就
/// 会超默认 2 MiB 线程栈），而真实工程（bootstrap/foundation）的递归深度
/// 都在个位数。
const MAX_CALL_DEPTH: usize = 64;

/// 一次 `@include` / `@content` 的实参（已求值）。
#[derive(Default)]
pub(crate) struct Invocation {
    pub(crate) positional: Vec<Value>,
    /// (归一化名, 值, 关键字名 span)。
    pub(crate) keywords: Vec<(String, Value, Span)>,
}

/// Sass 标识符里 `-` 与 `_` 完全等价（变量 / 函数 / mixin 名都适用，命名空间不适用）：
/// 符号表统一以 `-` 形式存 key，查找时也归一。
pub(crate) fn normalize_name(name: &str) -> String {
    if name.contains('_') {
        name.replace('_', "-")
    } else {
        name.to_string()
    }
}

/// 有 `$rest` 时的遗留检查：多余的命名实参 + 「关键字 map 是否被读过」的共享标记。
pub(crate) struct RestCheck {
    pub(crate) names: Vec<String>,
    pub(crate) accessed: Rc<Cell<bool>>,
}

/// 一层词法帧。`semi_global` 标记控制指令（`@if` 等）的作用域：赋值优先落到
/// 已存在的变量而不是新建遮蔽（dart 的 `Environment.scope(semiGlobal: true)`）。
#[derive(Debug, Default, Clone)]
pub(crate) struct Frame<'a> {
    vars: HashMap<String, Value>,
    funcs: HashMap<String, FunctionDef<'a>>,
    semi_global: bool,
}

/// 一个形参（`@mixin` 声明、`@include ... using` 与 `@function` 共用）。
#[derive(Clone, Debug)]
pub(crate) struct Param<'a> {
    pub(crate) name: &'a str,
    pub(crate) default: Option<&'a ComponentValue<'a>>,
}

/// `@function` 定义。`scope`/`depth` 是定义处的词法位置：调用时把帧栈截到 `depth`，
/// 函数体看不到调用方的局部变量（dart 的闭包语义）。
#[derive(Clone, Debug)]
pub(crate) struct FunctionDef<'a> {
    pub(crate) params: Vec<Param<'a>>,
    pub(crate) rest: Option<&'a str>,
    pub(crate) body: &'a [Statement<'a>],
    /// 定义处偏移（`@function` 整体）：`Function finished without @return.` 报这里。
    pub(crate) at: usize,
    pub(crate) scope: usize,
    pub(crate) depth: usize,
}

/// 用户函数体执行的结果：正常走完（缺 `@return`）或提前返回。
pub(crate) enum Flow {
    Normal,
    Return(Value),
}

#[derive(Default)]
struct Scope<'a> {
    frames: Vec<Frame<'a>>,
}

impl<'a> Scope<'a> {
    fn new() -> Self {
        Self {
            frames: vec![Frame::default()],
        }
    }

    fn get(&self, name: &str) -> Option<&Value> {
        if name.contains('_') {
            let normalized = normalize_name(name);
            return self
                .frames
                .iter()
                .rev()
                .find_map(|f| f.vars.get(&normalized));
        }
        self.frames.iter().rev().find_map(|f| f.vars.get(name))
    }
}

/// `@use` 得到的命名空间：用户模块（作用域编号）或内置模块。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ModuleRef {
    User(usize),
    Builtin(Builtin),
}

/// `@use ... as *` 引入的模块（按引入顺序，后引入的优先）。
///
/// 成员只在本作用域可见：查找未限定名时与自有成员一起参与解析，但**不进入**
/// 本模块的对外成员表（`frames[0]`）——只有 `@forward` 才导出成员。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StarModule {
    User(usize),
    Builtin(Builtin),
}

/// 导出成员的种类：变量与非变量（函数与 mixin 共用一份 show/hide 名单）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MemberKind {
    Var,
    Member,
}

/// `@forward ... show / hide` 的成员名单（名字已归一化，变量不带 `$`）。
#[derive(Clone, Debug, Default)]
pub(crate) struct MemberFilter {
    pub(crate) vars: HashSet<String>,
    pub(crate) members: HashSet<String>,
}

impl MemberFilter {
    fn allows(&self, kind: MemberKind, name: &str) -> bool {
        match kind {
            MemberKind::Var => self.vars.contains(name),
            MemberKind::Member => self.members.contains(name),
        }
    }
}

/// 一条 `@forward` 边：把目标模块的成员导出给下游，可选字面前缀与成员过滤。
///
/// 转发成员是**惰性**解析的：它们不进本模块的成员表，因此在本模块内不可见，
/// 也让目标模块后续的改动能被下游看到。
#[derive(Clone, Debug)]
pub(crate) struct ForwardEdge {
    pub(crate) scope: usize,
    /// `@forward` 语句在合并文本里的起点（重复转发的报错位置）。
    pub(crate) at: usize,
    /// `as p-*` 的字面前缀（`p-`），拼在成员名之前。
    pub(crate) prefix: String,
    pub(crate) show: Option<MemberFilter>,
    pub(crate) hide: MemberFilter,
}

impl ForwardEdge {
    /// 过滤看的是**加前缀之后**的对外名字（dart 1.93.2 实测）。
    pub(crate) fn allows(&self, kind: MemberKind, name: &str) -> bool {
        self.show.as_ref().is_none_or(|f| f.allows(kind, name)) && !self.hide.allows(kind, name)
    }

    /// 对外名字去掉前缀后的内部名字。
    pub(crate) fn strip<'n>(&self, name: &'n str) -> Option<&'n str> {
        name.strip_prefix(self.prefix.as_str())
    }
}

/// 成员名是否私有（`-` / `_` 开头：不导出、不可从模块外访问）。
pub(crate) fn is_private(name: &str) -> bool {
    name.starts_with('-') || name.starts_with('_')
}

/// 一条待应用的 `with` 配置（`!default` 表示调用方可以覆盖）。
#[derive(Clone, Debug)]
pub(crate) struct PendingConfig {
    pub(crate) id: usize,
    pub(crate) name: String,
    pub(crate) value: Value,
    /// 配置项在合并文本里的起点（报错定位）。
    pub(crate) at: usize,
    pub(crate) default: bool,
}

/// `@forward` 链的递归上限（装配层的循环检测之外的兵底）。
const MAX_FORWARD_DEPTH: usize = 32;

/// 求值器：每个「作用域编号」拥有一套变量帧；`active` 决定当前使用哪一套。
pub(crate) struct Evaluator<'a> {
    scopes: Vec<Scope<'a>>,
    namespaces: Vec<HashMap<String, (ModuleRef, u32)>>,
    /// `@use ... as *` 引入的模块（按引入顺序，后引入的优先）。
    star: Vec<Vec<StarModule>>,
    /// 各作用域的 `@forward` 边（按声明顺序）。
    forwards: Vec<Vec<ForwardEdge>>,
    /// 各模块待应用的 `with` 配置。
    pending: Vec<Vec<PendingConfig>>,
    /// 所有配置项的 (id, 定位)：没人消费的在编译末尾报错。
    configs: Vec<(usize, usize)>,
    used_configs: HashSet<usize>,
    config_seq: usize,
    /// 各作用域声明过的 mixin 名（`meta.mixin-exists()` 用）。
    mixins: Vec<HashSet<String>>,
    active: usize,
    base: usize,
    /// 用户函数递归层数（上限见 `MAX_CALL_DEPTH`）。
    call_depth: usize,
    /// 当前父选择器文本（`&` 的值）；根层是 `None`（dart：`null`）。
    parent_selector: Option<String>,
    /// 当前是否在 mixin 体里，以及这次调用有没有传 content 块
    /// （`content-exists()` 用）。
    mixin_context: (bool, bool),
    /// 合并后的真实源码。插值 `#{...}` 走的是「包装成 `a{b:…}`」的临时文本，
    /// 其中的 span 需要 `base` 平移；而函数体来自真实 AST，必须用真实源码求值。
    real: Option<&'a str>,
}

impl<'a> Evaluator<'a> {
    pub(crate) fn new(count: usize) -> Self {
        let count = count.max(1);
        Self {
            scopes: (0..count).map(|_| Scope::new()).collect(),
            namespaces: (0..count).map(|_| HashMap::new()).collect(),
            star: (0..count).map(|_| Vec::new()).collect(),
            forwards: (0..count).map(|_| Vec::new()).collect(),
            pending: (0..count).map(|_| Vec::new()).collect(),
            configs: Vec::new(),
            used_configs: HashSet::new(),
            config_seq: 0,
            mixins: (0..count).map(|_| HashSet::new()).collect(),
            active: 0,
            base: 0,
            call_depth: 0,
            parent_selector: None,
            mixin_context: (false, false),
            real: None,
        }
    }

    pub(crate) fn set_source(&mut self, text: &'a str) {
        self.real = Some(text);
    }

    pub(crate) fn parent_selector_text(&self) -> Option<String> {
        self.parent_selector.clone()
    }

    /// 进入/离开一条规则时更新 `&` 的值。
    pub(crate) fn set_parent_selector(&mut self, text: Option<String>) {
        self.parent_selector = text;
    }

    pub(crate) fn enter(&mut self, scope: usize) {
        self.active = scope.min(self.scopes.len() - 1);
    }

    pub(crate) fn active_scope(&self) -> usize {
        self.active
    }

    /// 当前是否处于模块的全局层（顶层，或只被 `@if` 之类的 semi-global 帧包着）：
    /// `with` 配置只在这一层被变量声明消费。
    pub(crate) fn at_module_global(&self) -> bool {
        self.scopes[self.active].frames[1..]
            .iter()
            .all(|frame| frame.semi_global)
    }

    /// 当前作用域的帧数（`@include` 记录调用方环境深度用）。
    pub(crate) fn frame_depth(&self) -> usize {
        self.scopes[self.active].frames.len()
    }

    /// 进入一个块作用域（变量遮蔽外层）；文件顶层不调用（顶层变量是作用域的全局）。
    pub(crate) fn push_frame(&mut self, semi_global: bool) {
        self.scopes[self.active].frames.push(Frame {
            vars: HashMap::new(),
            funcs: HashMap::new(),
            semi_global,
        });
    }

    pub(crate) fn pop_frame(&mut self) {
        if self.scopes[self.active].frames.len() > 1 {
            self.scopes[self.active].frames.pop();
        }
    }

    /// 词法隔离：帧栈接回 / 摘下（`@include` / `@content` 要在定义环境里跑）。
    pub(crate) fn take_frames(&mut self, scope: usize) -> Vec<Frame<'a>> {
        let index = scope.min(self.scopes.len() - 1);
        std::mem::take(&mut self.scopes[index].frames)
    }

    pub(crate) fn put_frames(&mut self, scope: usize, frames: Vec<Frame<'a>>) {
        let index = scope.min(self.scopes.len() - 1);
        self.scopes[index].frames = frames;
    }

    /// 把帧栈截到 `depth`（词法闭包的定义环境），返回被摘下的尾部。
    fn truncate_frames(&mut self, scope: usize, depth: usize) -> Vec<Frame<'a>> {
        let mut frames = self.take_frames(scope);
        let tail = frames.split_off(depth.min(frames.len()));
        self.put_frames(scope, frames);
        tail
    }

    fn append_frames(&mut self, scope: usize, tail: Vec<Frame<'a>>) {
        let mut frames = self.take_frames(scope);
        frames.extend(tail);
        self.put_frames(scope, frames);
    }

    /// `@function`：登记到当前帧（函数体里的声明只活在这次调用里）。
    pub(crate) fn define_function(&mut self, name: &str, def: FunctionDef<'a>) {
        let name = normalize_name(name);
        if let Some(frame) = self.scopes[self.active].frames.last_mut() {
            frame.funcs.insert(name, def);
        }
    }

    /// 用户函数查找：先找自己的帧（从内向外），再找 `@use ... as *` 引入的模块
    /// （后引入的优先）。星号模块的成员只在本作用域可见，不对外导出。
    pub(crate) fn find_function(&self, name: &str) -> Option<FunctionDef<'a>> {
        let name = normalize_name(name);
        if let Some(def) = self.scopes[self.active]
            .frames
            .iter()
            .rev()
            .find_map(|f| f.funcs.get(&name))
        {
            return Some(def.clone());
        }
        self.star[self.active]
            .iter()
            .rev()
            .find_map(|module| match module {
                StarModule::User(scope) => self.export_function(*scope, &name),
                StarModule::Builtin(_) => None,
            })
    }

    /// 命名空间（`@use` 模块）里的函数：只看模块的全局帧。
    pub(crate) fn find_module_function(
        &self,
        namespace: &str,
        name: &str,
    ) -> Option<FunctionDef<'a>> {
        let ModuleRef::User(scope) = self.namespace_module(namespace)? else {
            return None;
        };
        self.export_function(scope, name)
    }

    /// 参数绑定：永远写当前帧（不能覆盖外层同名变量）。
    pub(crate) fn declare_local(&mut self, name: &str, value: Value) {
        let name = normalize_name(name);
        if let Some(frame) = self.scopes[self.active].frames.last_mut() {
            frame.vars.insert(name, value.without_slash());
        }
    }

    pub(crate) fn namespace_module(&self, namespace: &str) -> Option<ModuleRef> {
        self.namespaces[self.active]
            .get(namespace)
            .map(|(module, _)| *module)
    }

    /// 用户模块的作用域编号（`@import` 的 mixin 导入、`ns.mixin` 用）；
    /// 内置模块没有用户作用域。
    pub(crate) fn namespace_scope(&self, namespace: &str) -> Option<usize> {
        match self.namespace_module(namespace)? {
            ModuleRef::User(scope) => Some(scope),
            ModuleRef::Builtin(_) => None,
        }
    }

    /// 模块命名空间是否存在（内置模块也算）。
    pub(crate) fn has_namespace(&self, namespace: &str) -> bool {
        self.namespaces[self.active].contains_key(namespace)
    }

    /// 变量声明（dart-sass 语义）：
    /// - `!default`：仅当变量未定义或值为 `null` 时赋值；
    /// - `!global`：写作用域全局帧；
    /// - 普通赋值：写「已声明它的最近块帧」（更深块里的赋值能改到外层块的局部
    ///   变量），都不存在时在当前帧声明 —— 全局帧只有本层赋值才会改。
    pub(crate) fn define(&mut self, name: &str, value: Value, default: bool, global: bool) {
        let name = &normalize_name(name);
        let value = value.without_slash();
        if default {
            if let Some(current) = self.lookup(name) {
                if !current.is_null() {
                    return;
                }
            }
        }
        let scope = &mut self.scopes[self.active];
        if global {
            scope.frames[0].vars.insert(name.to_string(), value);
            return;
        }
        // 向上找「已经声明」的帧并覆值；全局帧只有在「根本身或上面的帧全是 @if 帧」时
        // 才算数（dart 实测：根层 `@if` 会改全局，规则里的 `@if` 只会新建局部）。
        let from = if scope.frames[1..].iter().all(|f| f.semi_global) {
            0
        } else {
            1
        };
        if let Some(offset) = scope.frames[from..]
            .iter()
            .rposition(|f| f.vars.contains_key(name))
        {
            scope.frames[from + offset]
                .vars
                .insert(name.to_string(), value);
            return;
        }
        let last = scope.frames.len() - 1;
        scope.frames[last].vars.insert(name.to_string(), value);
    }

    /// 变量查找：先找自己的帧，再找 `@use ... as *` 引入的模块（后引入的优先）。
    ///
    /// 星号模块的变量是**懒读取**的：既不复制进本模块的全局帧（否则会顺着
    /// `@use this as *` 泄漏给下游），也能看到模块后续的改动。
    pub(crate) fn lookup(&self, name: &str) -> Option<Cow<'_, Value>> {
        if let Some(value) = self.scopes[self.active].get(name) {
            return Some(Cow::Borrowed(value));
        }
        let name = normalize_name(name);
        self.star[self.active]
            .iter()
            .rev()
            .find_map(|module| match module {
                StarModule::User(scope) => self.export_var(*scope, &name).map(Cow::Owned),
                StarModule::Builtin(module) => module.variable(&name).map(Cow::Owned),
            })
    }

    /// `m.$var`：用户模块读全局帧，内置模块读静态变量表。
    pub(crate) fn module_variable(&self, namespace: &str, name: &str) -> Option<Option<Value>> {
        match self.namespace_module(namespace)? {
            ModuleRef::User(scope) => Some(self.export_var(scope, name)),
            ModuleRef::Builtin(module) => Some(module.variable(name)),
        }
    }

    fn insert_namespace(&mut self, namespace: &str, module: ModuleRef, file: u32) -> R<()> {
        let table = &mut self.namespaces[self.active];
        // 重名已由装配层按文件静态拦下；这里的重复声明（同一个文件被 `@import`
        // 两次）静默保留第一份。
        table.entry(namespace.to_string()).or_insert((module, file));
        Ok(())
    }

    pub(crate) fn add_namespace(&mut self, namespace: &str, scope: usize, file: u32) -> R<()> {
        self.insert_namespace(namespace, ModuleRef::User(scope), file)
    }

    pub(crate) fn add_builtin_namespace(
        &mut self,
        namespace: &str,
        module: Builtin,
        file: u32,
    ) -> R<()> {
        self.insert_namespace(namespace, ModuleRef::Builtin(module), file)
    }

    /// 登记一条 `@forward` 边（成员惰性解析，不复制进本模块的成员表）。
    pub(crate) fn add_forward(&mut self, scope: usize, edge: ForwardEdge) {
        self.forwards[scope].push(edge);
    }

    pub(crate) fn forwards(&self, scope: usize) -> &[ForwardEdge] {
        &self.forwards[scope]
    }

    pub(crate) fn scope_count(&self) -> usize {
        self.scopes.len()
    }

    /// 登记一条 `with` 配置（`default` = `@forward ... with (... !default)`）。
    ///
    /// 返回冲突位置：同一个变量已经有非 `!default` 配置时再配一次就报错。
    pub(crate) fn configure(
        &mut self,
        scope: usize,
        name: &str,
        value: Value,
        at: usize,
        default: bool,
    ) -> Result<(), usize> {
        let id = self.config_seq;
        self.config_seq += 1;
        self.configs.push((id, at));
        self.push_config(
            scope,
            PendingConfig {
                id,
                name: normalize_name(name),
                value,
                at,
                default,
            },
        )
    }

    /// 把一条配置放进目标模块（已存在同名配置时按 `!default` 规则合并）。
    pub(crate) fn push_config(&mut self, scope: usize, entry: PendingConfig) -> Result<(), usize> {
        let list = &mut self.pending[scope];
        let Some(existing) = list.iter_mut().find(|e| e.name == entry.name) else {
            list.push(entry);
            return Ok(());
        };
        // 同一条配置沿多条转发边下传：目标里已有的就是它自己。
        if existing.id == entry.id {
            return Ok(());
        }
        match (existing.default, entry.default) {
            // 已有硬配置：新的默认值被忽略。
            (false, true) => {
                self.used_configs.insert(entry.id);
            }
            // 默认值被硬配置覆盖：旧的那条不再报错。
            (true, false) => {
                self.used_configs.insert(existing.id);
                *existing = entry;
            }
            (true, true) => {
                self.used_configs.insert(entry.id);
            }
            // 两个硬配置：后到的那个报「不是 !default 变量」。
            (false, false) => return Err(entry.at),
        }
        Ok(())
    }

    /// 取走当前模块的同名配置值（声明处消费）。
    pub(crate) fn take_config(&mut self, name: &str) -> Option<Value> {
        let name = normalize_name(name);
        let used = self.used_configs.clone();
        let list = &mut self.pending[self.active];
        let pos = list
            .iter()
            .position(|e| e.name == name && !used.contains(&e.id))?;
        let entry = list.remove(pos);
        self.used_configs.insert(entry.id);
        Some(entry.value)
    }

    /// 同名配置还没被消费时，非 `!default` 声明要报错（返回配置位置）。
    pub(crate) fn config_conflict(&self, name: &str) -> Option<usize> {
        let name = normalize_name(name);
        self.pending[self.active]
            .iter()
            .find(|e| e.name == name && !self.used_configs.contains(&e.id))
            .map(|e| e.at)
    }

    /// 某条配置是否已经在链上被消费（`@forward` 下传时跳过）。
    pub(crate) fn config_used(&self, id: usize) -> bool {
        self.used_configs.contains(&id)
    }

    /// 当前模块待应用的配置（下传时是「抄一份」，配置本体留在本模块等自己的
    /// `!default` 声明消费——dart 的 `Configuration` 是共享对象）。
    pub(crate) fn pending_configs(&self, scope: usize) -> Vec<PendingConfig> {
        self.pending[scope].clone()
    }

    /// 编译末尾检查：有没有配置项没人消费（dart 报在配置处）。
    pub(crate) fn unresolved_config(&self) -> Option<usize> {
        self.configs
            .iter()
            .find(|(id, _)| !self.used_configs.contains(id))
            .map(|(_, at)| *at)
    }

    /// 模块对外成员（自己的全局帧 + `@forward` 链）里读变量。
    pub(crate) fn export_var(&self, scope: usize, name: &str) -> Option<Value> {
        self.export_var_at(scope, name, 0)
    }

    fn export_var_at(&self, scope: usize, name: &str, depth: usize) -> Option<Value> {
        if depth > MAX_FORWARD_DEPTH {
            return None;
        }
        let name = normalize_name(name);
        if !is_private(&name) {
            if let Some(value) = self.scopes[scope]
                .frames
                .first()
                .and_then(|frame| frame.vars.get(&name))
            {
                return Some(value.clone());
            }
        }
        self.forwards[scope].iter().rev().find_map(|edge| {
            if !edge.allows(MemberKind::Var, &name) {
                return None;
            }
            let inner = edge.strip(&name)?;
            self.export_var_at(edge.scope, inner, depth + 1)
        })
    }

    /// 模块对外成员里的函数。
    pub(crate) fn export_function(&self, scope: usize, name: &str) -> Option<FunctionDef<'a>> {
        self.export_function_at(scope, name, 0)
    }

    fn export_function_at(
        &self,
        scope: usize,
        name: &str,
        depth: usize,
    ) -> Option<FunctionDef<'a>> {
        if depth > MAX_FORWARD_DEPTH {
            return None;
        }
        let name = normalize_name(name);
        if !is_private(&name) {
            if let Some(def) = self.scopes[scope]
                .frames
                .first()
                .and_then(|frame| frame.funcs.get(&name))
            {
                return Some(def.clone());
            }
        }
        self.forwards[scope].iter().rev().find_map(|edge| {
            if !edge.allows(MemberKind::Member, &name) {
                return None;
            }
            let inner = edge.strip(&name)?;
            self.export_function_at(edge.scope, inner, depth + 1)
        })
    }

    /// 模块对外成员里的变量名（`meta.module-variables()`）。
    pub(crate) fn export_var_names(&self, scope: usize) -> Vec<(String, Value)> {
        let mut out = HashMap::new();
        self.collect_export_vars(scope, 0, &mut out);
        let mut list: Vec<_> = out.into_iter().collect();
        list.sort_by(|a, b| a.0.cmp(&b.0));
        list
    }

    fn collect_export_vars(&self, scope: usize, depth: usize, out: &mut HashMap<String, Value>) {
        if depth > MAX_FORWARD_DEPTH {
            return;
        }
        if let Some(frame) = self.scopes[scope].frames.first() {
            for (name, value) in &frame.vars {
                if !is_private(name) {
                    out.entry(name.clone()).or_insert_with(|| value.clone());
                }
            }
        }
        for edge in &self.forwards[scope] {
            let mut inner = HashMap::new();
            self.collect_export_vars(edge.scope, depth + 1, &mut inner);
            for (name, value) in inner {
                let outward = format!("{}{}", edge.prefix, name);
                if edge.allows(MemberKind::Var, &outward) {
                    out.entry(outward).or_insert(value);
                }
            }
        }
    }

    /// 变量最终定义在哪个模块（`@forward` 重复检测用）。
    pub(crate) fn export_var_origin(&self, scope: usize, name: &str) -> Option<(usize, String)> {
        self.export_var_origin_at(scope, name, 0)
    }

    fn export_var_origin_at(
        &self,
        scope: usize,
        name: &str,
        depth: usize,
    ) -> Option<(usize, String)> {
        if depth > MAX_FORWARD_DEPTH {
            return None;
        }
        let name = normalize_name(name);
        if !is_private(&name)
            && self.scopes[scope]
                .frames
                .first()
                .is_some_and(|frame| frame.vars.contains_key(&name))
        {
            return Some((scope, name));
        }
        self.forwards[scope].iter().rev().find_map(|edge| {
            if !edge.allows(MemberKind::Var, &name) {
                return None;
            }
            let inner = edge.strip(&name)?;
            self.export_var_origin_at(edge.scope, inner, depth + 1)
        })
    }

    /// 函数最终定义在哪个模块（`@forward` 重复检测用）。
    pub(crate) fn export_function_origin(
        &self,
        scope: usize,
        name: &str,
    ) -> Option<(usize, String)> {
        self.export_function_origin_at(scope, name, 0)
    }

    fn export_function_origin_at(
        &self,
        scope: usize,
        name: &str,
        depth: usize,
    ) -> Option<(usize, String)> {
        if depth > MAX_FORWARD_DEPTH {
            return None;
        }
        let name = normalize_name(name);
        if !is_private(&name)
            && self.scopes[scope]
                .frames
                .first()
                .is_some_and(|frame| frame.funcs.contains_key(&name))
        {
            return Some((scope, name));
        }
        self.forwards[scope].iter().rev().find_map(|edge| {
            if !edge.allows(MemberKind::Member, &name) {
                return None;
            }
            let inner = edge.strip(&name)?;
            self.export_function_origin_at(edge.scope, inner, depth + 1)
        })
    }

    /// 模块对外成员里的函数名（`meta.module-functions()`）。
    pub(crate) fn export_function_names(&self, scope: usize) -> Vec<String> {
        let mut out = HashSet::new();
        self.collect_export_functions(scope, 0, &mut out);
        let mut list: Vec<_> = out.into_iter().collect();
        list.sort();
        list
    }

    fn collect_export_functions(&self, scope: usize, depth: usize, out: &mut HashSet<String>) {
        if depth > MAX_FORWARD_DEPTH {
            return;
        }
        if let Some(frame) = self.scopes[scope].frames.first() {
            for name in frame.funcs.keys() {
                if !is_private(name) {
                    out.insert(name.clone());
                }
            }
        }
        for edge in &self.forwards[scope] {
            let mut inner = HashSet::new();
            self.collect_export_functions(edge.scope, depth + 1, &mut inner);
            for name in inner {
                let outward = format!("{}{}", edge.prefix, name);
                if edge.allows(MemberKind::Member, &outward) {
                    out.insert(outward);
                }
            }
        }
    }

    /// 模块对外成员里的 mixin 名（`mixin-exists()` 用；mixin 本体在 `Converter`）。
    pub(crate) fn export_mixin_name(&self, scope: usize, name: &str) -> bool {
        self.export_mixin_name_at(scope, name, 0)
    }

    fn export_mixin_name_at(&self, scope: usize, name: &str, depth: usize) -> bool {
        if depth > MAX_FORWARD_DEPTH {
            return false;
        }
        let name = normalize_name(name);
        if !is_private(&name) && self.mixins[scope].contains(&name) {
            return true;
        }
        self.forwards[scope].iter().rev().any(|edge| {
            if !edge.allows(MemberKind::Member, &name) {
                return false;
            }
            edge.strip(&name)
                .is_some_and(|inner| self.export_mixin_name_at(edge.scope, inner, depth + 1))
        })
    }

    /// `@use ... as *` 的用户模块：成员参与本作用域的无限定名解析，但不导出。
    pub(crate) fn import_star_module(&mut self, scope: usize) {
        let star = &mut self.star[self.active];
        let module = StarModule::User(scope);
        if !star.contains(&module) {
            star.push(module);
        }
    }

    /// `@use "sass:xxx" as *`：成员表参与无限定名解析（变量由 `lookup` 读取）。
    pub(crate) fn import_builtin_star(&mut self, module: Builtin) {
        let star = &mut self.star[self.active];
        let module = StarModule::Builtin(module);
        if !star.contains(&module) {
            star.push(module);
        }
    }

    /// 星号引入的用户模块作用域（后引入的在前），`@include` 的无限定名查找用。
    pub(crate) fn star_scopes(&self) -> impl Iterator<Item = usize> + '_ {
        self.star[self.active]
            .iter()
            .rev()
            .filter_map(|module| match module {
                StarModule::User(scope) => Some(*scope),
                StarModule::Builtin(_) => None,
            })
    }

    /// `as *` 的内置模块成员：按引入顺序倒查（后引入的优先），且**先于**全局内置函数表
    /// ——dart 实测 `@use "sass:color" as *` 会让 `lighten()` 报「不在模块里」。
    fn star_member(&self, name: &str) -> Option<Builtin> {
        self.star[self.active]
            .iter()
            .rev()
            .find_map(|module| match module {
                StarModule::Builtin(module) if module.has_member(name) => Some(*module),
                _ => None,
            })
    }

    /// 登记 mixin 名（`meta.mixin-exists()`）。
    pub(crate) fn define_mixin(&mut self, name: &str) {
        self.mixins[self.active].insert(normalize_name(name));
    }

    /// 当前 mixin 调用是否带 content 块（`@include m {}` 的 `{}` 也算）。
    pub(crate) fn set_mixin_context(&mut self, in_mixin: bool, has_content: bool) {
        self.mixin_context = (in_mixin, has_content);
    }

    // -----------------------------------------------------------------------
    // 对外入口
    // -----------------------------------------------------------------------

    /// 声明值 / 任意表达式序列。
    pub(crate) fn eval_value(&mut self, src: &str, comps: &[ComponentValue]) -> R<Value> {
        self.eval_flat(src, comps)
    }

    /// 单个组件值。
    pub(crate) fn eval_component(&mut self, src: &str, comp: &ComponentValue) -> R<Value> {
        self.eval_comp(src, comp)
    }

    /// 从文本求值（选择器 / media prelude 的 `#{}` 内部）：内部包一层
    /// `a{b:<text>}` 重新交给 oxc 词法，错误偏移再换算回原文。
    pub(crate) fn eval_text(&mut self, text: &str, at: usize) -> R<Value> {
        const PREFIX: usize = 4; // "a{b:"
        let wrapped = format!("a{{b:{text}}}");
        let alloc = Allocator::default();
        // 源文件即使是 `.sass`，值/选择器文本也已折成 SCSS 形态，这里固定按 SCSS 解析。
        let mut parser = ParserBuilder::new(&alloc, &wrapped)
            .syntax(Syntax::Scss)
            .build();
        let ast = parser.parse::<Stylesheet>().map_err(|e| {
            EvalError::new(
                at + e.span.start.saturating_sub(PREFIX),
                "Expected expression.",
            )
        })?;
        let values = ast
            .statements
            .iter()
            .find_map(|st| match st {
                Statement::QualifiedRule(rule) => {
                    rule.block.statements.iter().find_map(|st| match st {
                        Statement::Declaration(decl) => Some(&decl.value),
                        _ => None,
                    })
                }
                _ => None,
            })
            .ok_or_else(|| EvalError::new(at, "Expected expression."))?;
        let saved = self.base;
        self.base = at.saturating_sub(PREFIX);
        let result = self.eval_value(&wrapped, values);
        self.base = saved;
        result
    }

    /// 文本里的 `#{...}` 插值（结果为无引号文本）。
    pub(crate) fn interpolate(&mut self, text: &str, at: usize) -> R<String> {
        let bytes = text.as_bytes();
        let mut out = String::with_capacity(text.len());
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'#' && bytes.get(i + 1) == Some(&b'{') {
                let start = i + 2;
                let mut depth = 1usize;
                let mut quote: Option<u8> = None;
                let mut j = start;
                while j < bytes.len() {
                    let b = bytes[j];
                    if let Some(q) = quote {
                        if b == b'\\' {
                            j += 2;
                            continue;
                        }
                        if b == q {
                            quote = None;
                        }
                        j += 1;
                        continue;
                    }
                    match b {
                        b'\'' | b'"' => quote = Some(b),
                        b'{' => depth += 1,
                        b'}' => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                    j += 1;
                }
                if j >= bytes.len() {
                    return Err(EvalError::new(at + i, "Expected }."));
                }
                let value = self.eval_text(&text[start..j], at + start)?;
                out.push_str(
                    &value
                        .interpolation()
                        .map_err(|message| EvalError::new(at + start, message))?,
                );
                i = j + 1;
                continue;
            }
            let ch = text[i..].chars().next().expect("边界安全");
            out.push(ch);
            i += ch.len_utf8();
        }
        Ok(out)
    }

    // -----------------------------------------------------------------------
    // 表达式
    // -----------------------------------------------------------------------

    fn eval_expr(&mut self, src: &str, expr: &ExprKind) -> R<Value> {
        match expr {
            ExprKind::Val(comp) => self.eval_comp(src, comp),
            ExprKind::Unary(op, inner) => {
                let value = self.eval_expr(src, inner)?;
                Ok(match op.kind {
                    SassUnaryOperatorKind::Plus => match value {
                        Value::Number(n) => Value::Number(n.without_slash()),
                        other => Value::Str(SassStr::unquoted(format!("+{}", other.inspect()))),
                    },
                    SassUnaryOperatorKind::Minus => match value {
                        Value::Number(n) => Value::Number(Number {
                            value: -n.value,
                            numer: n.numer,
                            denom: n.denom,
                            slash: None,
                        }),
                        other => Value::Str(SassStr::unquoted(format!("-{}", other.inspect()))),
                    },
                    SassUnaryOperatorKind::Not => Value::Bool(!value.truthy()),
                })
            }
            ExprKind::Binary(op, left, right) => {
                match op {
                    OpKind::Ast(kind, _)
                        if matches!(
                            kind,
                            SassBinaryOperatorKind::And | SassBinaryOperatorKind::Or
                        ) =>
                    {
                        let lhs = self.eval_expr(src, left)?;
                        let is_and = matches!(kind, SassBinaryOperatorKind::And);
                        if is_and != lhs.truthy() {
                            return Ok(lhs);
                        }
                        return self.eval_expr(src, right);
                    }
                    _ => {}
                }
                let at = self.base + left.start();
                let lhs = self.eval_expr(src, left)?;
                let rhs = self.eval_expr(src, right)?;
                let result = if op.is_division() {
                    value::binary_div(&lhs, &rhs)
                } else {
                    value::binary(op.ast_kind(), &lhs, &rhs)
                }
                .map_err(|message| EvalError::new(at, message))?;
                if op.is_division() && left.slash_ok() && right.slash_ok() {
                    if let Value::Number(n) = result {
                        return Ok(Value::Number(Number::with_slash(
                            n,
                            lhs.clone(),
                            rhs.clone(),
                        )));
                    }
                }
                Ok(result)
            }
            ExprKind::List(items, sep, bracketed) => {
                let mut values = Vec::with_capacity(items.len());
                for item in items {
                    values.push(self.eval_expr(src, item)?);
                }
                if values.len() == 1 && !bracketed {
                    return Ok(values.pop().expect("非空"));
                }
                Ok(Value::List(List {
                    items: values,
                    sep: *sep,
                    bracketed: *bracketed,
                }))
            }
        }
    }

    fn eval_comp(&mut self, src: &str, comp: &ComponentValue) -> R<Value> {
        match comp {
            ComponentValue::SassVariable(v) => self.lookup_var(v.name.name, v.span),
            ComponentValue::SassQualifiedName(q) => match &q.member {
                SassModuleMemberName::Variable(v) => {
                    if is_private(v.name.name) {
                        return Err(EvalError::new(
                            self.base + q.span.start,
                            "Private members can't be accessed from outside their modules.",
                        ));
                    }
                    let Some(value) = self.module_variable(q.module.name, v.name.name) else {
                        return Err(EvalError::new(
                            self.base + q.span.start,
                            format!(
                                "There is no module with the namespace \"{}\".",
                                q.module.name
                            ),
                        ));
                    };
                    match value {
                        Some(value) => Ok(value.clone()),
                        None => Err(EvalError::new(
                            self.base + q.span.start,
                            "Undefined variable.",
                        )),
                    }
                }
                SassModuleMemberName::Ident(_) => Err(EvalError::new(
                    self.base + q.span.start,
                    "SCSS 特性尚未实现（S2）: 模块成员引用",
                )),
            },
            // `&` 在表达式里是当前父选择器（根层是 null）：`if(&, "&", "")` 靠它。
            ComponentValue::SassParentSelector(_) => Ok(match &self.parent_selector {
                Some(text) => Value::Str(value::SassStr::unquoted(text.clone())),
                None => Value::Null,
            }),
            ComponentValue::InterpolableIdent(ident) => self.eval_ident(src, ident),
            ComponentValue::InterpolableStr(s) => self.eval_str(src, s),
            ComponentValue::Function(f) => self.eval_function(src, f),
            ComponentValue::Url(u) => self.eval_url(src, u),
            // oxc 把 CSS 数学函数（`round()` / `mod()` …）的实参读成 `Calc` 节点；
            // 对 1.35.2 的 Sass 内置函数来说它就是一个普通表达式（`round(1/3)` = 0）。
            ComponentValue::Calc(c) => {
                let text = src.get(c.span.start..c.span.end).unwrap_or("");
                self.eval_text(text, c.span.start)
            }
            // `min()`/`max()` 的实参被 oxc 读成 Calc 语法树：括号里仍是普通表达式。
            ComponentValue::CalcParenthesized(p) => {
                let value = self.eval_comp(src, &p.expr)?;
                Ok(value.without_slash())
            }
            ComponentValue::SassParenthesizedExpression(p) => {
                let value = self.eval_flat(src, std::slice::from_ref(&p.expr))?;
                Ok(value.without_slash())
            }
            // 裸表达式节点（`(10px/2px)` 的内层、函数实参等）走同一套分组解析。
            ComponentValue::SassBinaryExpression(_) | ComponentValue::SassUnaryExpression(_) => {
                self.eval_flat(src, std::slice::from_ref(comp))
            }
            ComponentValue::SassMap(map) => {
                if map.items.is_empty() {
                    return Ok(Value::List(List {
                        items: Vec::new(),
                        sep: Sep::Space,
                        bracketed: false,
                    }));
                }
                let mut entries = Vec::with_capacity(map.items.len());
                for item in &map.items {
                    entries.push((
                        self.eval_comp(src, &item.key)?,
                        self.eval_comp(src, &item.value)?,
                    ));
                }
                Ok(Value::Map(Map { entries }))
            }
            // oxc 的 `SassList` 只是「非逗号分隔的一串」：处理掉里面的裸 `/`
            // （`#{10 / 4}` 的元素是 `10 | / | 4`）、一元与二元表达式。
            ComponentValue::SassList(list) if !matches!(&list.comma_spans, Some(spans) if !spans.is_empty()) => {
                self.eval_flat(src, &list.elements)
            }
            ComponentValue::SassList(list) => {
                let mut items = Vec::with_capacity(list.elements.len());
                for element in &list.elements {
                    items.push(self.eval_comp(src, element)?);
                }
                if items.len() == 1 {
                    return Ok(items.pop().expect("非空"));
                }
                Ok(Value::List(List {
                    items,
                    sep: Sep::Comma,
                    bracketed: false,
                }))
            }
            ComponentValue::BracketBlock(block) => {
                let mut value = self.eval_flat(src, &block.value)?;
                if let Value::List(list) = &mut value {
                    list.bracketed = true;
                } else {
                    value = Value::List(List {
                        items: vec![value],
                        sep: Sep::Space,
                        bracketed: true,
                    });
                }
                Ok(value)
            }
            ComponentValue::Dimension(d) => {
                // `10px-5px` 在 SCSS 里是减法（单位不会以 `-数字` 开头）。
                if let Some(pos) = unit_split_point(d.unit.name) {
                    let left = value::from_component(src, comp).map_err(|message| {
                        EvalError::new(self.base + comp.span().start, message)
                    })?;
                    let right = Value::Number(Number::with_unit(
                        value::parse_number(&d.unit.name[pos + 1..]),
                        &d.unit.name[..pos],
                    ));
                    return value::binary(&SassBinaryOperatorKind::Minus, &left, &right)
                        .map_err(|message| EvalError::new(self.base + comp.span().start, message));
                }
                value::from_component(src, comp)
                    .map_err(|message| EvalError::new(self.base + comp.span().start, message))
            }
            ComponentValue::Number(_)
            | ComponentValue::Percentage(_)
            | ComponentValue::Ratio(_)
            | ComponentValue::HexColor(_) => value::from_component(src, comp)
                .map_err(|message| EvalError::new(self.base + comp.span().start, message)),
            ComponentValue::SassArbitraryArgument(a) => Err(EvalError::new(
                self.base + a.span.start,
                "SCSS 特性尚未实现（S2）: 展开实参 `...`",
            )),
            ComponentValue::SassKeywordArgument(a) => Err(EvalError::new(
                self.base + a.span.start,
                "SCSS 特性尚未实现（S1）: 关键字实参",
            )),
            other => Ok(Value::Raw(
                src.get(other.span().start..other.span().end)
                    .unwrap_or("")
                    .to_string(),
            )),
        }
    }

    /// 组件序列 → 表达式（摊平成 token 后按 dart 优先级重分组）。
    fn eval_flat(&mut self, src: &str, comps: &[ComponentValue]) -> R<Value> {
        let mut toks = Vec::new();
        for comp in comps {
            flatten(comp, &mut toks);
        }
        let mut parser = ExprParser { toks, pos: 0 };
        let expr = parser
            .parse_list()
            .map_err(|e| EvalError::new(self.base + e.at, e.message))?;
        self.eval_expr(src, &expr)
    }

    fn lookup_var(&self, name: &str, span: Span) -> R<Value> {
        match self.lookup(name) {
            Some(value) => Ok(value.into_owned()),
            None => Err(EvalError::new(
                self.base + span.start,
                "Undefined variable.",
            )),
        }
    }

    fn eval_ident(&mut self, src: &str, ident: &InterpolableIdent) -> R<Value> {
        match ident {
            InterpolableIdent::Literal(i) => Ok(value::ident_value(i.name)),
            InterpolableIdent::SassInterpolated(s) => {
                let mut text = String::new();
                for element in &s.elements {
                    match element {
                        SassInterpolatedIdentElement::Static(part) => text.push_str(part.value),
                        SassInterpolatedIdentElement::Expression(comp) => {
                            let value = self.eval_comp(src, comp)?;
                            text.push_str(&value.interpolation().map_err(|message| {
                                EvalError::new(self.base + comp.span().start, message)
                            })?);
                        }
                        SassInterpolatedIdentElement::Placeholder(p) => {
                            return Err(EvalError::new(
                                self.base + p.span.start,
                                "SCSS 特性尚未实现（S1）: 模板占位符",
                            ));
                        }
                    }
                }
                Ok(Value::Str(SassStr::unquoted(text)))
            }
            _ => Err(EvalError::new(
                self.base + ident_span(ident).start,
                "SCSS 特性尚未实现（S1）: less 风格插值",
            )),
        }
    }

    fn eval_str(&mut self, src: &str, s: &InterpolableStr) -> R<Value> {
        match s {
            InterpolableStr::Literal(lit) => Ok(Value::Str(SassStr::quoted(lit.value))),
            InterpolableStr::SassInterpolated(interp) => {
                let mut text = String::new();
                for element in &interp.elements {
                    match element {
                        SassInterpolatedStrElement::Static(part) => text.push_str(part.value),
                        SassInterpolatedStrElement::Expression(comp) => {
                            let value = self.eval_comp(src, comp)?;
                            text.push_str(&value.interpolation().map_err(|message| {
                                EvalError::new(self.base + comp.span().start, message)
                            })?);
                        }
                    }
                }
                Ok(Value::Str(SassStr::quoted(text)))
            }
            _ => Err(EvalError::new(
                self.base + str_span(s).start,
                "SCSS 特性尚未实现（S1）: less 风格字符串插值",
            )),
        }
    }

    fn eval_url(&mut self, src: &str, url: &Url) -> R<Value> {
        let raw = src.get(url.span.start..url.span.end).unwrap_or("");
        let inner = raw
            .strip_prefix("url(")
            .and_then(|rest| rest.strip_suffix(')'))
            .unwrap_or("");
        let trimmed = inner.trim();
        // `url($x)`：dart-sass 把它当 SassScript 表达式求值。
        if trimmed.starts_with('$') {
            let value = self.eval_text(trimmed, url.span.start + 4)?;
            let text = value
                .to_css()
                .map_err(|message| EvalError::new(self.base + url.span.start, message))?;
            return Ok(Value::Str(value::SassStr::unquoted(format!("url({text})"))));
        }
        // dart 把 `url(...)` 解析成一个**不加引号的字符串**：`type-of(url(x))` 是
        // `string`，`str-slice(url(x), 1, 1)` 是 `u`（bootstrap 的 `escape-svg()` 依赖它）。
        let rewritten = value::rewrite_raw(src, url.span);
        let interpolated = self.interpolate(&rewritten, url.span.start)?;
        Ok(Value::Str(value::SassStr::unquoted(interpolated)))
    }

    fn eval_function(&mut self, src: &str, f: &Function) -> R<Value> {
        let name = match &f.name {
            FunctionName::Ident(ident) => match ident {
                InterpolableIdent::Literal(i) => i.name.to_string(),
                other => {
                    let value = self.eval_ident(src, other)?;
                    let (args, _) = self.eval_args(src, &f.args)?;
                    return Ok(Value::Function(value::Function {
                        name: value.interpolation().map_err(|message| {
                            EvalError::new(self.base + value_span(other).start, message)
                        })?,
                        args: Box::new(args),
                    }));
                }
            },
            FunctionName::SassQualifiedName(q) => return self.qualified_function(src, f, q),
            _ => {
                return Ok(Value::Raw(
                    src.get(f.span.start..f.span.end).unwrap_or("").to_string(),
                ));
            }
        };
        let lower = name.to_ascii_lowercase();

        // calc 系列内部不是 SassScript：只做插值。dart 只认 `calc` 与
        // 带厂商前缀的 `-webkit-calc`（`rem-calc` 之类的用户函数名不算）。
        if lower == "calc" || (lower.starts_with('-') && lower.ends_with("-calc")) {
            let raw = src.get(f.span.start..f.span.end).unwrap_or("");
            let inner = raw
                .find('(')
                .and_then(|start| raw.rfind(')').map(|end| (start + 1, end)))
                .map(|(s, e)| &raw[s..e])
                .unwrap_or("");
            // calc 内部是计算式，但 **SassScript 仍然生效**：`$var`、用户函数、
            // `#{}` 都要求值后内联（element-plus 的 `calc(getCssVar(...) - 1)`）。
            let inner = self.calc_script(inner, f.span.start + 1)?;
            // calc 的内容按计算式重新序列化：引号统一成 dart 的写法。
            let inner = value::collapse_ws(&value::requote_strings(&inner));
            // 常数化简：插值结果是不透明原子，旁边的数字仍可以合并。
            match fold_calc(&inner) {
                CalcFold::Number(number) => return Ok(Value::Number(number)),
                CalcFold::Text(text) => return Ok(Value::Raw(format!("{lower}({text})"))),
                CalcFold::Verbatim => {
                    return Ok(Value::Raw(format!(
                        "{lower}({})",
                        strip_interp_marks(&inner)
                    )));
                }
            }
        }
        // `min()` / `max()` / `clamp()` 优先按纯 CSS 数学函数解析（dart 的计算式语义）。
        // 现代 dart 只认这三个函数名本身：`-webkit-min` 是未知函数（原样透传）。
        if matches!(lower.as_str(), "min" | "max" | "clamp") {
            if let Some(value) = self.css_math_call(src, f, &lower)? {
                return Ok(value);
            }
        }
        // `progid:...` 是 IE filter 的原文写法：整体透传（空白保留），只规范化字符串引号。
        if lower.contains("progid") {
            let raw = src.get(f.span.start..f.span.end).unwrap_or("");
            let raw = self.interpolate(raw, f.span.start)?;
            return Ok(Value::Raw(value::normalize_raw(&value::requote_strings(
                &raw,
            ))));
        }
        // 其余原文参数（`alpha(opacity=65)`）：`=` 两侧不留空格（dart-sass 实测）。
        if value::has_raw_args(&f.args) {
            let raw = value::rewrite_raw(src, f.span);
            let raw = self.interpolate(&raw, f.span.start)?;
            return Ok(Value::Raw(value::strip_eq_spaces(&raw)));
        }

        // `if($cond, $a, $b)` 惰性求值。
        if normalize_name(&lower) == "if" {
            let groups = split_args(src, &f.args);
            if groups.len() == 3 {
                let cond = self.eval_value(src, groups[0])?;
                return self.eval_value(src, if cond.truthy() { groups[1] } else { groups[2] });
            }
        }

        // 需要求值器上下文的元函数（变量/函数存在性、一等函数）。
        match normalize_name(&lower).as_str() {
            "variable-exists" | "global-variable-exists" | "function-exists" | "mixin-exists" => {
                return self.meta_exists(&lower, src, &f.args, f.span.start);
            }
            "content-exists" => return self.content_exists(f.span.start),
            "get-function" => return self.get_function(src, &f.args, f.span.start),
            "call" => return self.call_first_class(src, f),
            "calc-name" | "calc-args" => {
                return self.calc_parts(&lower, src, &f.args, f.span.start);
            }
            _ => {}
        }

        // 用户函数优先于同名内置函数（dart 实测：`@function rgb()` 能盖掉内置 rgb）。
        if let Some(def) = self.find_function(&name) {
            let inv = self.eval_invocation(src, &f.args)?;
            return self.call_function(def, src, inv, f.span.start);
        }

        let inv = self.eval_invocation(src, &f.args)?;
        // 函数名同样 `-`/`_` 等价（`map_get` = `map-get`）；未知函数仍按原拼写透传。
        let keywords = inv
            .keywords
            .iter()
            .map(|(name, value, _)| (name.clone(), value.clone()))
            .collect::<Vec<_>>();
        // `@use "sass:xxx" as *` 的成员**优先于**全局内置函数：`@use "sass:color" as *`
        // 之后 `lighten()` 会报「不在 sass:color 模块里」（1.35.2 实测）。
        let member = normalize_name(&lower);
        if let Some(module) = self.star_member(&member) {
            return modules::call(module, &member, &inv.positional, &keywords)
                .map_err(|message| EvalError::new(self.base + f.span.start, message));
        }
        if let Some(value) = crate::scss::builtins::call(&member, &inv.positional, &keywords)
            .map_err(|message| EvalError::new(self.base + f.span.start, message))?
        {
            return Ok(value::with_original_name(value, &name, &lower));
        }
        let args = match inv.positional.len() {
            0 => Value::List(List {
                items: Vec::new(),
                sep: Sep::Comma,
                bracketed: false,
            }),
            1 => inv.positional.into_iter().next().expect("长度 1"),
            _ => Value::List(List {
                items: inv.positional,
                sep: Sep::Comma,
                bracketed: false,
            }),
        };
        Ok(Value::Function(value::Function {
            name,
            args: Box::new(args),
        }))
    }

    /// `meta.calc-name($calc)` / `meta.calc-args($calc)`（1.93.2）。
    ///
    /// 我们的 calc 家族是 `Value::Raw("calc(...)")`：`calc-name` 取函数名（带引号
    /// 字符串），`calc-args` 取实参——`calc` 整体算一个计算式，其余按顶层逗号拆开
    /// 并重新求值（这样 `min(1px, 2%)` 的 `1px` 仍是带单位的数）。
    fn calc_parts(
        &mut self,
        which: &str,
        src: &str,
        args: &[ComponentValue],
        at: usize,
    ) -> R<Value> {
        let inv = self.eval_invocation(src, args)?;
        let keywords = keywords_of(&inv);
        let base = self.base;
        let fail = |message: String| EvalError::new(base + at, message);
        let bound = Args::new(&["calc"], &inv.positional, &keywords).map_err(fail)?;
        let text = match bound.req(0).map_err(|m| EvalError::new(base + at, m))? {
            Value::Raw(text) => text.clone(),
            // `min()` 保留成不带引号的字符串；`clamp()` 是 CSS 函数透传。
            Value::Str(text) if !text.quoted => text.text.clone(),
            Value::Function(function) => {
                format!("{}({})", function.name, function.args.inspect())
            }
            other => {
                return Err(EvalError::new(
                    base + at,
                    format!("$calc: {} is not a calculation.", other.inspect()),
                ));
            }
        };
        let Some((name, inner)) = split_calc(&text) else {
            return Err(EvalError::new(
                base + at,
                format!("$calc: {text} is not a calculation."),
            ));
        };
        if which == "calc-name" {
            return Ok(Value::Str(SassStr::quoted(name)));
        }
        let parts = if name == "calc" {
            vec![Value::Raw(inner.trim().to_string())]
        } else {
            let mut values = Vec::new();
            for piece in split_top_level(&inner, ',') {
                values.push(self.eval_text(piece.trim(), at)?);
            }
            values
        };
        Ok(Value::List(List {
            items: parts,
            sep: Sep::Comma,
            bracketed: false,
        }))
    }

    /// `variable-exists` / `global-variable-exists` / `function-exists` / `mixin-exists`：
    /// 除 `variable-exists` 外都可带 `$module`（指向 `@use` 的命名空间）。
    fn meta_exists(
        &mut self,
        which: &str,
        src: &str,
        args: &[ComponentValue],
        at: usize,
    ) -> R<Value> {
        let inv = self.eval_invocation(src, args)?;
        let keywords = keywords_of(&inv);
        let params: &[&str] = if which == "variable-exists" {
            &["name"]
        } else {
            &["name", "module"]
        };
        let base = self.base;
        let check = |evaluator: &Self| -> Result<Value, String> {
            let bound = Args::new(params, &inv.positional, &keywords)?;
            let name = match bound.req(0)? {
                Value::Str(text) => text.text.clone(),
                other => return Err(format!("$name: {} is not a string.", other.inspect())),
            };
            let module = match bound.at(1).filter(|_| which != "variable-exists") {
                None => None,
                Some(Value::Null) => None,
                Some(Value::Str(text)) => Some(text.text.clone()),
                Some(other) => {
                    return Err(format!("$module: {} is not a string.", other.inspect()));
                }
            };
            let found = match (which, &module) {
                ("mixin-exists", Some(namespace)) => {
                    let Some(module) = evaluator.namespace_module(namespace) else {
                        return Err(format!(
                            "There is no module with the namespace \"{namespace}\"."
                        ));
                    };
                    match module {
                        ModuleRef::User(scope) => evaluator.export_mixin_name(scope, &name),
                        ModuleRef::Builtin(_) => false,
                    }
                }
                (_, Some(namespace)) => {
                    let Some(module) = evaluator.namespace_module(namespace) else {
                        return Err(format!(
                            "There is no module with the namespace \"{namespace}\"."
                        ));
                    };
                    match which {
                        "function-exists" => match module {
                            ModuleRef::Builtin(module) => module.has_member(&name),
                            ModuleRef::User(scope) => {
                                evaluator.export_function(scope, &name).is_some()
                            }
                        },
                        "global-variable-exists" => match module {
                            ModuleRef::Builtin(module) => module.variable(&name).is_some(),
                            ModuleRef::User(scope) => evaluator.export_var(scope, &name).is_some(),
                        },
                        _ => return Err("No argument named $module.".to_string()),
                    }
                }
                ("variable-exists", None) => evaluator.variable_exists(&name, false),
                ("global-variable-exists", None) => evaluator.variable_exists(&name, true),
                ("function-exists", None) => {
                    evaluator.find_function(&name).is_some()
                        || crate::scss::builtins::is_builtin(&normalize_name(&name))
                        || evaluator.star_member(&normalize_name(&name)).is_some()
                }
                ("mixin-exists", None) => {
                    let name = normalize_name(&name);
                    evaluator.mixins[evaluator.active].contains(&name)
                        || evaluator
                            .star_scopes()
                            .any(|scope| evaluator.export_mixin_name(scope, &name))
                }
                _ => unreachable!("元函数已穷举"),
            };
            Ok(Value::Bool(found))
        };
        check(self).map_err(|message| EvalError::new(base + at, message))
    }

    /// `meta.module-functions($module)` / `meta.module-variables($module)`。
    fn module_members(
        &mut self,
        which: &str,
        src: &str,
        args: &[ComponentValue],
        at: usize,
    ) -> R<Value> {
        let inv = self.eval_invocation(src, args)?;
        let keywords = keywords_of(&inv);
        let base = self.base;
        let result = (|| -> Result<Value, String> {
            let bound = Args::new(&["module"], &inv.positional, &keywords)?;
            let namespace = match bound.req(0)? {
                Value::Str(text) => text.text.clone(),
                other => return Err(format!("$module: {} is not a string.", other.inspect())),
            };
            let Some(module) = self.namespace_module(&namespace) else {
                return Err(format!(
                    "There is no module with the namespace \"{namespace}\"."
                ));
            };
            let entries = match (module, which) {
                (ModuleRef::Builtin(module), "module-functions") => module
                    .listed_functions()
                    .iter()
                    .map(|name| {
                        (
                            Value::Str(SassStr::quoted(*name)),
                            Value::FuncRef(value::FuncRef {
                                name: (*name).to_string(),
                                module: Some(module),
                                namespace: None,
                                css: false,
                            }),
                        )
                    })
                    .collect(),
                (ModuleRef::Builtin(module), _) => module
                    .variables()
                    .into_iter()
                    .map(|(name, value)| {
                        (
                            Value::Str(SassStr::quoted(name)),
                            Value::Number(Number::unitless(value)),
                        )
                    })
                    .collect(),
                (ModuleRef::User(scope), "module-functions") => self
                    .export_function_names(scope)
                    .into_iter()
                    .map(|name| {
                        (
                            Value::Str(SassStr::quoted(name.clone())),
                            Value::FuncRef(value::FuncRef {
                                name,
                                module: None,
                                namespace: Some(namespace.clone()),
                                css: false,
                            }),
                        )
                    })
                    .collect(),
                (ModuleRef::User(scope), _) => self
                    .export_var_names(scope)
                    .into_iter()
                    .map(|(name, value)| (Value::Str(SassStr::quoted(name)), value))
                    .collect(),
            };
            Ok(Value::Map(Map { entries }))
        })();
        result.map_err(|message| EvalError::new(base + at, message))
    }

    /// `content-exists()`：只能在 mixin 体里调用。
    fn content_exists(&mut self, at: usize) -> R<Value> {
        let (in_mixin, has_content) = self.mixin_context;
        if !in_mixin {
            return Err(EvalError::new(
                self.base + at,
                "content-exists() may only be called within a mixin.",
            ));
        }
        Ok(Value::Bool(has_content))
    }

    /// `get-function($name, $css: false, $module: null)`：名字在**取函数时**校验，
    /// 取不到就报 `Function not found: "x"`（1.35.2）。
    fn get_function(&mut self, src: &str, args: &[ComponentValue], at: usize) -> R<Value> {
        let inv = self.eval_invocation(src, args)?;
        let keywords = keywords_of(&inv);
        let base = self.base;
        let result = (|| -> Result<Value, String> {
            let bound = Args::new(&["name", "css", "module"], &inv.positional, &keywords)?;
            let name = match bound.req(0)? {
                Value::Str(text) => text.text.clone(),
                other => return Err(format!("$name: {} is not a string.", other.inspect())),
            };
            let css = bound.at(1).is_some_and(Value::truthy);
            let module = match bound.at(2) {
                None | Some(Value::Null) => None,
                Some(Value::Str(text)) => Some(text.text.clone()),
                Some(other) => {
                    return Err(format!("$module: {} is not a string.", other.inspect()));
                }
            };
            if let Some(namespace) = module {
                let Some(module) = self.namespace_module(&namespace) else {
                    return Err(format!(
                        "There is no module with the namespace \"{namespace}\"."
                    ));
                };
                let exists = match module {
                    ModuleRef::Builtin(module) => module.has_member(&name),
                    ModuleRef::User(scope) => self.export_function(scope, &name).is_some(),
                };
                if !exists {
                    return Err(format!(
                        "Function not found: {}",
                        value::quote_string(&name)
                    ));
                }
                return Ok(Value::FuncRef(value::FuncRef {
                    name,
                    module: match module {
                        ModuleRef::Builtin(module) => Some(module),
                        ModuleRef::User(_) => None,
                    },
                    namespace: match module {
                        ModuleRef::Builtin(_) => None,
                        ModuleRef::User(_) => Some(namespace),
                    },
                    css,
                }));
            }
            if !css
                && self.find_function(&name).is_none()
                && !crate::scss::builtins::is_builtin(&normalize_name(&name))
                && self.star_member(&normalize_name(&name)).is_none()
            {
                return Err(format!(
                    "Function not found: {}",
                    value::quote_string(&name)
                ));
            }
            Ok(Value::FuncRef(value::FuncRef {
                name,
                module: None,
                namespace: None,
                css,
            }))
        })();
        result.map_err(|message| EvalError::new(base + at, message))
    }

    /// `call($function, $args...)`：首个位置实参是 `get-function(...)` 的结果，
    /// 其余实参照常绑定（用户函数 → 内置 → 未知时按 CSS 函数透传）。
    fn call_first_class(&mut self, src: &str, f: &Function) -> R<Value> {
        let mut inv = self.eval_invocation(src, &f.args)?;
        if inv.positional.is_empty() {
            return Err(EvalError::new(
                self.base + f.span.start,
                "Missing argument $function.",
            ));
        }
        let target = inv.positional.remove(0);
        let reference = match target {
            Value::FuncRef(reference) => reference,
            Value::Str(text) => value::FuncRef {
                name: text.text,
                module: None,
                namespace: None,
                css: false,
            },
            other => {
                return Err(EvalError::new(
                    self.base + f.span.start,
                    format!(
                        "$function: {} is not a function reference.",
                        other.inspect()
                    ),
                ));
            }
        };
        let keywords = keywords_of(&inv);
        let at = self.base + f.span.start;
        if reference.css {
            return Ok(css_function_call(&reference.name, inv.positional));
        }
        if let Some(module) = reference.module {
            return modules::call(
                module,
                &normalize_name(&reference.name),
                &inv.positional,
                &keywords,
            )
            .map_err(|message| EvalError::new(at, message));
        }
        if let Some(namespace) = &reference.namespace {
            let Some(def) = self.find_module_function(namespace, &reference.name) else {
                return Err(EvalError::new(
                    at,
                    format!(
                        "Function not found: {}",
                        value::quote_string(&reference.name)
                    ),
                ));
            };
            return self.call_function(def, src, inv, f.span.start);
        }
        if let Some(def) = self.find_function(&reference.name) {
            return self.call_function(def, src, inv, f.span.start);
        }
        let name = normalize_name(&reference.name);
        if let Some(module) = self.star_member(&name) {
            return modules::call(module, &name, &inv.positional, &keywords)
                .map_err(|message| EvalError::new(at, message));
        }
        if let Some(value) = crate::scss::builtins::call(&name, &inv.positional, &keywords)
            .map_err(|message| EvalError::new(at, message))?
        {
            return Ok(value);
        }
        Err(EvalError::new(
            at,
            format!(
                "Function not found: {}",
                value::quote_string(&reference.name)
            ),
        ))
    }

    /// `variable-exists()` / `global-variable-exists()`：`global_only` 只看全局帧。
    /// `as *` 引入的变量也算（dart 实测 `global-variable-exists()` 为 true）。
    fn variable_exists(&self, name: &str, global_only: bool) -> bool {
        let name = normalize_name(name);
        let scope = &self.scopes[self.active];
        let own = if global_only {
            scope
                .frames
                .first()
                .is_some_and(|frame| frame.vars.contains_key(&name))
        } else {
            scope
                .frames
                .iter()
                .rev()
                .any(|f| f.vars.contains_key(&name))
        };
        own || self.star_has_variable(&name)
    }

    fn star_has_variable(&self, name: &str) -> bool {
        self.star[self.active]
            .iter()
            .rev()
            .any(|module| match module {
                StarModule::User(scope) => self.export_var(*scope, name).is_some(),
                StarModule::Builtin(module) => module.variable(name).is_some(),
            })
    }

    /// `ns.fn(...)`：用户模块查函数表，内置模块走 `modules` 分派；两者都没有时
    /// / 不是模块函数时都按 dart 报错。
    fn qualified_function(&mut self, src: &str, f: &Function, q: &SassQualifiedName) -> R<Value> {
        let raw = || Value::Raw(src.get(f.span.start..f.span.end).unwrap_or("").to_string());
        let SassModuleMemberName::Ident(member) = &q.member else {
            return Ok(raw());
        };
        if is_private(member.name) {
            return Err(EvalError::new(
                self.base + q.span.start,
                "Private members can't be accessed from outside their modules.",
            ));
        }
        let Some(module) = self.namespace_module(q.module.name) else {
            return Err(EvalError::new(
                self.base + q.span.start,
                format!(
                    "There is no module with the namespace \"{}\".",
                    q.module.name
                ),
            ));
        };
        let at = self.base + f.span.start;
        match module {
            ModuleRef::User(scope) => {
                let def = self.export_function(scope, member.name);
                let Some(def) = def else {
                    return Err(EvalError::new(at, "Undefined function."));
                };
                let inv = self.eval_invocation(src, &f.args)?;
                self.call_function(def, src, inv, f.span.start)
            }
            ModuleRef::Builtin(Builtin::Meta) => {
                let member = normalize_name(member.name);
                match member.as_str() {
                    "variable-exists"
                    | "global-variable-exists"
                    | "function-exists"
                    | "mixin-exists" => self.meta_exists(&member, src, &f.args, f.span.start),
                    "content-exists" => self.content_exists(f.span.start),
                    "get-function" => self.get_function(src, &f.args, f.span.start),
                    "call" => self.call_first_class(src, f),
                    "module-functions" | "module-variables" => {
                        self.module_members(&member, src, &f.args, f.span.start)
                    }
                    "calc-name" | "calc-args" => {
                        self.calc_parts(&member, src, &f.args, f.span.start)
                    }
                    _ => {
                        let inv = self.eval_invocation(src, &f.args)?;
                        let keywords = keywords_of(&inv);
                        modules::call(Builtin::Meta, &member, &inv.positional, &keywords)
                            .map_err(|message| EvalError::new(at, message))
                    }
                }
            }
            ModuleRef::Builtin(module) => {
                // 1.35.2：模块里根本没这个名字时报裸 `Undefined function.`；
                // 名字在成员表里但不可调用（`color.lighten`）才报模块专属文案。
                if !module.has_member(&normalize_name(member.name)) {
                    return Err(EvalError::new(at, "Undefined function."));
                }
                let inv = self.eval_invocation(src, &f.args)?;
                let keywords = keywords_of(&inv);
                modules::call(
                    module,
                    &normalize_name(member.name),
                    &inv.positional,
                    &keywords,
                )
                .map_err(|message| EvalError::new(at, message))
            }
        }
    }

    /// `@include` / `@content` 的实参：位置实参 + 关键字实参（按源码顺序求值）。
    /// `...` 展开 list 为位置实参、map 为关键字实参（dart 实测：其它值当作单个位置实参）。
    pub(crate) fn eval_invocation(&mut self, src: &str, args: &[ComponentValue]) -> R<Invocation> {
        let mut out = Invocation::default();
        let mut saw_keyword = false;
        // 先按顶层逗号分组：一组就是一个实参（组内可能是空格分隔的 list）。
        for group in split_args(src, args) {
            match group {
                [] => {}
                [ComponentValue::SassKeywordArgument(k)] => {
                    let name = normalize_name(k.name.name.name);
                    if out.keywords.iter().any(|(n, _, _)| n == &name) {
                        return Err(EvalError::new(k.name.span.start, "Duplicate argument."));
                    }
                    let value = self.eval_comp(src, &k.value)?;
                    out.keywords.push((name, value, k.name.span));
                    saw_keyword = true;
                }
                [ComponentValue::SassArbitraryArgument(a)] => {
                    // `$x...`：list → 位置实参，map / ArgList → 关键字实参，其余当单个位置实参
                    // （dart 实测：`null...` 就是一个 null 位置实参）。
                    let add_keyword =
                        |out: &mut Invocation, name: String, value: Value, span: Span| {
                            if out.keywords.iter().any(|(n, _, _)| n == &name) {
                                return Err(EvalError::new(span.start, "Duplicate argument."));
                            }
                            out.keywords.push((name, value, span));
                            Ok(())
                        };
                    match self.eval_comp(src, &a.value)? {
                        Value::List(list) if !list.bracketed => out.positional.extend(list.items),
                        Value::ArgList(list) => {
                            // 展开会读关键字 map：dart 视为「已访问」，因此不再报多余命名实参。
                            list.keywords_accessed.set(true);
                            out.positional.extend(list.positional);
                            for (name, value) in list.keywords {
                                add_keyword(&mut out, name, value, a.span)?;
                            }
                            saw_keyword |= !out.keywords.is_empty();
                        }
                        Value::Map(map) => {
                            for (key, value) in map.entries {
                                let name = key
                                    .interpolation()
                                    .map_err(|message| EvalError::new(a.span.start, message))?;
                                add_keyword(&mut out, normalize_name(&name), value, a.span)?;
                            }
                            saw_keyword = true;
                        }
                        other => out.positional.push(other),
                    }
                }
                other => {
                    if saw_keyword {
                        return Err(EvalError::new(
                            other[0].span().start,
                            "Positional arguments must come before keyword arguments.",
                        ));
                    }
                    out.positional.push(self.eval_value(src, other)?);
                }
            }
        }
        Ok(out)
    }

    /// 实参：按逗号分组求值（每组是一个表达式），`...` 展开与 `@include` 同一套。
    fn eval_args(
        &mut self,
        src: &str,
        args: &[ComponentValue],
    ) -> R<(Value, Vec<(String, Value)>)> {
        let mut positional = Vec::new();
        let mut keywords = Vec::new();
        for group in split_args(src, args) {
            match group {
                [ComponentValue::SassKeywordArgument(k)] => keywords.push((
                    normalize_name(k.name.name.name),
                    self.eval_comp(src, &k.value)?,
                )),
                [ComponentValue::SassArbitraryArgument(_)] => {
                    let inv = self.eval_invocation(src, group)?;
                    positional.extend(inv.positional);
                    keywords.extend(
                        inv.keywords
                            .into_iter()
                            .map(|(name, value, _)| (name, value)),
                    );
                }
                other => positional.push(self.eval_value(src, other)?),
            }
        }
        let value = match positional.len() {
            0 => Value::List(List {
                items: Vec::new(),
                sep: Sep::Comma,
                bracketed: false,
            }),
            1 => positional.into_iter().next().expect("长度 1"),
            _ => Value::List(List {
                items: positional,
                sep: Sep::Comma,
                bracketed: false,
            }),
        };
        Ok((value, keywords))
    }

    // -----------------------------------------------------------------------
    // 用户函数（@function / @return）
    // -----------------------------------------------------------------------

    /// 实参绑定（dart `ArgumentDeclaration.verify` + 绑定顺序）：
    /// 位置实参按序填形参 → 关键字按名补位 → 默认值按声明顺序求值（可引用前面的形参）；
    /// `$rest...` 收集多余位置实参（dart 实测：rest 不吃关键字实参）。
    /// 有 `$rest` 时多余的命名实参先装进 ArgList，等体跑完再决定要不要报错
    /// （dart 实测：只要有人读过关键字 map（`keywords($args)`）就不报）。
    pub(crate) fn bind(
        &mut self,
        src: &str,
        params: &[Param<'a>],
        rest: Option<&str>,
        inv: Invocation,
        at: usize,
    ) -> R<Option<RestCheck>> {
        let name_eq = |a: &str, b: &str| normalize_name(a) == normalize_name(b);
        for (i, param) in params.iter().enumerate() {
            let matched = inv
                .keywords
                .iter()
                .position(|(name, _, _)| name_eq(name, param.name));
            if i < inv.positional.len() {
                if matched.is_some() {
                    return Err(EvalError::new(
                        at,
                        format!(
                            "Argument ${} was passed both by position and by name.",
                            param.name
                        ),
                    ));
                }
            } else if matched.is_none() && param.default.is_none() {
                return Err(EvalError::new(
                    at,
                    format!("Missing argument ${}.", param.name),
                ));
            }
        }
        if rest.is_none() && inv.positional.len() > params.len() {
            let allowed = params.len();
            let passed = inv.positional.len();
            return Err(EvalError::new(
                at,
                format!(
                    "Only {allowed} {} allowed, but {passed} {} passed.",
                    if allowed == 1 {
                        "argument"
                    } else {
                        "arguments"
                    },
                    if passed == 1 { "was" } else { "were" },
                ),
            ));
        }
        let leftover = inv
            .keywords
            .iter()
            .filter(|(name, _, _)| !params.iter().any(|p| name_eq(p.name, name)))
            .map(|(name, _, _)| name.clone())
            .collect::<Vec<_>>();
        if rest.is_none() {
            if let Some(name) = leftover.first() {
                return Err(EvalError::new(at, format!("No argument named ${name}.")));
            }
        }
        for (i, param) in params.iter().enumerate() {
            let value = if let Some(value) = inv.positional.get(i) {
                value.clone()
            } else if let Some((_, value, _)) = inv
                .keywords
                .iter()
                .find(|(name, _, _)| name_eq(name, param.name))
            {
                value.clone()
            } else {
                let default = param.default.expect("已校验存在默认值");
                self.eval_component(src, default)?
            };
            self.declare_local(param.name, value);
        }
        let Some(rest) = rest else {
            return Ok(None);
        };
        let accessed = Rc::new(Cell::new(false));
        let items = inv.positional[params.len().min(inv.positional.len())..].to_vec();
        let keywords = inv
            .keywords
            .iter()
            .filter(|(name, _, _)| !params.iter().any(|p| name_eq(p.name, name)))
            .map(|(name, value, _)| (name.clone(), value.clone()))
            .collect::<Vec<_>>();
        self.declare_local(
            rest,
            Value::ArgList(value::ArgList {
                positional: items,
                keywords,
                sep: None,
                keywords_accessed: Rc::clone(&accessed),
            }),
        );
        Ok(Some(RestCheck {
            names: leftover,
            accessed,
        }))
    }

    /// 多余的命名实参：体跑完再报（体里读过关键字 map 就不报）。
    pub(crate) fn check_keywords(&self, check: RestCheck, at: usize) -> R<()> {
        if check.names.is_empty() || check.accessed.get() {
            return Ok(());
        }
        let names = check
            .names
            .iter()
            .map(|name| format!("${name}"))
            .collect::<Vec<_>>()
            .join(" or ");
        Err(EvalError::new(
            at,
            if check.names.len() == 1 {
                format!("No argument named {names}.")
            } else {
                format!("No arguments named {names}.")
            },
        ))
    }

    /// 调用用户函数：帧栈截到定义深度（看不到调用方局部变量），体里新建的局部变量
    /// 随调用丢弃；`!global` 仍然写真正的全局帧（dart 实测）。
    pub(crate) fn call_function(
        &mut self,
        def: FunctionDef<'a>,
        src: &str,
        inv: Invocation,
        at: usize,
    ) -> R<Value> {
        if self.call_depth >= MAX_CALL_DEPTH {
            return Err(EvalError::new(
                at,
                format!("Function nesting depth exceeded ({MAX_CALL_DEPTH})."),
            ));
        }
        let caller = self.active;
        self.enter(def.scope);
        let tail = if def.scope == caller {
            self.truncate_frames(def.scope, def.depth)
        } else {
            Vec::new()
        };
        self.push_frame(false);
        let mut rest_check = None;
        // 函数体（含默认值表达式）的 span 来自真实源码，只有在 base = 0 时才对得上。
        let body_src = self.real.unwrap_or(src);
        let saved_base = self.base;
        self.base = 0;
        let result = match self.bind(body_src, &def.params, def.rest, inv, at) {
            Err(e) => Err(e),
            Ok(check) => {
                rest_check = check;
                self.call_depth += 1;
                let flow = self.run_body(body_src, def.body);
                self.call_depth -= 1;
                self.enter(def.scope);
                match flow {
                    Ok(Flow::Return(value)) => Ok(value),
                    Ok(Flow::Normal) => {
                        Err(EvalError::new(def.at, "Function finished without @return."))
                    }
                    Err(e) => Err(e),
                }
            }
        };
        self.base = saved_base;
        self.pop_frame();
        self.append_frames(def.scope, tail);
        self.enter(caller);
        let value = result?;
        match rest_check {
            Some(check) => {
                self.check_keywords(check, at)?;
                Ok(value)
            }
            None => Ok(value),
        }
    }

    /// function 体：只有变量 / `@if` / `@each` / `@for` / `@while` / `@return` /
    /// `@debug` / `@warn` / `@error`（其余由静态容器校验拦下）。
    fn run_body(&mut self, src: &str, stmts: &'a [Statement<'a>]) -> R<Flow> {
        for st in stmts {
            match st {
                Statement::SassVariableDeclaration(var) => self.function_var(src, var)?,
                Statement::SassIfAtRule(rule) => {
                    if let Flow::Return(value) = self.function_if(src, rule)? {
                        return Ok(Flow::Return(value));
                    }
                }
                Statement::AtRule(at) => {
                    if let Flow::Return(value) = self.function_at(src, at)? {
                        return Ok(Flow::Return(value));
                    }
                }
                Statement::Declaration(decl) => {
                    return Err(EvalError::new(
                        decl.span.start,
                        "@function rules may not contain declarations.",
                    ));
                }
                Statement::QualifiedRule(rule) => {
                    return Err(EvalError::new(
                        rule.span.start,
                        "@function rules may not contain style rules.",
                    ));
                }
                other => {
                    return Err(EvalError::new(
                        other.span().start,
                        "This at-rule is not allowed here.",
                    ));
                }
            }
        }
        Ok(Flow::Normal)
    }

    fn function_var(&mut self, src: &str, var: &SassVariableDeclaration<'a>) -> R<()> {
        let value = self.eval_value(src, std::slice::from_ref(&var.value))?;
        let has = |name: &str| {
            var.flags
                .iter()
                .any(|f| f.keyword.name.eq_ignore_ascii_case(name))
        };
        self.define(var.name.name.name, value, has("default"), has("global"));
        Ok(())
    }

    fn function_if(&mut self, src: &str, rule: &'a SassIfAtRule<'a>) -> R<Flow> {
        let mut chosen: Option<&'a SimpleBlock<'a>> = None;
        for clause in std::iter::once(&rule.if_clause).chain(rule.else_if_clauses.iter()) {
            if self.eval_comp(src, &clause.condition)?.truthy() {
                chosen = Some(&clause.block);
                break;
            }
        }
        let Some(block) = chosen.or(rule.else_clause.as_ref()) else {
            return Ok(Flow::Normal);
        };
        self.push_frame(true);
        let result = self.run_body(src, &block.statements);
        self.pop_frame();
        result
    }

    fn function_at(&mut self, src: &str, at: &'a AtRule<'a>) -> R<Flow> {
        let lower = at.name.name.to_ascii_lowercase();
        match lower.as_str() {
            "return" => {
                let Some(AtRulePrelude::SassExpr(expr)) = &at.prelude else {
                    return Err(EvalError::new(at.span.start, "Expected expression."));
                };
                Ok(Flow::Return(self.eval_comp(src, expr)?))
            }
            "debug" | "warn" => {
                if let Some(AtRulePrelude::SassExpr(expr)) = &at.prelude {
                    self.eval_comp(src, expr)?;
                }
                Ok(Flow::Normal)
            }
            "error" => {
                let message = match &at.prelude {
                    Some(AtRulePrelude::SassExpr(expr)) => self.eval_comp(src, expr)?.inspect(),
                    _ => String::new(),
                };
                Err(EvalError::new(at.span.start, message))
            }
            "each" | "for" | "while" => self.function_loop(src, at),
            _ => Err(EvalError::new(
                at.span.start,
                "This at-rule is not allowed here.",
            )),
        }
    }

    fn function_loop(&mut self, src: &str, at: &'a AtRule<'a>) -> R<Flow> {
        let Some(block) = &at.block else {
            return Ok(Flow::Normal);
        };
        match &at.prelude {
            Some(AtRulePrelude::SassEach(each)) => {
                let rows = self.each_rows(src, &each.expr, each.bindings.len())?;
                self.push_frame(true);
                let mut result = Ok(Flow::Normal);
                for row in rows {
                    for (var, value) in each.bindings.iter().zip(row) {
                        self.declare_local(var.name.name, value);
                    }
                    match self.run_body(src, &block.statements) {
                        Ok(Flow::Normal) => {}
                        Ok(flow) => {
                            result = Ok(flow);
                            break;
                        }
                        Err(e) => {
                            result = Err(e);
                            break;
                        }
                    }
                }
                self.pop_frame();
                result
            }
            Some(AtRulePrelude::SassFor(spec)) => {
                let inclusive = matches!(spec.boundary.kind, SassForBoundaryKind::Inclusive);
                let values = self.for_values(src, &spec.start, &spec.end, inclusive)?;
                self.push_frame(true);
                let mut result = Ok(Flow::Normal);
                for value in values {
                    self.declare_local(spec.binding.name.name, value);
                    match self.run_body(src, &block.statements) {
                        Ok(Flow::Normal) => {}
                        Ok(flow) => {
                            result = Ok(flow);
                            break;
                        }
                        Err(e) => {
                            result = Err(e);
                            break;
                        }
                    }
                }
                self.pop_frame();
                result
            }
            Some(AtRulePrelude::SassExpr(cond)) => {
                self.push_frame(true);
                let mut result = Ok(Flow::Normal);
                loop {
                    if !self.eval_comp(src, cond)?.truthy() {
                        break;
                    }
                    match self.run_body(src, &block.statements) {
                        Ok(Flow::Normal) => {}
                        Ok(flow) => {
                            result = Ok(flow);
                            break;
                        }
                        Err(e) => {
                            result = Err(e);
                            break;
                        }
                    }
                }
                self.pop_frame();
                result
            }
            _ => Ok(Flow::Normal),
        }
    }

    // -----------------------------------------------------------------------
    // 循环迭代协议（Converter 与 function 体共用）
    // -----------------------------------------------------------------------

    /// `@each`：表达式 → 每次迭代的绑定值。null → 零次；map → 每项是 `(k v)` 对；
    /// 多变量按元素解构，缺位补 `null`（dart 实测），多出的元素忽略。
    pub(crate) fn each_rows(
        &mut self,
        src: &str,
        expr: &ComponentValue,
        count: usize,
    ) -> R<Vec<Vec<Value>>> {
        let value = self.eval_comp(src, expr)?;
        let Some(items) = as_list(&value) else {
            return Ok(Vec::new());
        };
        let mut rows = Vec::with_capacity(items.len());
        for item in items {
            let item = item.without_slash();
            if count == 1 {
                rows.push(vec![item]);
                continue;
            }
            let fields = as_list(&item).unwrap_or_else(|| vec![item.clone()]);
            let mut row = Vec::with_capacity(count);
            for i in 0..count {
                row.push(fields.get(i).cloned().unwrap_or(Value::Null));
            }
            rows.push(row);
        }
        Ok(rows)
    }

    /// `@for`：`from`/`to` 求值成整数后按方向生成序列（dart 实测：下界大于上界时倒序，
    /// 不是空循环）。循环变量的单位取 `from` 的写法，`to` 会先换算到同一单位。
    pub(crate) fn for_values(
        &mut self,
        src: &str,
        start: &ComponentValue,
        end: &ComponentValue,
        inclusive: bool,
    ) -> R<Vec<Value>> {
        let from = self.eval_number(src, start)?;
        let to = self.eval_number(src, end)?;
        let at = end.span().start;
        let to_value = coerce_for(&from, &to, at)?;
        let from_int = int_value(&from, from.value, start.span().start)?;
        let to_int = int_value(&to, to_value, at)?;
        let direction = if from_int > to_int { -1 } else { 1 };
        let last = if inclusive {
            to_int
        } else {
            to_int - direction
        };
        let mut values = Vec::new();
        let mut i = from_int;
        while i != last + direction {
            values.push(Value::Number(Number {
                value: i as f64,
                numer: from.numer.clone(),
                denom: from.denom.clone(),
                slash: None,
            }));
            i += direction;
        }
        Ok(values)
    }

    fn eval_number(&mut self, src: &str, comp: &ComponentValue) -> R<Number> {
        let at = comp.span().start;
        match self.eval_comp(src, comp)? {
            Value::Number(n) => Ok(n),
            other => Err(EvalError::new(
                at,
                format!("{} is not a number.", other.inspect()),
            )),
        }
    }
}

/// `@each` / 解构用：null → None；map → `(k v)` 对的 list；单个值 → `[自身]`。
/// 关键字实参 → `modules` 需要的形态。
fn keywords_of(inv: &Invocation) -> Vec<(String, Value)> {
    inv.keywords
        .iter()
        .map(|(name, value, _)| (name.clone(), value.clone()))
        .collect()
}

/// 用 `$args...` 调一个纯 CSS 函数：只有一个实参时直接用它（`bar(1px)`）。
fn css_function_call(name: &str, positional: Vec<Value>) -> Value {
    let args = match positional.len() {
        0 => Value::List(List {
            items: Vec::new(),
            sep: Sep::Comma,
            bracketed: false,
        }),
        1 => positional.into_iter().next().expect("长度 1"),
        _ => Value::List(List {
            items: positional,
            sep: Sep::Comma,
            bracketed: false,
        }),
    };
    Value::Function(value::Function {
        name: name.to_string(),
        args: Box::new(args),
    })
}

fn as_list(value: &Value) -> Option<Vec<Value>> {
    match value {
        Value::Null => None,
        Value::List(list) => Some(list.items.clone()),
        Value::ArgList(args) => Some(args.positional.clone()),
        Value::Map(map) => Some(
            map.entries
                .iter()
                .map(|(k, v)| {
                    Value::List(List {
                        items: vec![k.clone(), v.clone()],
                        sep: Sep::Space,
                        bracketed: false,
                    })
                })
                .collect(),
        ),
        other => Some(vec![other.clone()]),
    }
}

/// `@for` 的边界必须是整数（dart `assertInt` 文案）。
fn int_value(number: &Number, value: f64, at: usize) -> R<i64> {
    if (value - value.round()).abs() > value::EPSILON {
        return Err(EvalError::new(
            at,
            format!(
                "{} is not an int.",
                Number {
                    value,
                    numer: number.numer.clone(),
                    denom: number.denom.clone(),
                    slash: None,
                }
                .format()
            ),
        ));
    }
    Ok(value.round() as i64)
}

/// 把 `to` 换算到 `from` 的单位（coerceUnitless）；类型不匹配时给 dart 的
/// `Expected 3em to have a length unit (in, cm, pc, mm, q, pt, px).` 文案。
fn coerce_for(from: &Number, to: &Number, at: usize) -> R<f64> {
    if from.numer == to.numer && from.denom == to.denom {
        return Ok(to.value);
    }
    if from.is_unitless() || to.is_unitless() {
        return Ok(to.value);
    }
    if from.is_simple() && to.is_simple() {
        if let Some(factor) = value::convert_factor_public(to.unit(), from.unit()) {
            return Ok(to.value * factor);
        }
    }
    let (kind, names) = value::unit_kind_names(from.unit())
        .map(|(kind, names)| (kind, names.join(", ")))
        .unwrap_or(("compatible", String::new()));
    Err(EvalError::new(
        at,
        format!("Expected {} to have a {kind} unit ({names}).", to.format()),
    ))
}

fn value_span(ident: &InterpolableIdent) -> Span {
    ident_span(ident)
}

fn ident_span(ident: &InterpolableIdent) -> Span {
    match ident {
        InterpolableIdent::Literal(i) => i.span,
        InterpolableIdent::SassInterpolated(s) => s.span,
        InterpolableIdent::LessInterpolated(s) => s.span,
        InterpolableIdent::Placeholder(p) => p.span,
    }
}

fn str_span(s: &InterpolableStr) -> Span {
    match s {
        InterpolableStr::Literal(lit) => lit.span,
        InterpolableStr::SassInterpolated(i) => i.span,
        InterpolableStr::LessInterpolated(i) => i.span,
    }
}

/// 单位里出现 `-数字` 时切成两个数字（`10px-5px` → `10px - 5px`）。
fn unit_split_point(unit: &str) -> Option<usize> {
    let bytes = unit.as_bytes();
    (1..bytes.len()).find(|&i| bytes[i] == b'-' && bytes.get(i + 1).is_some_and(u8::is_ascii_digit))
}

/// 顶层逗号切分实参（组件层已经去掉嵌套括号）。
/// 按顶层逗号切分实参。oxc 的 `Function` 实参里逗号是独立组件，而 `@include` 会把
/// 逗号从 `args` 里去掉、只留 span 间隙——所以两处都要看：显式逗号组件，或相邻组件
/// 源码间隙里的 `,`。
fn split_args<'a, 'b>(src: &str, args: &'b [ComponentValue<'a>]) -> Vec<&'b [ComponentValue<'a>]> {
    let mut groups = Vec::new();
    let mut start = 0usize;
    for (i, comp) in args.iter().enumerate() {
        let comma =
            matches!(comp, ComponentValue::Delimiter(d) if matches!(d.kind, DelimiterKind::Comma));
        let gap_comma = i > start
            && src
                .get(args[i - 1].span().end..comp.span().start)
                .is_some_and(|gap| gap.contains(','));
        if comma {
            groups.push(&args[start..i]);
            start = i + 1;
        } else if gap_comma {
            // 逗号只存在于 span 间隙（`@include` 的实参），当前组件属于下一组。
            groups.push(&args[start..i]);
            start = i;
        }
    }
    groups.push(&args[start..]);
    groups
}

// ---------------------------------------------------------------------------
// token 摊平 + 优先级解析
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum OpKind<'b> {
    Ast(&'b SassBinaryOperatorKind, &'b Span),
    /// 组件序列里的裸 `/`（oxc 不把它折成二元运算）。
    Slash(Span),
}

impl OpKind<'_> {
    fn precedence(&self) -> u8 {
        use SassBinaryOperatorKind::*;
        match self {
            OpKind::Slash(_) => 6,
            OpKind::Ast(kind, _) => match kind {
                Or => 1,
                And => 2,
                EqualsEquals | ExclamationEquals => 3,
                GreaterThan | GreaterThanOrEqual | LessThan | LessThanOrEqual => 4,
                Plus | Minus => 5,
                Multiply | Division | Modulo => 6,
            },
        }
    }

    fn is_division(&self) -> bool {
        match self {
            OpKind::Slash(_) => true,
            OpKind::Ast(kind, _) => matches!(kind, SassBinaryOperatorKind::Division),
        }
    }

    fn span(&self) -> Span {
        match self {
            OpKind::Slash(span) => *span,
            OpKind::Ast(_, span) => **span,
        }
    }

    fn ast_kind(&self) -> &SassBinaryOperatorKind {
        match self {
            OpKind::Slash(_) => &SassBinaryOperatorKind::Division,
            OpKind::Ast(kind, _) => kind,
        }
    }
}

enum Tok<'a, 'b> {
    Val(&'b ComponentValue<'a>),
    Op(OpKind<'b>),
    Prefix(&'b SassUnaryOperator),
    Comma,
}

enum ExprKind<'a, 'b> {
    Val(&'b ComponentValue<'a>),
    Unary(&'b SassUnaryOperator, Box<ExprKind<'a, 'b>>),
    Binary(OpKind<'b>, Box<ExprKind<'a, 'b>>, Box<ExprKind<'a, 'b>>),
    List(Vec<ExprKind<'a, 'b>>, Sep, bool),
}

impl ExprKind<'_, '_> {
    /// 表达式起点（dart 的运算符错误都指向整个运算表达式的开头）。
    fn start(&self) -> usize {
        match self {
            ExprKind::Val(comp) => comp.span().start,
            ExprKind::Unary(op, _) => op.span.start,
            ExprKind::Binary(op, _, _) => op.span().start,
            ExprKind::List(items, _, _) => items.first().map_or(0, ExprKind::start),
        }
    }

    /// dart-sass 的 allowSlash：整棵子树只由 `/` 与字面数字组成。
    fn slash_ok(&self) -> bool {
        match self {
            ExprKind::Val(comp) => matches!(
                comp,
                ComponentValue::Number(_)
                    | ComponentValue::Dimension(_)
                    | ComponentValue::Percentage(_)
                    | ComponentValue::Ratio(_)
            ),
            ExprKind::Unary(op, inner) => {
                matches!(
                    op.kind,
                    SassUnaryOperatorKind::Plus | SassUnaryOperatorKind::Minus
                ) && inner.slash_ok()
            }
            ExprKind::Binary(op, left, right) => {
                op.is_division() && left.slash_ok() && right.slash_ok()
            }
            ExprKind::List(..) => false,
        }
    }
}

fn flatten<'a, 'b>(comp: &'b ComponentValue<'a>, out: &mut Vec<Tok<'a, 'b>>) {
    match comp {
        ComponentValue::SassBinaryExpression(b) => {
            flatten(&b.left, out);
            out.push(Tok::Op(OpKind::Ast(&b.op.kind, &b.op.span)));
            flatten(&b.right, out);
        }
        ComponentValue::SassUnaryExpression(u) => {
            out.push(Tok::Prefix(&u.op));
            flatten(&u.expr, out);
        }
        ComponentValue::Delimiter(d) if matches!(d.kind, DelimiterKind::Comma) => {
            out.push(Tok::Comma)
        }
        ComponentValue::Delimiter(d) if matches!(d.kind, DelimiterKind::Solidus) => {
            out.push(Tok::Op(OpKind::Slash(d.span)))
        }
        other => out.push(Tok::Val(other)),
    }
}

pub(crate) struct ParseError {
    pub(crate) at: usize,
    pub(crate) message: String,
}

struct ExprParser<'a, 'b> {
    toks: Vec<Tok<'a, 'b>>,
    pos: usize,
}

impl<'a, 'b> ExprParser<'a, 'b> {
    fn peek(&self) -> Option<&Tok<'a, 'b>> {
        self.toks.get(self.pos)
    }

    fn span_of(&self, tok: &Tok<'a, 'b>) -> usize {
        match tok {
            Tok::Val(comp) => comp.span().start,
            Tok::Op(op) => op.span().start,
            Tok::Prefix(op) => op.span.start,
            Tok::Comma => 0,
        }
    }

    fn parse_list(&mut self) -> Result<ExprKind<'a, 'b>, ParseError> {
        let mut items = Vec::new();
        let mut comma = false;
        let mut trailing = false;
        loop {
            match self.peek() {
                None => break,
                Some(Tok::Comma) => {
                    comma = true;
                    self.pos += 1;
                    trailing = true;
                    continue;
                }
                _ => {}
            }
            trailing = false;
            items.push(self.parse_space()?);
        }
        if comma {
            if items.len() == 1 && trailing {
                return Ok(ExprKind::List(items, Sep::Comma, false));
            }
            match items.len() {
                0 => Ok(ExprKind::List(Vec::new(), Sep::Comma, false)),
                1 => Ok(items.pop().expect("长度 1")),
                _ => Ok(ExprKind::List(items, Sep::Comma, false)),
            }
        } else {
            match items.len() {
                0 => Ok(ExprKind::List(Vec::new(), Sep::Space, false)),
                1 => Ok(items.pop().expect("长度 1")),
                _ => Ok(ExprKind::List(items, Sep::Space, false)),
            }
        }
    }

    /// 空格分隔的一段：连续的二元表达式。
    fn parse_space(&mut self) -> Result<ExprKind<'a, 'b>, ParseError> {
        let mut items = vec![self.parse_binary(0)?];
        while matches!(self.peek(), Some(Tok::Val(_)) | Some(Tok::Prefix(_))) {
            items.push(self.parse_binary(0)?);
        }
        if items.len() == 1 {
            return Ok(items.pop().expect("长度 1"));
        }
        Ok(ExprKind::List(items, Sep::Space, false))
    }

    fn parse_binary(&mut self, min: u8) -> Result<ExprKind<'a, 'b>, ParseError> {
        let mut left = self.parse_prefix()?;
        while let Some(Tok::Op(op)) = self.peek() {
            let op = *op;
            let prec = op.precedence();
            if prec < min {
                break;
            }
            self.pos += 1;
            let right = self.parse_binary(prec + 1)?;
            left = ExprKind::Binary(op, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_prefix(&mut self) -> Result<ExprKind<'a, 'b>, ParseError> {
        match self.peek() {
            Some(Tok::Prefix(op)) => {
                let op = *op;
                self.pos += 1;
                let inner = self.parse_prefix()?;
                Ok(ExprKind::Unary(op, Box::new(inner)))
            }
            Some(Tok::Val(comp)) => {
                let comp = *comp;
                self.pos += 1;
                Ok(ExprKind::Val(comp))
            }
            Some(tok) => {
                let at = self.span_of(tok);
                Err(ParseError {
                    at,
                    message: "Expected expression.".to_string(),
                })
            }
            None => Err(ParseError {
                at: 0,
                message: "Expected expression.".to_string(),
            }),
        }
    }
}

/// dart 的 `unvendor`：去掉 `-webkit-` 等厂商前缀（只用于识别特殊函数名）。
fn unvendor(name: &str) -> &str {
    name.strip_prefix("-webkit-")
        .or_else(|| name.strip_prefix("-moz-"))
        .or_else(|| name.strip_prefix("-ms-"))
        .or_else(|| name.strip_prefix("-o-"))
        .unwrap_or(name)
}

/// 嵌套在 `min()`/`max()` 里时按纯 CSS 处理、内容不再校验的函数。
const CSS_MATH_FUNCS: [&str; 6] = ["calc", "clamp", "env", "var", "min", "max"];

impl<'a> Evaluator<'a> {
    /// `min()` / `max()` / `clamp()` 的纯 CSS 形态（现代 dart-sass 的计算式语义）。
    ///
    /// 实参里出现不可求值的原子（`var()` / `env()` / `constant()` 这类未知函数 /
    /// 裸标识符 / 插值）时整条按 CSS 透传；整段实参是 `calc()` 时剥掉冗余层。
    /// 实参全是数字时仍交给内置函数求值（`min(1px, 2px)` → `1px`）。
    fn css_math_call(&mut self, src: &str, f: &Function, name: &str) -> R<Option<Value>> {
        let math = name.to_ascii_lowercase();
        let raw = src.get(f.span.start..f.span.end).unwrap_or("");
        let (Some(open), Some(close)) = (raw.find('('), raw.rfind(')')) else {
            return Ok(None);
        };
        if close < open {
            return Ok(None);
        }
        let inner = &raw[open + 1..close];
        let base = self.base + f.span.start + open + 1;
        let mut texts = Vec::new();
        let mut opaque = false;
        for group in split_top_level(inner, ',') {
            let offset = group.as_ptr() as usize - inner.as_ptr() as usize;
            let lead = group.len() - group.trim_start().len();
            let Some((text, atom)) = self.css_math_arg(group.trim(), base + offset + lead)? else {
                return Ok(None);
            };
            opaque |= atom;
            texts.push(text);
        }
        let text = format!("{name}({})", texts.join(", "));
        if !opaque {
            if let Some(value) = self.math_numeric(&math, &texts) {
                return Ok(Some(value));
            }
        }
        Ok(Some(Value::Raw(text)))
    }

    /// 一个数学函数实参：整段就是一个 `calc()` 时剥掉冗余层。
    fn css_math_arg(&mut self, text: &str, at: usize) -> R<Option<(String, bool)>> {
        if let Some((inner, inner_at)) = whole_calc(text, at) {
            let Some(scan) = self.css_math_scan(inner, inner_at, 0, false)? else {
                return Ok(None);
            };
            if scan.end < inner.len() {
                return Ok(None);
            }
            // dart 的 `_needsParentheses`：剥掉 `calc()` 后只剩一个不透明值、
            // 且该值含空白 / `*` / `/` 或以 `var(` 开头时补括号
            // （`calc(var(--x))` → `(var(--x))`，`calc(env(x))` → `env(x)`）。
            if scan.opaque && !scan.operators && needs_parentheses(&scan.text) {
                return Ok(Some((format!("({})", scan.text), true)));
            }
            return Ok(Some((fold_calc_text(scan.text), scan.opaque)));
        }
        let Some(scan) = self.css_math_scan(text, at, 0, false)? else {
            return Ok(None);
        };
        if scan.end < text.len() {
            return Ok(None);
        }
        Ok(Some((fold_calc_text(scan.text), scan.opaque)))
    }

    /// 把一段函数调用当 SassScript 求值；结果是简单单位的数字才返回。
    /// `var()`/`env()`/未知函数会得到非数字（或报错），交由调用方按纯 CSS 透传。
    fn sass_math_number(&mut self, call: &str, at: usize) -> Option<Number> {
        match self.eval_text(call, at).ok()? {
            Value::Number(number) if number.is_simple() => Some(number.without_slash()),
            _ => None,
        }
    }

    /// 全数字实参：走内置 `min`/`max` 求值；单位不可比等情况返回 `None`（按 CSS 透传）。
    fn math_numeric(&mut self, name: &str, texts: &[String]) -> Option<Value> {
        if texts.is_empty() {
            return None;
        }
        let mut values = Vec::with_capacity(texts.len());
        for text in texts {
            match self.eval_text(text, 0).ok()? {
                value @ Value::Number(_) => values.push(value),
                _ => return None,
            }
        }
        crate::scss::builtins::call(name, &values, &[])
            .ok()
            .flatten()
    }

    /// `calc()` 内部的 SassScript：内联变量值与 Sass 函数调用，其余原样。
    ///
    /// 插值的结果用 `INTERP_OPEN`/`INTERP_CLOSE` 括起来：化简时它是不透明原子
    /// （dart 的 `CalculationInterpolation` 不参与求值），但旁边的数字仍能合并。
    fn calc_script(&mut self, text: &str, at: usize) -> R<String> {
        let bytes = text.as_bytes();
        let mut out = String::new();
        let mut i = 0;
        while i < bytes.len() {
            let b = bytes[i];
            if b == b'#' && bytes.get(i + 1) == Some(&b'{') {
                if let Some(end) = interpolation_end(text, i) {
                    out.push(INTERP_OPEN);
                    out.push_str(&self.interpolate(&text[i..end], at + i)?);
                    out.push(INTERP_CLOSE);
                    i = end;
                    continue;
                }
            }
            if b == b'$' {
                let end = ident_end(text, i + 1);
                let name = &text[i + 1..end];
                let value = self
                    .lookup(name)
                    .ok_or_else(|| EvalError::new(self.base + at + i, "Undefined variable."))?;
                out.push_str(&calc_text(&value)?);
                i = end;
                continue;
            }
            let ident_start = b.is_ascii_alphabetic()
                || (b == b'-'
                    && bytes
                        .get(i + 1)
                        .is_some_and(|c| c.is_ascii_alphabetic() || *c == b'-'));
            if ident_start && !starts_number(text, i) {
                let mut end = ident_end(text, i);
                // `map.get(...)` 这类限定名整体是一个 Sass 调用。
                if bytes.get(end) == Some(&b'.')
                    && bytes.get(end + 1).is_some_and(u8::is_ascii_alphabetic)
                {
                    let member_end = ident_end(text, end + 1);
                    if bytes.get(member_end) == Some(&b'(') {
                        end = member_end;
                    }
                }
                let name = &text[i..end];
                let lower = name.to_ascii_lowercase();
                if bytes.get(end) == Some(&b'(') {
                    if let Some(close) = matching_paren(text, end) {
                        let call = &text[i..=close];
                        let css_math = !lower.contains('.')
                            && (CSS_MATH_FUNCS.contains(&lower.as_str())
                                || lower == "calc"
                                || (lower.starts_with('-') && lower.ends_with("-calc")));
                        if !css_math {
                            // 用户函数 / Sass 内置：求值后按 CSS 文本内联。
                            let value = self.eval_text(call, at + i)?;
                            out.push_str(&calc_text(&value)?);
                            i = close + 1;
                            continue;
                        }
                        let inner = self.calc_script(&text[end + 1..close], at + end + 1)?;
                        out.push_str(&format!("{name}({inner})"));
                        i = close + 1;
                        continue;
                    }
                }
                out.push_str(name);
                i = end;
                continue;
            }
            let ch = text[i..].chars().next().expect("非空");
            out.push(ch);
            i += ch.len_utf8();
        }
        Ok(out)
    }

    /// 扫描一段 CSS 数学函数内容。`in_paren` 为真时遇到 `)` 就停下（不消费）。
    /// 语法不合 CSS 产生式时返回 `None`。
    fn css_math_scan(
        &mut self,
        text: &str,
        at: usize,
        mut i: usize,
        in_paren: bool,
    ) -> R<Option<MathScan>> {
        let bytes = text.as_bytes();
        let mut out = String::new();
        let mut value_pending = false;
        let mut prev_opaque = false;
        let mut opaque = false;
        let mut operators = false;
        loop {
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            let Some(&b) = bytes.get(i) else {
                return Ok((!in_paren).then_some(MathScan {
                    text: out,
                    end: i,
                    opaque,
                    operators,
                }));
            };
            match b {
                b')' if in_paren => {
                    return Ok(Some(MathScan {
                        text: out,
                        end: i + 1,
                        opaque,
                        operators,
                    }));
                }
                b',' if !in_paren => {
                    out.push_str(", ");
                    i += 1;
                    value_pending = false;
                    prev_opaque = false;
                    continue;
                }
                b',' => return Ok(None),
                b'+' | b'*' | b'/' if !starts_number(text, i) => {
                    out.push(' ');
                    out.push(b as char);
                    out.push(' ');
                    i += 1;
                    value_pending = false;
                    prev_opaque = false;
                    operators = true;
                    continue;
                }
                b'-' if !starts_number(text, i) => {
                    out.push_str(" - ");
                    i += 1;
                    value_pending = false;
                    prev_opaque = false;
                    operators = true;
                    continue;
                }
                b'#' if bytes.get(i + 1) == Some(&b'{') => {
                    // 插值是求值期才知道的值：dart 不做化简，整条按 CSS 透传。
                    if value_pending {
                        if !prev_opaque {
                            return Ok(None);
                        }
                        out.push(' ');
                    }
                    let Some(end) = interpolation_end(text, i) else {
                        return Ok(None);
                    };
                    out.push_str(&self.interpolate(&text[i..end], at + i)?);
                    i = end;
                    value_pending = true;
                    prev_opaque = true;
                    opaque = true;
                    continue;
                }
                b'(' => {
                    // `( 1px` 这种左括号后带空白的写法在 dart 里解析失败。
                    if value_pending || bytes.get(i + 1).is_some_and(u8::is_ascii_whitespace) {
                        return Ok(None);
                    }
                    let Some(inner) = self.css_math_scan(text, at, i + 1, true)? else {
                        return Ok(None);
                    };
                    out.push('(');
                    out.push_str(&inner.text);
                    out.push(')');
                    i = inner.end;
                    value_pending = true;
                    prev_opaque = inner.opaque;
                    opaque |= inner.opaque;
                    operators |= inner.operators;
                    continue;
                }
                _ if starts_number(text, i) => {
                    if value_pending {
                        // `1px 2px` 这类相邻数字不合法；`bar 1px` 则是合法的空格序列。
                        if !prev_opaque {
                            return Ok(None);
                        }
                        out.push(' ');
                    }
                    let end = number_end(text, i);
                    out.push_str(&text[i..end]);
                    i = end;
                    value_pending = true;
                    prev_opaque = false;
                    continue;
                }
                _ if b.is_ascii_alphabetic() => {
                    // 空格序列（`2px bar`）在 dart 里合法：标识符本身把整条变成不透明值。
                    if value_pending {
                        out.push(' ');
                    }
                    let end = ident_end(text, i);
                    let name = text[i..end].to_ascii_lowercase();
                    if bytes.get(end) != Some(&b'(') {
                        // 裸标识符（`bar`、`e`）：不透明值，整条按 CSS 透传。
                        out.push_str(&text[i..end]);
                        i = end;
                        value_pending = true;
                        prev_opaque = true;
                        opaque = true;
                        continue;
                    }
                    let Some(close) = matching_paren(text, end) else {
                        return Ok(None);
                    };
                    let inner_raw = &text[end + 1..close];
                    if matches!(name.as_str(), "min" | "max" | "clamp") {
                        let Some(inner) = self.css_math_scan(inner_raw, at + end + 1, 0, false)?
                        else {
                            return Ok(None);
                        };
                        if inner.end < inner_raw.len() {
                            return Ok(None);
                        }
                        out.push_str(&format!("{name}({})", inner.text));
                        prev_opaque = inner.opaque;
                        opaque |= inner.opaque;
                        operators |= inner.operators;
                    } else if let Some(number) = self.sass_math_number(&text[i..=close], at + i) {
                        // 已知 Sass 函数（`lightness($c)` / `map.get(…)`）：求值成数字后
                        // 内联，照常参与 CSS 数学（dart 默认就把实参当 SassScript 求值）。
                        if value_pending {
                            if !prev_opaque {
                                return Ok(None);
                            }
                            out.push(' ');
                        }
                        out.push_str(&number.format());
                        prev_opaque = false;
                    } else {
                        // `var()` / `env()` / 嵌套 `calc()` / 未知函数（`constant()`）：
                        // 原样内联（只做插值与空白折叠），并且都不可求值。
                        let inner = self.interpolate(inner_raw, at + end + 1)?;
                        out.push_str(&format!("{name}({})", collapse_spaces(&inner)));
                        prev_opaque = true;
                        opaque = true;
                    }
                    i = close + 1;
                    value_pending = true;
                    continue;
                }
                _ => return Ok(None),
            }
        }
    }
}

/// 一次 CSS 数学函数内容扫描的结果。
struct MathScan {
    text: String,
    end: usize,
    /// 含不可求值的原子（`var()` / `env()` / 未知函数 / 裸标识符 / 插值）。
    opaque: bool,
    /// 出现过运算符（决定剥 `calc()` 时要不要补括号）。
    operators: bool,
}

/// 整段文本恰好就是一个 `calc(...)` 时返回括号内的内容与它的源码起点。
fn whole_calc(text: &str, at: usize) -> Option<(&str, usize)> {
    let open = text.find('(')?;
    if unvendor(&text[..open].to_ascii_lowercase()) != "calc" {
        return None;
    }
    if matching_paren(text, open)? != text.len() - 1 {
        return None;
    }
    Some((&text[open + 1..text.len() - 1], at + open + 1))
}

/// dart 的 `_needsParentheses`：`calc()` 内容嵌进别的计算式时是否需要括号。
fn needs_parentheses(text: &str) -> bool {
    text.chars()
        .any(|c| c.is_whitespace() || c == '/' || c == '*')
        || text
            .get(..4)
            .is_some_and(|h| h.eq_ignore_ascii_case("var("))
}

// ---------------------------------------------------------------------------
// calc 常数化简
// ---------------------------------------------------------------------------

/// `calc()` 内容的化简结果。
enum CalcFold {
    /// 整条是纯数字：`calc(1px + 2px)` → `3px`（连 `calc()` 一起去掉）。
    Number(Number),
    /// 化简后的计算式文本（仍需外层 `calc()`）。
    Text(String),
    /// 超出支持范围：原样保留。
    Verbatim,
}

/// 计算式 AST。括号只在包住“单独一个原子”时保留（dart 同样保留
/// `calc((var(--x)))`），包住运算时丢掉，优先级由序列化重算。
enum CalcNode {
    Num(Number),
    Opaque(String),
    Op(char, Box<CalcNode>, Box<CalcNode>),
    Paren(Box<CalcNode>),
}

/// 插值结果的边界（`calc_script` 标记，化简完就删掉）。
const INTERP_OPEN: char = '\u{1}';
const INTERP_CLOSE: char = '\u{2}';
const INTERP_OPEN_BYTE: u8 = 1;

fn strip_interp_marks(text: &str) -> String {
    text.replace([INTERP_OPEN, INTERP_CLOSE], "")
}

/// 常数化简后的文本；化简不了时原样返回。
fn fold_calc_text(text: String) -> String {
    match fold_calc(&text) {
        CalcFold::Number(number) => number.format(),
        CalcFold::Text(folded) => folded,
        CalcFold::Verbatim => text,
    }
}

/// 化简一段 `calc()` 内容（已折叠空白），口径同 dart 的 `Calculation.simplify`：
/// 纯数字子式按 Sass 单位规则合并，其余按优先级重新序列化；需要括号时写成
/// 嵌套 `calc()`（老 WebKit 对 calc 内裸括号支持不完整）。
fn fold_calc(text: &str) -> CalcFold {
    let mut parser = CalcParser { text, i: 0 };
    let Some(node) = parser.sum() else {
        return CalcFold::Verbatim;
    };
    parser.skip_ws();
    if parser.i != text.len() {
        return CalcFold::Verbatim;
    }
    match simplify_calc(node) {
        CalcNode::Num(number) if number.is_simple() => CalcFold::Number(number),
        node => {
            let mut out = String::new();
            write_calc(&node, &mut out);
            CalcFold::Text(strip_interp_marks(&out))
        }
    }
}

struct CalcParser<'a> {
    text: &'a str,
    i: usize,
}

impl CalcParser<'_> {
    fn bytes(&self) -> &[u8] {
        self.text.as_bytes()
    }

    fn peek(&self) -> Option<u8> {
        self.bytes().get(self.i).copied()
    }

    fn skip_ws(&mut self) {
        while self.peek().is_some_and(|b| b.is_ascii_whitespace()) {
            self.i += 1;
        }
    }

    fn sum(&mut self) -> Option<CalcNode> {
        let mut lhs = self.product()?;
        loop {
            self.skip_ws();
            let op = match self.peek() {
                Some(b'+') => '+',
                Some(b'-') => '-',
                _ => return Some(lhs),
            };
            self.i += 1;
            let rhs = self.product()?;
            lhs = CalcNode::Op(op, Box::new(lhs), Box::new(rhs));
        }
    }

    fn product(&mut self) -> Option<CalcNode> {
        let mut lhs = self.unary()?;
        loop {
            self.skip_ws();
            let op = match self.peek() {
                Some(b'*') => '*',
                Some(b'/') => '/',
                _ => return Some(lhs),
            };
            self.i += 1;
            let rhs = self.unary()?;
            lhs = CalcNode::Op(op, Box::new(lhs), Box::new(rhs));
        }
    }

    fn unary(&mut self) -> Option<CalcNode> {
        self.skip_ws();
        match self.peek() {
            Some(b'+') => {
                self.i += 1;
                self.unary()
            }
            Some(b'-') => {
                self.i += 1;
                Some(negate_calc(self.unary()?))
            }
            _ => self.atom(),
        }
    }

    fn atom(&mut self) -> Option<CalcNode> {
        self.skip_ws();
        match self.peek()? {
            INTERP_OPEN_BYTE => {
                let start = self.i + 1;
                let len = self.text[start..].find(INTERP_CLOSE)?;
                self.i = start + len + 1;
                Some(CalcNode::Opaque(self.text[start..start + len].to_string()))
            }
            b'(' => {
                self.i += 1;
                let node = self.sum()?;
                self.skip_ws();
                if self.peek()? != b')' {
                    return None;
                }
                self.i += 1;
                Some(CalcNode::Paren(Box::new(node)))
            }
            b if b.is_ascii_digit() || b == b'.' => self.number(),
            b if b.is_ascii_alphabetic() => self.ident(),
            _ => None,
        }
    }

    fn number(&mut self) -> Option<CalcNode> {
        let (value, unit, len) = calc_number(&self.text[self.i..])?;
        self.i += len;
        Some(CalcNode::Num(if unit.is_empty() {
            Number::unitless(value)
        } else {
            Number::with_unit(value, unit)
        }))
    }

    fn ident(&mut self) -> Option<CalcNode> {
        let start = self.i;
        let end = ident_end(self.text, start);
        let name = &self.text[start..end];
        if self.bytes().get(end) != Some(&b'(') {
            self.i = end;
            return Some(CalcNode::Opaque(name.to_string()));
        }
        let close = matching_paren(self.text, end)?;
        let inner = &self.text[end + 1..close];
        self.i = close + 1;
        if unvendor(&name.to_ascii_lowercase()) != "calc" {
            return Some(CalcNode::Opaque(format!("{name}({inner})")));
        }
        // `calc()` 嵌在计算式里：展开成子式（dart 的 calculation 是扁平的）。
        let mut sub = CalcParser { text: inner, i: 0 };
        let node = sub.sum()?;
        sub.skip_ws();
        (sub.i == inner.len()).then_some(node)
    }
}

/// 一元负号：数字直接取反，其余按 dart 的写法变成 `-1 * x`。
fn negate_calc(node: CalcNode) -> CalcNode {
    match node {
        CalcNode::Num(number) => CalcNode::Num(Number {
            value: -number.value,
            ..number
        }),
        other => CalcNode::Op(
            '*',
            Box::new(CalcNode::Num(Number::unitless(-1.0))),
            Box::new(other),
        ),
    }
}

fn simplify_calc(node: CalcNode) -> CalcNode {
    if let CalcNode::Paren(inner) = node {
        // `calc((1em*0.5))` 折叠成数字；`calc((var(--x)))` 保留括号（dart 实测）。
        return match simplify_calc(*inner) {
            CalcNode::Opaque(text) => CalcNode::Paren(Box::new(CalcNode::Opaque(text))),
            other => other,
        };
    }
    let CalcNode::Op(op, lhs, rhs) = node else {
        return node;
    };
    let (lhs, rhs) = (simplify_calc(*lhs), simplify_calc(*rhs));
    if let (CalcNode::Num(a), CalcNode::Num(b)) = (&lhs, &rhs) {
        if let Some(number) = fold_calc_numbers(op, a, b) {
            return CalcNode::Num(number);
        }
    }
    // `a + -2px` → `a - 2px`，`a - -2px` → `a + 2px`（dart 同样规整）。
    if let (CalcNode::Num(b), '+' | '-') = (&rhs, op) {
        if b.value < 0.0 {
            let flipped = if op == '+' { '-' } else { '+' };
            let positive = CalcNode::Num(Number {
                value: -b.value,
                ..b.clone()
            });
            return CalcNode::Op(flipped, Box::new(lhs), Box::new(positive));
        }
    }
    CalcNode::Op(op, Box::new(lhs), Box::new(rhs))
}

/// 两个数字的加减乘除；单位不可合并 / 结果是复合单位时返回 `None`（保留原式）。
fn fold_calc_numbers(op: char, a: &Number, b: &Number) -> Option<Number> {
    let result = match op {
        '+' | '-' if a.is_unitless() != b.is_unitless() => return None,
        '+' => a.add(b).ok()?,
        '-' => a.sub(b).ok()?,
        '*' if b.is_unitless() => a.mul(b),
        '*' if a.is_unitless() => b.mul(a),
        '*' => return None,
        '/' if b.is_unitless() => a.div(b).ok()?,
        '/' if a.is_simple() && b.is_simple() && !a.unit().is_empty() && a.unit() == b.unit() => {
            Number::unitless(a.value / b.value)
        }
        _ => return None,
    };
    (result.value.is_finite() && result.is_simple()).then(|| result.without_slash())
}

fn calc_precedence(op: char) -> u8 {
    match op {
        '+' | '-' => 1,
        _ => 2,
    }
}

fn write_calc(node: &CalcNode, out: &mut String) {
    match node {
        CalcNode::Num(number) => out.push_str(&number.format()),
        CalcNode::Opaque(text) => out.push_str(text),
        CalcNode::Paren(inner) => {
            out.push('(');
            write_calc(inner, out);
            out.push(')');
        }
        CalcNode::Op(op, lhs, rhs) => {
            write_calc_child(lhs, *op, false, out);
            out.push(' ');
            out.push(*op);
            out.push(' ');
            write_calc_child(rhs, *op, true, out);
        }
    }
}

/// 右结合侧的同级运算要括号（`a - (b - c)`）；需要时写成嵌套 `calc()`。
fn write_calc_child(child: &CalcNode, parent: char, right: bool, out: &mut String) {
    let needs = match child {
        CalcNode::Op(inner, ..) => {
            let (parent_prec, inner_prec) = (calc_precedence(parent), calc_precedence(*inner));
            inner_prec < parent_prec
                || (right
                    && inner_prec == parent_prec
                    && (*inner != parent || parent == '-' || parent == '/'))
        }
        _ => false,
    };
    if needs {
        out.push_str("calc(");
        write_calc(child, out);
        out.push(')');
    } else {
        write_calc(child, out);
    }
}

/// 解析 `1.5px` / `50%` / `1e3px`：返回 (数值, 单位, 消费的字节数)。
fn calc_number(text: &str) -> Option<(f64, &str, usize)> {
    let bytes = text.as_bytes();
    let mut j = 0;
    while bytes.get(j).is_some_and(u8::is_ascii_digit) {
        j += 1;
    }
    if bytes.get(j) == Some(&b'.') {
        j += 1;
        while bytes.get(j).is_some_and(u8::is_ascii_digit) {
            j += 1;
        }
    }
    if matches!(bytes.get(j), Some(b'e') | Some(b'E')) {
        let mut k = j + 1;
        if matches!(bytes.get(k), Some(b'+') | Some(b'-')) {
            k += 1;
        }
        if bytes.get(k).is_some_and(u8::is_ascii_digit) {
            j = k;
            while bytes.get(j).is_some_and(u8::is_ascii_digit) {
                j += 1;
            }
        }
    }
    let digits = j;
    while bytes.get(j).is_some_and(u8::is_ascii_alphabetic) {
        j += 1;
    }
    if bytes.get(j) == Some(&b'%') {
        j += 1;
    }
    let value: f64 = text[..digits].parse().ok()?;
    Some((value, &text[digits..j], j))
}

fn starts_number(text: &str, i: usize) -> bool {
    let bytes = text.as_bytes();
    let mut j = i;
    if matches!(bytes.get(j), Some(b'+') | Some(b'-')) {
        j += 1;
    }
    bytes.get(j).is_some_and(u8::is_ascii_digit)
        || (bytes.get(j) == Some(&b'.') && bytes.get(j + 1).is_some_and(u8::is_ascii_digit))
}

/// 数字（含符号、小数、指数）加单位的结尾位置。
fn number_end(text: &str, i: usize) -> usize {
    let bytes = text.as_bytes();
    let mut j = i;
    if matches!(bytes.get(j), Some(b'+') | Some(b'-')) {
        j += 1;
    }
    while bytes.get(j).is_some_and(u8::is_ascii_digit) {
        j += 1;
    }
    if bytes.get(j) == Some(&b'.') {
        j += 1;
        while bytes.get(j).is_some_and(u8::is_ascii_digit) {
            j += 1;
        }
    }
    if matches!(bytes.get(j), Some(b'e') | Some(b'E')) {
        let mut k = j + 1;
        if matches!(bytes.get(k), Some(b'+') | Some(b'-')) {
            k += 1;
        }
        if bytes.get(k).is_some_and(u8::is_ascii_digit) {
            j = k;
            while bytes.get(j).is_some_and(u8::is_ascii_digit) {
                j += 1;
            }
        }
    }
    if bytes.get(j) == Some(&b'%') {
        return j + 1;
    }
    while bytes.get(j).is_some_and(u8::is_ascii_alphabetic) {
        j += 1;
    }
    j
}

fn ident_end(text: &str, i: usize) -> usize {
    let bytes = text.as_bytes();
    let mut j = i;
    while bytes
        .get(j)
        .is_some_and(|b| b.is_ascii_alphabetic() || *b == b'-')
    {
        j += 1;
    }
    j
}

/// `#{...}` 的结尾（含 `}` 之后的位置）。
fn interpolation_end(text: &str, i: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut depth = 1usize;
    let mut j = i + 2;
    while j < bytes.len() {
        match bytes[j] {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(j + 1);
                }
            }
            b'\'' | b'"' => {
                let quote = bytes[j];
                j += 1;
                while j < bytes.len() && bytes[j] != quote {
                    if bytes[j] == b'\\' {
                        j += 1;
                    }
                    j += 1;
                }
            }
            _ => {}
        }
        j += 1;
    }
    None
}

/// `open` 位置 `(` 对应的 `)` 位置。
fn matching_paren(text: &str, open: usize) -> Option<usize> {
    crate::text::matching(text, open).map(|end| end - 1)
}

/// 把连续空白折成一个空格（不裁剪首尾）。
fn collapse_spaces(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut space = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            if !space {
                out.push(' ');
                space = true;
            }
        } else {
            out.push(ch);
            space = false;
        }
    }
    out
}

/// `calc()` 里内联一个 Sass 值：字符串去掉引号，其余用 CSS 文本。
fn calc_text(value: &Value) -> Result<String, EvalError> {
    let text = match value {
        Value::Str(text) => text.text.clone(),
        other => other
            .to_css()
            .map_err(|message| EvalError { message, at: 0 })?,
    };
    Ok(text)
}

/// `calc(...)` / `min(...)` 之类 → (函数名, 括号内的文本)。
fn split_calc(text: &str) -> Option<(String, String)> {
    const CALC_FUNCS: [&str; 11] = [
        "calc", "min", "max", "clamp", "round", "mod", "rem", "sin", "cos", "tan", "hypot",
    ];
    let open = text.find('(')?;
    if !text.ends_with(')') {
        return None;
    }
    let name = text[..open].to_ascii_lowercase();
    let plain = name
        .trim_start_matches("-webkit-")
        .trim_start_matches("-moz-");
    if !CALC_FUNCS.contains(&plain) {
        return None;
    }
    Some((
        plain.to_string(),
        text[open + 1..text.len() - 1].to_string(),
    ))
}

/// 按顶层分隔符切分（忽略括号 / 引号 / 注释内的分隔符）。
fn split_top_level(text: &str, sep: char) -> Vec<&str> {
    crate::text::split_top_level(text, sep as u8)
        .into_iter()
        .map(|(start, end)| &text[start..end])
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// S2 的 mixin / function 查找会复用这个归一（dart：`m_x` ≡ `m-x`，但 `un_quote` ≠ `unquote`）。
    #[test]
    fn normalize_name_maps_underscore_to_hyphen() {
        assert_eq!(normalize_name("a_b"), "a-b");
        assert_eq!(normalize_name("a-b"), "a-b");
        assert_eq!(normalize_name("a__b"), "a--b");
        assert_eq!(normalize_name("m_x"), "m-x");
        assert_eq!(normalize_name("un_quote"), "un-quote");
        assert_eq!(normalize_name("plain"), "plain");
    }

    #[test]
    fn variables_are_underscore_hyphen_equivalent() {
        let mut eval = Evaluator::new(1);
        eval.define("a_b", Value::Bool(true), false, false);
        assert!(eval.lookup("a-b").is_some());
        eval.define("c-d", Value::Bool(false), false, false);
        assert!(eval.lookup("c_d").is_some());
    }
}
