//! SassScript evaluation: expression parsing (including `/` regrouping) -> typed values ->
//! variables/scope.
//!
//! The parser does not use oxc's expression tree directly: oxc's precedence differs from
//! dart-sass's "slash syntax" (`1 / 2 * 3` is folded into `1 / (2 * 3)`). Here the oxc
//! expression nodes are **flattened into a token sequence** (`SassBinaryExpression` is
//! restored to infix order recursively) and then regrouped by dart-sass's precedence:
//!
//! - precedence: `or` < `and` < `== !=` < `< <= > >=` < `+ -` < `* / %` < unary;
//! - `/` is an ordinary binary division, but when "the whole subtree consists only of `/`
//!   and literal numbers" the result keeps the written form `a/b` (dart's
//!   `SassNumber.withSlash`);
//! - taking part in other arithmetic, variable assignment, parentheses, or being an operand
//!   of another operation -> the written form is dropped.

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
    /// Absolute offset in the merged text (used to map back to file line/column).
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

/// User function recursion limit (dart itself overflows the stack on deep recursion too).
///
/// 64 rather than the 256 used for mixins: function evaluation has heavier stack frames
/// (128 levels already exceed the default 2 MiB thread stack in a debug build), while real
/// projects (bootstrap/foundation) recurse only a few levels deep.
const MAX_CALL_DEPTH: usize = 64;

/// Arguments of one `@include` / `@content` (already evaluated).
#[derive(Default)]
pub(crate) struct Invocation {
    pub(crate) positional: Vec<Value>,
    /// (normalized name, value, keyword name span).
    pub(crate) keywords: Vec<(String, Value, Span)>,
}

/// In Sass identifiers `-` and `_` are completely equivalent (for variable / function /
/// mixin names; not for namespaces): the symbol table always stores keys in the `-` form
/// and normalizes on lookup.
pub(crate) fn normalize_name(name: &str) -> String {
    if name.contains('_') {
        name.replace('_', "-")
    } else {
        name.to_string()
    }
}

/// Leftover check when `$rest` exists: extra named arguments + the shared "was the keyword
/// map read" marker.
pub(crate) struct RestCheck {
    pub(crate) names: Vec<String>,
    pub(crate) accessed: Rc<Cell<bool>>,
}

/// One lexical frame. `semi_global` marks the scope of control directives (`@if` etc.):
/// assignment prefers an existing variable over creating a new shadow (dart's
/// `Environment.scope(semiGlobal: true)`).
#[derive(Debug, Default, Clone)]
pub(crate) struct Frame<'a> {
    vars: HashMap<String, Value>,
    funcs: HashMap<String, FunctionDef<'a>>,
    semi_global: bool,
}

/// One parameter (shared by `@mixin` declarations, `@include ... using` and `@function`).
#[derive(Clone, Debug)]
pub(crate) struct Param<'a> {
    pub(crate) name: &'a str,
    pub(crate) default: Option<&'a ComponentValue<'a>>,
}

/// A `@function` definition. `scope`/`depth` are the lexical position of the definition:
/// a call truncates the frame stack to `depth`, so the body cannot see the caller's locals
/// (dart's closure semantics).
#[derive(Clone, Debug)]
pub(crate) struct FunctionDef<'a> {
    pub(crate) params: Vec<Param<'a>>,
    pub(crate) rest: Option<&'a str>,
    pub(crate) body: &'a [Statement<'a>],
    /// Offset of the definition (the whole `@function`): `Function finished without
    /// @return.` is reported here.
    pub(crate) at: usize,
    pub(crate) scope: usize,
    pub(crate) depth: usize,
}

/// Result of executing a user function body: either ran to the end (missing `@return`) or
/// returned early.
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

/// A namespace obtained from `@use`: a user module (scope id) or a built-in module.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ModuleRef {
    User(usize),
    Builtin(Builtin),
}

/// Modules pulled in by `@use ... as *` (in import order; later ones win).
///
/// Members are visible only in this scope: they take part in unqualified-name resolution
/// together with the own members, but **do not enter** this module's exported member table
/// (`frames[0]`) -- only `@forward` exports members.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StarModule {
    User(usize),
    Builtin(Builtin),
}

/// The kind of an exported member: variable or non-variable (functions and mixins share one
/// show/hide list).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MemberKind {
    Var,
    Member,
}

/// Member list of `@forward ... show / hide` (names already normalized, variables without
/// `$`).
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

/// One `@forward` edge: exports the members of the target module downstream, with an
/// optional literal prefix and member filter.
///
/// Forwarded members are resolved **lazily**: they do not enter this module's member table,
/// so they are invisible inside this module, and later changes to the target module are
/// visible downstream.
#[derive(Clone, Debug)]
pub(crate) struct ForwardEdge {
    pub(crate) scope: usize,
    /// Start of the `@forward` statement in the merged text (location of duplicate-forward
    /// errors).
    pub(crate) at: usize,
    /// The literal prefix of `as p-*` (`p-`), prepended to the member names.
    pub(crate) prefix: String,
    pub(crate) show: Option<MemberFilter>,
    pub(crate) hide: MemberFilter,
}

impl ForwardEdge {
    /// The filter looks at the exported name **after the prefix is added** (observed with
    /// dart 1.93.2).
    pub(crate) fn allows(&self, kind: MemberKind, name: &str) -> bool {
        self.show.as_ref().is_none_or(|f| f.allows(kind, name)) && !self.hide.allows(kind, name)
    }

    /// The internal name after the prefix is removed from the exported name.
    pub(crate) fn strip<'n>(&self, name: &'n str) -> Option<&'n str> {
        name.strip_prefix(self.prefix.as_str())
    }
}

/// Whether a member name is private (starts with `-` / `_`: not exported and not
/// accessible from outside the module).
pub(crate) fn is_private(name: &str) -> bool {
    name.starts_with('-') || name.starts_with('_')
}

/// A pending `with` configuration (`!default` means the caller may override it).
#[derive(Clone, Debug)]
pub(crate) struct PendingConfig {
    pub(crate) id: usize,
    pub(crate) name: String,
    pub(crate) value: Value,
    /// Start of the configuration entry in the merged text (error location).
    pub(crate) at: usize,
    pub(crate) default: bool,
}

/// Recursion limit of the `@forward` chain (a last resort beyond the assembly layer's
/// cycle detection).
const MAX_FORWARD_DEPTH: usize = 32;

/// The evaluator: every "scope id" owns a set of variable frames, and `active` decides
/// which set is currently in use.
pub(crate) struct Evaluator<'a> {
    scopes: Vec<Scope<'a>>,
    namespaces: Vec<HashMap<String, (ModuleRef, u32)>>,
    /// Modules pulled in by `@use ... as *` (in import order; later ones win).
    star: Vec<Vec<StarModule>>,
    /// The `@forward` edges of every scope (in declaration order).
    forwards: Vec<Vec<ForwardEdge>>,
    /// Pending `with` configurations of every module.
    pending: Vec<Vec<PendingConfig>>,
    /// (id, location) of all configuration entries: the ones nobody consumes are reported
    /// at the end of the compilation.
    configs: Vec<(usize, usize)>,
    used_configs: HashSet<usize>,
    config_seq: usize,
    /// Mixin names declared in every scope (used by `meta.mixin-exists()`).
    mixins: Vec<HashSet<String>>,
    active: usize,
    base: usize,
    /// User function recursion depth (see `MAX_CALL_DEPTH` for the limit).
    call_depth: usize,
    /// Text of the current parent selector (the value of `&`); `None` at the root level
    /// (dart: `null`).
    parent_selector: Option<String>,
    /// Whether we are currently inside a mixin body, and whether this call received a
    /// content block (used by `content-exists()`).
    mixin_context: (bool, bool),
    /// The merged real source. Interpolation `#{...}` goes through temporary text "wrapped as
    /// `a{b:...}`", whose spans must be shifted by `base`; function bodies, on the other
    /// hand, come from the real AST and must be evaluated against the real source.
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

    /// Update the value of `&` when entering/leaving a rule.
    pub(crate) fn set_parent_selector(&mut self, text: Option<String>) {
        self.parent_selector = text;
    }

    pub(crate) fn enter(&mut self, scope: usize) {
        self.active = scope.min(self.scopes.len() - 1);
    }

    pub(crate) fn active_scope(&self) -> usize {
        self.active
    }

    /// Whether we are currently at a module's global layer (top level, or wrapped only in
    /// semi-global frames such as `@if`): `with` configurations are only consumed by variable
    /// declarations at this layer.
    pub(crate) fn at_module_global(&self) -> bool {
        self.scopes[self.active].frames[1..]
            .iter()
            .all(|frame| frame.semi_global)
    }

    /// Frame count of the current scope (used by `@include` to record the caller's
    /// environment depth).
    pub(crate) fn frame_depth(&self) -> usize {
        self.scopes[self.active].frames.len()
    }

    /// Enter a block scope (variables shadow the outer ones); not called for the top level of
    /// a file (top-level variables are the scope's globals).
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

    /// Lexical isolation: reconnect / take off the frame stack (`@include` / `@content`
    /// must run in the definition environment).
    pub(crate) fn take_frames(&mut self, scope: usize) -> Vec<Frame<'a>> {
        let index = scope.min(self.scopes.len() - 1);
        std::mem::take(&mut self.scopes[index].frames)
    }

    pub(crate) fn put_frames(&mut self, scope: usize, frames: Vec<Frame<'a>>) {
        let index = scope.min(self.scopes.len() - 1);
        self.scopes[index].frames = frames;
    }

    /// Truncate the frame stack to `depth` (the definition environment of the lexical
    /// closure) and return the tail that was taken off.
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

    /// `@function`: register in the current frame (declarations inside a function body only
    /// live for that call).
    pub(crate) fn define_function(&mut self, name: &str, def: FunctionDef<'a>) {
        let name = normalize_name(name);
        if let Some(frame) = self.scopes[self.active].frames.last_mut() {
            frame.funcs.insert(name, def);
        }
    }

    /// User function lookup: first the own frames (from the inside out), then the modules
    /// pulled in by `@use ... as *` (later ones win). Members of a star module are visible
    /// only in this scope and are not exported.
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

    /// A function inside a namespace (`@use` module): only the module's global frame is
    /// consulted.
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

    /// Parameter binding: always writes the current frame (it must not override an outer
    /// variable of the same name).
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

    /// Scope id of a user module (used for mixins imported by `@import` and for `ns.mixin`);
    /// built-in modules have no user scope.
    pub(crate) fn namespace_scope(&self, namespace: &str) -> Option<usize> {
        match self.namespace_module(namespace)? {
            ModuleRef::User(scope) => Some(scope),
            ModuleRef::Builtin(_) => None,
        }
    }

    /// Whether a module namespace exists (built-in modules count as well).
    pub(crate) fn has_namespace(&self, namespace: &str) -> bool {
        self.namespaces[self.active].contains_key(namespace)
    }

    /// Variable declaration (dart-sass semantics):
    /// - `!default`: assign only when the variable is undefined or `null`;
    /// - `!global`: write the scope's global frame;
    /// - plain assignment: write "the nearest block frame that already declared it" (an
    ///   assignment in a deeper block can change an outer block's local variable); when none
    ///   exists, declare it in the current frame -- the global frame is only changed by an
    ///   assignment at that layer.
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
        // Walk up to the frame that "already declared" it and overwrite its value; the global
        // frame only counts when "the root itself or all frames above it are @if frames"
        // (observed with dart: an `@if` at the root layer changes the global, an `@if` inside
        // a rule only creates a local).
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

    /// Variable lookup: first the own frames, then the modules pulled in by
    /// `@use ... as *` (later ones win).
    ///
    /// The variables of a star module are **read lazily**: they are not copied into this
    /// module's global frame (which would leak them downstream through `@use this as *`),
    /// and later changes to the module are still visible.
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

    /// `m.$var`: a user module reads its global frame, a built-in module its static variable
    /// table.
    pub(crate) fn module_variable(&self, namespace: &str, name: &str) -> Option<Option<Value>> {
        match self.namespace_module(namespace)? {
            ModuleRef::User(scope) => Some(self.export_var(scope, name)),
            ModuleRef::Builtin(module) => Some(module.variable(name)),
        }
    }

    fn insert_namespace(&mut self, namespace: &str, module: ModuleRef, file: u32) -> R<()> {
        let table = &mut self.namespaces[self.active];
        // Duplicate names have already been rejected statically per file by the assembly
        // layer; the duplicate declaration here (the same file imported twice by `@import`)
        // silently keeps the first one.
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

    /// Register a `@forward` edge (members are resolved lazily and are not copied into this
    /// module's member table).
    pub(crate) fn add_forward(&mut self, scope: usize, edge: ForwardEdge) {
        self.forwards[scope].push(edge);
    }

    pub(crate) fn forwards(&self, scope: usize) -> &[ForwardEdge] {
        &self.forwards[scope]
    }

    pub(crate) fn scope_count(&self) -> usize {
        self.scopes.len()
    }

    /// Register a `with` configuration (`default` = `@forward ... with (... !default)`).
    ///
    /// Returns the conflict location: configuring the same variable again while a non-default
    /// configuration exists is an error.
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

    /// Put a configuration into the target module (merged by the `!default` rules when a
    /// configuration of the same name already exists).
    pub(crate) fn push_config(&mut self, scope: usize, entry: PendingConfig) -> Result<(), usize> {
        let list = &mut self.pending[scope];
        let Some(existing) = list.iter_mut().find(|e| e.name == entry.name) else {
            list.push(entry);
            return Ok(());
        };
        // The same configuration travelling down several forward edges: what the target
        // already holds is itself.
        if existing.id == entry.id {
            return Ok(());
        }
        match (existing.default, entry.default) {
            // A hard configuration already exists: the new default is ignored.
            (false, true) => {
                self.used_configs.insert(entry.id);
            }
            // A default value is overridden by a hard configuration: the old entry no longer
            // reports an error.
            (true, false) => {
                self.used_configs.insert(existing.id);
                *existing = entry;
            }
            (true, true) => {
                self.used_configs.insert(entry.id);
            }
            // Two hard configurations: the later one reports "isn't a !default variable".
            (false, false) => return Err(entry.at),
        }
        Ok(())
    }

    /// Take the configuration of the same name for the current module (consumed at the
    /// declaration).
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

    /// When the configuration of the same name has not been consumed yet, a non-`!default`
    /// declaration is an error (returns the configuration location).
    pub(crate) fn config_conflict(&self, name: &str) -> Option<usize> {
        let name = normalize_name(name);
        self.pending[self.active]
            .iter()
            .find(|e| e.name == name && !self.used_configs.contains(&e.id))
            .map(|e| e.at)
    }

    /// Whether a configuration has already been consumed along the chain (skipped when
    /// passed down by `@forward`).
    pub(crate) fn config_used(&self, id: usize) -> bool {
        self.used_configs.contains(&id)
    }

    /// The configurations pending for the current module (passing down is a "copy": the
    /// configuration itself stays in this module for its own `!default` declaration to
    /// consume -- dart's `Configuration` is a shared object).
    pub(crate) fn pending_configs(&self, scope: usize) -> Vec<PendingConfig> {
        self.pending[scope].clone()
    }

    /// End-of-compilation check: whether any configuration entry was consumed by nobody
    /// (dart reports at the configuration).
    pub(crate) fn unresolved_config(&self) -> Option<usize> {
        self.configs
            .iter()
            .find(|(id, _)| !self.used_configs.contains(id))
            .map(|(_, at)| *at)
    }

    /// Read a variable from the module's exported members (own global frame + `@forward`
    /// chain).
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

    /// A function among the module's exported members.
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

    /// Variable names among the module's exported members (`meta.module-variables()`).
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

    /// The module a variable is finally defined in (used for `@forward` duplicate
    /// detection).
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

    /// The module a function is finally defined in (used for `@forward` duplicate
    /// detection).
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

    /// Function names among the module's exported members (`meta.module-functions()`).
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

    /// Mixin names among the module's exported members (used by `mixin-exists()`; the mixin
    /// bodies live in `Converter`).
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

    /// A user module pulled in by `@use ... as *`: its members take part in this scope's
    /// unqualified-name resolution but are not exported.
    pub(crate) fn import_star_module(&mut self, scope: usize) {
        let star = &mut self.star[self.active];
        let module = StarModule::User(scope);
        if !star.contains(&module) {
            star.push(module);
        }
    }

    /// `@use "sass:xxx" as *`: the member table takes part in unqualified-name resolution
    /// (variables are read by `lookup`).
    pub(crate) fn import_builtin_star(&mut self, module: Builtin) {
        let star = &mut self.star[self.active];
        let module = StarModule::Builtin(module);
        if !star.contains(&module) {
            star.push(module);
        }
    }

    /// Scopes of user modules pulled in with a star (later ones first), used for the
    /// unqualified lookup of `@include`.
    pub(crate) fn star_scopes(&self) -> impl Iterator<Item = usize> + '_ {
        self.star[self.active]
            .iter()
            .rev()
            .filter_map(|module| match module {
                StarModule::User(scope) => Some(*scope),
                StarModule::Builtin(_) => None,
            })
    }

    /// Members of `as *` built-in modules: searched in reverse import order (later ones win),
    /// and **before** the global built-in function table -- observed with dart:
    /// `@use "sass:color" as *` makes `lighten()` report "isn't in the module".
    fn star_member(&self, name: &str) -> Option<Builtin> {
        self.star[self.active]
            .iter()
            .rev()
            .find_map(|module| match module {
                StarModule::Builtin(module) if module.has_member(name) => Some(*module),
                _ => None,
            })
    }

    /// Register a mixin name (used by `meta.mixin-exists()`).
    pub(crate) fn define_mixin(&mut self, name: &str) {
        self.mixins[self.active].insert(normalize_name(name));
    }

    /// Whether the current mixin call carries a content block (the `{}` of
    /// `@include m {}` counts too).
    pub(crate) fn set_mixin_context(&mut self, in_mixin: bool, has_content: bool) {
        self.mixin_context = (in_mixin, has_content);
    }

    // -----------------------------------------------------------------------
    // Public entry points
    // -----------------------------------------------------------------------

    /// A declaration value / an arbitrary expression sequence.
    pub(crate) fn eval_value(&mut self, src: &str, comps: &[ComponentValue]) -> R<Value> {
        self.eval_flat(src, comps)
    }

    /// A single component value.
    pub(crate) fn eval_component(&mut self, src: &str, comp: &ComponentValue) -> R<Value> {
        self.eval_comp(src, comp)
    }

    /// Evaluate from text (inside `#{}` of a selector / media prelude): the text is wrapped
    /// in `a{b:<text>}` and handed back to the oxc lexer, and error offsets are mapped back
    /// to the original text.
    pub(crate) fn eval_text(&mut self, text: &str, at: usize) -> R<Value> {
        const PREFIX: usize = 4; // "a{b:"
        let wrapped = format!("a{{b:{text}}}");
        let alloc = Allocator::default();
        // Even when the source file is `.sass`, the value/selector text has already been
        // folded into SCSS form, so it is parsed as SCSS here.
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

    /// `#{...}` interpolation in text (the result is unquoted text).
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
            let ch = text[i..].chars().next().expect("boundary safe");
            out.push(ch);
            i += ch.len_utf8();
        }
        Ok(out)
    }

    // -----------------------------------------------------------------------
    // Expressions
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
                    return Ok(values.pop().expect("non-empty"));
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
                    "SCSS feature not implemented yet (S2): module member reference",
                )),
            },
            // `&` in an expression is the current parent selector (null at the root layer):
            // `if(&, "&", "")` relies on it.
            ComponentValue::SassParentSelector(_) => Ok(match &self.parent_selector {
                Some(text) => Value::Str(value::SassStr::unquoted(text.clone())),
                None => Value::Null,
            }),
            ComponentValue::InterpolableIdent(ident) => self.eval_ident(src, ident),
            ComponentValue::InterpolableStr(s) => self.eval_str(src, s),
            ComponentValue::Function(f) => self.eval_function(src, f),
            ComponentValue::Url(u) => self.eval_url(src, u),
            // oxc reads the arguments of CSS math functions (`round()` / `mod()` …) as `Calc`
            // nodes; for the Sass built-ins of 1.35.2 it is just an ordinary expression
            // (`round(1/3)` = 0).
            ComponentValue::Calc(c) => {
                let text = src.get(c.span.start..c.span.end).unwrap_or("");
                self.eval_text(text, c.span.start)
            }
            // The arguments of `min()`/`max()` are read by oxc as a Calc syntax tree: what is
            // inside the parentheses is still an ordinary expression.
            ComponentValue::CalcParenthesized(p) => {
                let value = self.eval_comp(src, &p.expr)?;
                Ok(value.without_slash())
            }
            ComponentValue::SassParenthesizedExpression(p) => {
                let value = self.eval_flat(src, std::slice::from_ref(&p.expr))?;
                Ok(value.without_slash())
            }
            // Bare expression nodes (the inner part of `(10px/2px)`, function arguments, …)
            // go through the same grouping parser.
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
            // oxc's `SassList` is just "a sequence not separated by commas": handle the bare
            // `/` inside it (the elements of `#{10 / 4}` are `10 | / | 4`), along with unary
            // and binary expressions.
            ComponentValue::SassList(list) if !matches!(&list.comma_spans, Some(spans) if !spans.is_empty()) => {
                self.eval_flat(src, &list.elements)
            }
            ComponentValue::SassList(list) => {
                let mut items = Vec::with_capacity(list.elements.len());
                for element in &list.elements {
                    items.push(self.eval_comp(src, element)?);
                }
                if items.len() == 1 {
                    return Ok(items.pop().expect("non-empty"));
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
                // `10px-5px` is a subtraction in SCSS (a unit never starts with `-digit`).
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
                "SCSS feature not implemented yet (S2): spread argument `...`",
            )),
            ComponentValue::SassKeywordArgument(a) => Err(EvalError::new(
                self.base + a.span.start,
                "SCSS feature not implemented yet (S1): keyword argument",
            )),
            other => Ok(Value::Raw(
                src.get(other.span().start..other.span().end)
                    .unwrap_or("")
                    .to_string(),
            )),
        }
    }

    /// Component sequence -> expression (flattened into tokens, then regrouped by dart's
    /// precedence).
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
                                "SCSS feature not implemented yet (S1): template placeholder",
                            ));
                        }
                    }
                }
                Ok(Value::Str(SassStr::unquoted(text)))
            }
            _ => Err(EvalError::new(
                self.base + ident_span(ident).start,
                "SCSS feature not implemented yet (S1): less-style interpolation",
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
                "SCSS feature not implemented yet (S1): less-style string interpolation",
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
        // `url($x)`: dart-sass evaluates it as a SassScript expression.
        if trimmed.starts_with('$') {
            let value = self.eval_text(trimmed, url.span.start + 4)?;
            let text = value
                .to_css()
                .map_err(|message| EvalError::new(self.base + url.span.start, message))?;
            return Ok(Value::Str(value::SassStr::unquoted(format!("url({text})"))));
        }
        // dart parses `url(...)` into an **unquoted string**: `type-of(url(x))` is `string`
        // and `str-slice(url(x), 1, 1)` is `u` (bootstrap's `escape-svg()` relies on it).
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

        // The inside of the calc family is not SassScript: only interpolation applies. dart
        // only recognizes `calc` and the vendor-prefixed `-webkit-calc` (user function names
        // such as `rem-calc` do not count).
        if lower == "calc" || (lower.starts_with('-') && lower.ends_with("-calc")) {
            let raw = src.get(f.span.start..f.span.end).unwrap_or("");
            let inner = raw
                .find('(')
                .and_then(|start| raw.rfind(')').map(|end| (start + 1, end)))
                .map(|(s, e)| &raw[s..e])
                .unwrap_or("");
            // The inside of calc is a calculation, but **SassScript still applies**: `$var`,
            // user functions and `#{}` must all be evaluated and inlined (element-plus's
            // `calc(getCssVar(...) - 1)`).
            let inner = self.calc_script(inner, f.span.start + 1)?;
            // The content of calc is re-serialized as a calculation: quotes are normalized to
            // dart's style.
            let inner = value::collapse_ws(&value::requote_strings(&inner));
            // Constant folding: an interpolation result is an opaque atom, but the numbers
            // next to it can still be merged.
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
        // `min()` / `max()` / `clamp()` are parsed as plain CSS math functions first (dart's
        // calculation semantics). Modern dart only recognizes these three names themselves:
        // `-webkit-min` is an unknown function (passed through as is).
        if matches!(lower.as_str(), "min" | "max" | "clamp") {
            if let Some(value) = self.css_math_call(src, f, &lower)? {
                return Ok(value);
            }
        }
        // `progid:...` is the verbatim form of an IE filter: passed through as is (whitespace
        // kept), only the quotes of strings are normalized.
        if lower.contains("progid") {
            let raw = src.get(f.span.start..f.span.end).unwrap_or("");
            let raw = self.interpolate(raw, f.span.start)?;
            return Ok(Value::Raw(value::normalize_raw(&value::requote_strings(
                &raw,
            ))));
        }
        // Other verbatim arguments (`alpha(opacity=65)`): no spaces around `=` (observed with
        // dart-sass).
        if value::has_raw_args(&f.args) {
            let raw = value::rewrite_raw(src, f.span);
            let raw = self.interpolate(&raw, f.span.start)?;
            return Ok(Value::Raw(value::strip_eq_spaces(&raw)));
        }

        // `if($cond, $a, $b)` is evaluated lazily.
        if normalize_name(&lower) == "if" {
            let groups = split_args(src, &f.args);
            if groups.len() == 3 {
                let cond = self.eval_value(src, groups[0])?;
                return self.eval_value(src, if cond.truthy() { groups[1] } else { groups[2] });
            }
        }

        // Metafunctions that need the evaluator context (variable/function existence,
        // first-class functions).
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

        // User functions take precedence over a built-in of the same name (observed with
        // dart: `@function rgb()` can override the built-in rgb).
        if let Some(def) = self.find_function(&name) {
            let inv = self.eval_invocation(src, &f.args)?;
            return self.call_function(def, src, inv, f.span.start);
        }

        let inv = self.eval_invocation(src, &f.args)?;
        // Function names are `-`/`_` equivalent as well (`map_get` = `map-get`); unknown
        // functions are still passed through with their original spelling.
        let keywords = inv
            .keywords
            .iter()
            .map(|(name, value, _)| (name.clone(), value.clone()))
            .collect::<Vec<_>>();
        // Members of `@use "sass:xxx" as *` are **higher priority than** the global built-in
        // functions: after `@use "sass:color" as *`, `lighten()` reports "isn't in the
        // sass:color module" (observed with 1.35.2).
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
            1 => inv.positional.into_iter().next().expect("length 1"),
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

    /// `meta.calc-name($calc)` / `meta.calc-args($calc)` (1.93.2).
    ///
    /// Our calc family is `Value::Raw("calc(...)")`: `calc-name` takes the function name (as
    /// a quoted string) and `calc-args` takes the arguments -- `calc` as a whole counts as one
    /// calculation, while the rest is split on top-level commas and re-evaluated (so the `1px`
    /// of `min(1px, 2%)` stays a number with a unit).
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
            // `min()` is kept as an unquoted string; `clamp()` is a CSS function passthrough.
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

    /// `variable-exists` / `global-variable-exists` / `function-exists` / `mixin-exists`:
    /// All except `variable-exists` accept `$module` (pointing at a `@use` namespace).
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
                _ => unreachable!("metafunctions exhausted"),
            };
            Ok(Value::Bool(found))
        };
        check(self).map_err(|message| EvalError::new(base + at, message))
    }

    /// `meta.module-functions($module)` / `meta.module-variables($module)`.
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

    /// `content-exists()`: can only be called inside a mixin body.
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

    /// `get-function($name, $css: false, $module: null)`: the name is validated **when the
    /// function is taken**, and a miss reports `Function not found: "x"` (1.35.2).
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

    /// `call($function, $args...)`: the first positional argument is the result of
    /// `get-function(...)`, and the rest bind as usual (user function -> built-in -> passed
    /// through as a CSS function when unknown).
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

    /// `variable-exists()` / `global-variable-exists()`: with `global_only` only the global
    /// frames are consulted.
    /// Variables brought in by `as *` count as well (observed with dart:
    /// `global-variable-exists()` is true).
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

    /// `ns.fn(...)`: a user module consults its function table, a built-in module goes through
    /// the `modules` dispatch; when neither has it / it is not a module function, the error
    /// follows dart.
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
                // 1.35.2: when the module has no such name at all it reports a bare
                // `Undefined function.`; the module-specific message is only for a name in the
                // member table that is not callable (`color.lighten`).
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

    /// Arguments of `@include` / `@content`: positional + keyword (evaluated in source
    /// order).
    /// `...` expands a list into positional arguments and a map into keyword arguments
    /// (observed with dart: other values count as a single positional argument).
    pub(crate) fn eval_invocation(&mut self, src: &str, args: &[ComponentValue]) -> R<Invocation> {
        let mut out = Invocation::default();
        let mut saw_keyword = false;
        // Group by top-level commas first: one group is one argument (a group may contain a
        // space-separated list).
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
                    // `$x...`: list -> positional arguments, map / ArgList -> keyword
                    // arguments, the rest counts as a single positional argument (observed with
                    // dart: `null...` is one null positional argument).
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
                            // Expanding reads the keyword map: dart counts that as "accessed" and
                            // therefore no longer reports extra named arguments.
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

    /// Arguments: evaluated in comma groups (each group is one expression), with `...`
    /// expansion using the same rules as `@include`.
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
            1 => positional.into_iter().next().expect("length 1"),
            _ => Value::List(List {
                items: positional,
                sep: Sep::Comma,
                bracketed: false,
            }),
        };
        Ok((value, keywords))
    }

    // -----------------------------------------------------------------------
    // User functions (@function / @return)
    // -----------------------------------------------------------------------

    /// Argument binding (dart `ArgumentDeclaration.verify` + binding order):
    /// positional arguments fill the parameters in order -> keywords fill by name ->
    /// defaults are evaluated in declaration order (they may refer to earlier parameters);
    /// `$rest...` collects the extra positional arguments (observed with dart: rest does not
    /// take keyword arguments).
    /// With `$rest`, extra named arguments are first collected into an ArgList and whether to
    /// report them is decided after the body has run (observed with dart: nothing is reported
    /// once anyone read the keyword map (`keywords($args)`)).
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
                let default = param.default.expect("validated to have a default value");
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

    /// Extra named arguments: reported after the body has run (nothing is reported when the
    /// body read the keyword map).
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

    /// Calling a user function: the frame stack is truncated to the definition depth (the
    /// caller's locals are invisible), and locals created in the body are dropped with the
    /// call; `!global` still writes the real global frame (observed with dart).
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
        // The spans of the function body (including default-value expressions) come from the
        // real source and only line up when base = 0.
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

    /// A function body: only variables / `@if` / `@each` / `@for` / `@while` / `@return` /
    /// `@debug` / `@warn` / `@error` (the rest is rejected by the static container check).
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
    // Loop iteration protocol (shared by Converter and function bodies)
    // -----------------------------------------------------------------------

    /// `@each`: expression -> the bound value of every iteration. null -> zero times; map ->
    /// each item is a `(k v)` pair; several variables destructure per element, missing
    /// positions are filled with `null` (observed with dart), extra elements are ignored.
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

    /// `@for`: `from`/`to` are evaluated into integers and the sequence is generated by
    /// direction (observed with dart: a lower bound greater than the upper one counts down,
    /// it is not an empty loop). The loop variable takes the unit spelling of `from`, and
    /// `to` is converted to the same unit first.
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

/// For `@each` / destructuring: null -> None; map -> a list of `(k v)` pairs; a single
/// value -> `[itself]`.
/// Keyword arguments -> the shape `modules` needs.
fn keywords_of(inv: &Invocation) -> Vec<(String, Value)> {
    inv.keywords
        .iter()
        .map(|(name, value, _)| (name.clone(), value.clone()))
        .collect()
}

/// Call a plain CSS function with `$args...`: with a single argument, use it directly
/// (`bar(1px)`).
fn css_function_call(name: &str, positional: Vec<Value>) -> Value {
    let args = match positional.len() {
        0 => Value::List(List {
            items: Vec::new(),
            sep: Sep::Comma,
            bracketed: false,
        }),
        1 => positional.into_iter().next().expect("length 1"),
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

/// The bounds of `@for` must be integers (wording of dart's `assertInt`).
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

/// Convert `to` into the unit of `from` (coerceUnitless); on a type mismatch, dart's
/// `Expected 3em to have a length unit (in, cm, pc, mm, q, pt, px).` message.
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

/// When a unit contains `-digit`, split it into two numbers (`10px-5px` -> `10px - 5px`).
fn unit_split_point(unit: &str) -> Option<usize> {
    let bytes = unit.as_bytes();
    (1..bytes.len()).find(|&i| bytes[i] == b'-' && bytes.get(i + 1).is_some_and(u8::is_ascii_digit))
}

/// Split arguments on top-level commas (nested parentheses are already gone at the
/// component level).
/// Split arguments on top-level commas. In oxc's `Function` arguments a comma is a separate
/// component, while `@include` removes the commas from `args` and leaves only span gaps -- so
/// both must be considered: an explicit comma component, or a `,` in the source gap between
/// adjacent components.
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
            // A comma only exists in the span gap (the arguments of `@include`), so the
            // current component starts the next group.
            groups.push(&args[start..i]);
            start = i;
        }
    }
    groups.push(&args[start..]);
    groups
}

// ---------------------------------------------------------------------------
// token flattening + precedence parsing
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum OpKind<'b> {
    Ast(&'b SassBinaryOperatorKind, &'b Span),
    /// A bare `/` in the component sequence (oxc does not fold it into a binary operation).
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
    /// Start of the expression (dart's operator errors all point at the start of the whole
    /// operation expression).
    fn start(&self) -> usize {
        match self {
            ExprKind::Val(comp) => comp.span().start,
            ExprKind::Unary(op, _) => op.span.start,
            ExprKind::Binary(op, _, _) => op.span().start,
            ExprKind::List(items, _, _) => items.first().map_or(0, ExprKind::start),
        }
    }

    /// dart-sass's allowSlash: the whole subtree consists only of `/` and literal numbers.
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
                1 => Ok(items.pop().expect("length 1")),
                _ => Ok(ExprKind::List(items, Sep::Comma, false)),
            }
        } else {
            match items.len() {
                0 => Ok(ExprKind::List(Vec::new(), Sep::Space, false)),
                1 => Ok(items.pop().expect("length 1")),
                _ => Ok(ExprKind::List(items, Sep::Space, false)),
            }
        }
    }

    /// A space-separated run: consecutive binary expressions.
    fn parse_space(&mut self) -> Result<ExprKind<'a, 'b>, ParseError> {
        let mut items = vec![self.parse_binary(0)?];
        while matches!(self.peek(), Some(Tok::Val(_)) | Some(Tok::Prefix(_))) {
            items.push(self.parse_binary(0)?);
        }
        if items.len() == 1 {
            return Ok(items.pop().expect("length 1"));
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

/// dart's `unvendor`: strip vendor prefixes such as `-webkit-` (only used to recognize
/// special function names).
fn unvendor(name: &str) -> &str {
    name.strip_prefix("-webkit-")
        .or_else(|| name.strip_prefix("-moz-"))
        .or_else(|| name.strip_prefix("-ms-"))
        .or_else(|| name.strip_prefix("-o-"))
        .unwrap_or(name)
}

/// Functions nested in `min()`/`max()` are treated as plain CSS and their contents are not
/// validated.
const CSS_MATH_FUNCS: [&str; 6] = ["calc", "clamp", "env", "var", "min", "max"];

impl<'a> Evaluator<'a> {
    /// The plain CSS form of `min()` / `max()` / `clamp()` (the calculation semantics of modern
    /// dart-sass).
    ///
    /// When an argument contains an atom that cannot be evaluated (an unknown function such
    /// as `var()` / `env()` / `constant()` / a bare identifier / an interpolation), the whole
    /// call passes through as CSS; when the whole argument is `calc()`, the redundant layer is
    /// stripped.
    /// When all arguments are numbers, the built-in function still evaluates it
    /// (`min(1px, 2px)` -> `1px`).
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

    /// One math function argument: when the whole segment is a `calc()`, strip the redundant
    /// layer.
    fn css_math_arg(&mut self, text: &str, at: usize) -> R<Option<(String, bool)>> {
        if let Some((inner, inner_at)) = whole_calc(text, at) {
            let Some(scan) = self.css_math_scan(inner, inner_at, 0, false)? else {
                return Ok(None);
            };
            if scan.end < inner.len() {
                return Ok(None);
            }
            // dart's `_needsParentheses`: after stripping `calc()` only one opaque value is
            // left, and parentheses are added when that value contains whitespace / `*` / `/`
            // or starts with `var(`
            // (`calc(var(--x))` -> `(var(--x))`, `calc(env(x))` -> `env(x)`).
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

    /// Evaluate a function call as SassScript; return only when the result is a number with a
    /// simple unit.
    /// `var()`/`env()`/unknown functions yield a non-number (or an error), and the caller
    /// passes them through as plain CSS.
    fn sass_math_number(&mut self, call: &str, at: usize) -> Option<Number> {
        match self.eval_text(call, at).ok()? {
            Value::Number(number) if number.is_simple() => Some(number.without_slash()),
            _ => None,
        }
    }

    /// All-numeric arguments: evaluated by the built-in `min`/`max`; incomparable units and
    /// the like return `None` (passed through as CSS).
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

    /// SassScript inside `calc()`: inline variable values and Sass function calls, keep the
    /// rest as is.
    ///
    /// Interpolation results are wrapped in `INTERP_OPEN`/`INTERP_CLOSE`: while simplifying
    /// they are opaque atoms (dart's `CalculationInterpolation` takes no part in evaluation),
    /// but the numbers next to them can still be merged.
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
                // A qualified name such as `map.get(...)` is one Sass call as a whole.
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
                            // User function / Sass built-in: evaluate and inline as CSS text.
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
            let ch = text[i..].chars().next().expect("non-empty");
            out.push(ch);
            i += ch.len_utf8();
        }
        Ok(out)
    }

    /// Scan the content of a CSS math function. When `in_paren` is true, stop at a `)`
    /// (without consuming it).
    /// Return `None` when the syntax does not match the CSS production.
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
                    // Interpolation is a value known only at evaluation time: dart does no
                    // simplification and passes the whole thing through as CSS.
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
                    // A left parenthesis followed by whitespace, such as `( 1px`, fails to parse
                    // in dart.
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
                        // Adjacent numbers such as `1px 2px` are illegal; `bar 1px` is a valid
                        // space sequence.
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
                    // A space sequence (`2px bar`) is legal in dart: the identifier itself makes
                    // the whole thing an opaque value.
                    if value_pending {
                        out.push(' ');
                    }
                    let end = ident_end(text, i);
                    let name = text[i..end].to_ascii_lowercase();
                    if bytes.get(end) != Some(&b'(') {
                        // A bare identifier (`bar`, `e`): an opaque value, so the whole thing
                        // passes through as CSS.
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
                        // A known Sass function (`lightness($c)` / `map.get(…)`): evaluated into
                        // a number and inlined, taking part in CSS math as usual (dart evaluates
                        // arguments as SassScript by default).
                        if value_pending {
                            if !prev_opaque {
                                return Ok(None);
                            }
                            out.push(' ');
                        }
                        out.push_str(&number.format());
                        prev_opaque = false;
                    } else {
                        // `var()` / `env()` / nested `calc()` / unknown functions
                        // (`constant()`): inlined as is (only interpolation and whitespace
                        // collapsing), and none of them can be evaluated.
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

/// The result of one scan of CSS math function content.
struct MathScan {
    text: String,
    end: usize,
    /// Contains an atom that cannot be evaluated (`var()` / `env()` / unknown function / bare
    /// identifier / interpolation).
    opaque: bool,
    /// An operator appeared (decides whether parentheses are needed when stripping
    /// `calc()`).
    operators: bool,
}

/// When the whole text is exactly one `calc(...)`, return the content inside the parentheses
/// and its source start.
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

/// dart's `_needsParentheses`: whether the content of `calc()` needs parentheses when
/// nested into another calculation.
fn needs_parentheses(text: &str) -> bool {
    text.chars()
        .any(|c| c.is_whitespace() || c == '/' || c == '*')
        || text
            .get(..4)
            .is_some_and(|h| h.eq_ignore_ascii_case("var("))
}

// ---------------------------------------------------------------------------
// calc constant folding
// ---------------------------------------------------------------------------

/// The simplification result of `calc()` content.
enum CalcFold {
    /// The whole thing is numeric: `calc(1px + 2px)` -> `3px` (the `calc()` goes away too).
    Number(Number),
    /// The simplified calculation text (still needs an outer `calc()`).
    Text(String),
    /// Out of the supported range: kept as is.
    Verbatim,
}

/// A calculation AST. Parentheses are kept only when they wrap "a single atom" (dart keeps
/// `calc((var(--x)))` as well) and dropped when they wrap an operation, whose precedence is
/// recomputed by serialization.
enum CalcNode {
    Num(Number),
    Opaque(String),
    Op(char, Box<CalcNode>, Box<CalcNode>),
    Paren(Box<CalcNode>),
}

/// Boundary of an interpolation result (the `calc_script` marker; removed once
/// simplification is done).
const INTERP_OPEN: char = '\u{1}';
const INTERP_CLOSE: char = '\u{2}';
const INTERP_OPEN_BYTE: u8 = 1;

fn strip_interp_marks(text: &str) -> String {
    text.replace([INTERP_OPEN, INTERP_CLOSE], "")
}

/// The text after constant folding; returned as is when it cannot be simplified.
fn fold_calc_text(text: String) -> String {
    match fold_calc(&text) {
        CalcFold::Number(number) => number.format(),
        CalcFold::Text(folded) => folded,
        CalcFold::Verbatim => text,
    }
}

/// Simplify a piece of `calc()` content (whitespace already collapsed), with the same rules
/// as dart's `Calculation.simplify`: purely numeric subexpressions are merged by the Sass
/// unit rules, the rest is re-serialized by precedence, and parentheses are written as a
/// nested `calc()` when needed (old WebKit does not fully support bare parentheses inside
/// calc).
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
        // `calc()` nested in a calculation: expand it into a subexpression (dart's calculation
        // is flat).
        let mut sub = CalcParser { text: inner, i: 0 };
        let node = sub.sum()?;
        sub.skip_ws();
        (sub.i == inner.len()).then_some(node)
    }
}

/// Unary minus: a number is negated directly, the rest becomes `-1 * x` the dart way.
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
        // `calc((1em*0.5))` folds into a number; `calc((var(--x)))` keeps its parentheses
        // (observed with dart).
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
    // `a + -2px` -> `a - 2px`, `a - -2px` -> `a + 2px` (dart normalizes the same way).
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

/// Add/subtract/multiply/divide two numbers; return `None` when the units cannot be merged
/// or the result has a compound unit (the original expression is kept).
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

/// A same-precedence operation on the right-associative side needs parentheses
/// (`a - (b - c)`); written as a nested `calc()` when needed.
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

/// Parse `1.5px` / `50%` / `1e3px`: returns (value, unit, number of bytes consumed).
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

/// End position of a number (including sign, fraction, exponent) plus its unit.
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

/// End of `#{...}` (including the position after `}`).
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

/// The `)` position matching the `(` at `open`.
fn matching_paren(text: &str, open: usize) -> Option<usize> {
    crate::text::matching(text, open).map(|end| end - 1)
}

/// Collapse consecutive whitespace into one space (leading/trailing ones are not trimmed).
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

/// Inline a Sass value inside `calc()`: strings lose their quotes, the rest uses CSS text.
fn calc_text(value: &Value) -> Result<String, EvalError> {
    let text = match value {
        Value::Str(text) => text.text.clone(),
        other => other
            .to_css()
            .map_err(|message| EvalError { message, at: 0 })?,
    };
    Ok(text)
}

/// `calc(...)` / `min(...)` and the like -> (function name, text inside the parentheses).
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

/// Split on top-level separators (ignoring separators inside parentheses / quotes /
/// comments).
fn split_top_level(text: &str, sep: char) -> Vec<&str> {
    crate::text::split_top_level(text, sep as u8)
        .into_iter()
        .map(|(start, end)| &text[start..end])
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The mixin / function lookup of S2 reuses this normalization (dart: `m_x` ≡ `m-x`,
    /// but `un_quote` ≠ `unquote`).
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
