use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use oxc_css_parser::ast::*;
use oxc_css_parser::pos::Span;

use super::Compiler;
use super::emit::merge_groups;
use super::eval::{callee_raw_tokens, param_name, params_of};
use super::guard::Val;
use super::selector::selector_tokens;
use super::value::{self, Color, PropSource, VarRef, eval};

/// One variable table (for mixin closures and expansion injection).
pub(super) type Vars<'a> = HashMap<String, VarRef<'a>>;

/// Scopes injected during mixin expansion, from outer to inner: closure frame, parameter frame, body frame.
pub(super) type Inject<'a> = Rc<Vec<Vars<'a>>>;

/// One body part of a ruleset: the block of a detached ruleset or of a mixin definition.
#[derive(Clone)]
pub(crate) struct RulePart<'a> {
    pub(crate) block: &'a SimpleBlock<'a>,
    /// Captured frames of this body (outer → inner; excluding the body's own variable frame)
    pub(crate) frames: Inject<'a>,
}

/// A detached ruleset (`@dr: {...}`) or the result of a mixin call used as a ruleset.
///
/// A mixin with the same name can have several definitions: less's `MixinCall.eval` concatenates all matching bodies
/// in definition order into one ruleset, so lookup is a merged lookup — later definitions win, and items missing from a later definition fall back to the earlier one.
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

/// One declaration for lookup: the declaration itself + the scope of its body (already including the body variable frame and property table).
pub(super) struct FlatRule<'a> {
    pub(super) decl: Option<&'a Declaration<'a>>,
    pub(super) var: Option<&'a LessVariableDeclaration<'a>>,
    pub(super) scope: Inject<'a>,
    pub(super) props: Rc<HashMap<String, Vec<PropSource<'a>>>>,
}

/// First-level result of a namespace lookup (`@p[key]` may be a value or a nested ruleset).
pub(super) enum Lookup<'a> {
    Text(String),
    Ruleset(RuleSet<'a>),
}

/// A mixin definition (a parenthesized definition or a single-selector rule).
#[derive(Clone)]
pub(super) struct MixinDef<'a> {
    /// Absolute selector token path (including itself), interpolated in the defining scope at registration time
    pub(super) tokens: Vec<String>,
    /// Tokens occupied by itself
    pub(super) own: Vec<String>,
    /// Same length as `tokens`: whether this level can be entered with zero arguments as a namespace chain element
    pub(super) chain: Vec<bool>,
    /// Definitions without parentheses have no parameter list
    pub(super) params: Option<&'a LessMixinParameters<'a>>,
    /// The definition's own guard (`when (...)`)
    pub(super) guard: Option<&'a LessConditions<'a>>,
    /// Guards along the namespace path together with their evaluation scope
    pub(super) conds: Vec<(&'a LessConditions<'a>, Inject<'a>)>,
    pub(super) block: &'a SimpleBlock<'a>,
    /// Variable scope chain at the definition site (outer → inner); the closure
    pub(super) closure: Inject<'a>,
}

#[derive(Clone)]
pub(super) struct Frame<'a> {
    pub(super) vars: HashMap<String, VarRef<'a>>,
    /// Property declarations of this level (for `$prop` lookup)
    pub(super) props: HashMap<String, Vec<PropSource<'a>>>,
    /// Block comment at the end of a variable declaration value (less treats comments as value nodes and carries them to the reference site)
    pub(super) suffixes: HashMap<String, String>,
}

#[derive(Clone)]
pub(super) struct Env<'a> {
    pub(super) frames: Vec<Frame<'a>>,
    /// Lookup roots for file functions (entry file directory + `paths`), propagated with scope copies.
    pub(super) roots: Option<Rc<Roots>>,
    /// Lookups that missed during evaluation (less throws on the same input; they are collected here and reported together).
    pub(super) misses: Rc<RefCell<Vec<(String, Span)>>>,
    /// Disables undefined-variable errors when the source has `@plugin`: plugins can inject variables during evaluation.
    pub(super) plugins: bool,
}

/// Relative path base for `data-uri` / `image-size`.
///
/// less uses `currentFileInfo.entryPath` (when rewriteUrls is off) — that is the directory of **the entry file**,
/// not the file currently being inlined; `@import` is already inlined as text, so this directory information can only be carried into the evaluation layer from here.
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
    /// Pushes the injection scopes of a mixin expansion (closure + parameters + body variables).
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
    /// Same as above, additionally recording the block comment at the end of the value (carried to the reference site).
    pub(super) fn set_var(
        &mut self,
        name: String,
        node: &'a ComponentValue<'a>,
        suffix: Option<String>,
    ) {
        if let Some(f) = self.frames.last_mut() {
            f.vars.insert(name.clone(), VarRef::Ast(node));
            if let Some(suffix) = suffix {
                f.suffixes.insert(name, suffix);
            }
        }
    }
    /// Writes an already evaluated text variable (for mixin body variables leaking into the calling block).
    pub(super) fn set_text(&mut self, name: String, value: String) {
        if let Some(f) = self.frames.last_mut() {
            f.vars.insert(name, VarRef::Text(value));
        }
    }
    /// Scope copy with the injection frames layered on (evaluated in the ruleset body's scope).
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
    /// Scope copy with an extra property table layered on (for `$prop` in a detached ruleset body looking up declarations in the same block).
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
    /// Unresolved variable reference: less reports a NameError and the caller reports it uniformly (except when `@plugin` is present).
    pub(super) fn report_var_miss(&self, name: &str, span: Span) {
        if !self.plugins {
            self.miss(format!("variable @{name} is undefined"), span);
        }
    }
    /// Records one semantic error during evaluation (a missed variable/property lookup).
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
    /// Snapshot of the currently visible variable frames (outer → inner).
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

/// Body variables: only top-level declarations of the body (lazy, later definitions override), used for the injection frame during expansion.
pub(super) fn body_vars<'a>(block: &'a SimpleBlock<'a>) -> Vars<'a> {
    let mut vars = Vars::new();
    for st in &block.statements {
        if let Statement::LessVariableDeclaration(d) = st {
            vars.insert(d.name.name.name.to_string(), VarRef::Ast(&d.value));
        }
    }
    vars
}

/// Uses the frame stack directly as a scope (avoiding a full-stack clone on every lookup).
pub(super) struct Frames<'a, 'b>(pub(super) &'b [Vars<'a>]);

impl<'a> value::Scope<'a> for Frames<'a, '_> {
    fn var(&self, name: &str) -> Option<VarRef<'a>> {
        self.0.iter().rev().find_map(|f| f.get(name).cloned())
    }

    fn prop(&self, _: &str) -> Option<Vec<PropSource<'a>>> {
        None
    }
}

/// Mixin definition table: bucketed by own name (last token) to avoid a full scan on every call.
#[derive(Default)]
pub(super) struct Defs<'a> {
    pub(super) list: Vec<MixinDef<'a>>,
    pub(super) index: HashMap<String, Vec<usize>>,
}

impl<'a> Defs<'a> {
    /// Registers a definition; when the same (body, token chain) repeats, the first registration is kept (the closure is equivalent).
    pub(super) fn insert(&mut self, def: MixinDef<'a>) {
        let bucket = self
            .index
            .entry(def.tokens.last().cloned().unwrap_or_default())
            .or_default();
        // Re-registering the same (body, token chain) only refreshes the closure: dynamically registered definitions update on every expansion
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

/// A call argument (value already evaluated).
pub(super) struct CallArg<'a> {
    pub(super) name: Option<String>,
    pub(super) text: String,
    /// Elements of a space-separated value list in this argument (`length(@arguments)` relies on it)
    pub(super) items: Vec<String>,
    pub(super) val: Val,
    /// Floating-point channels of a color argument (preserved when binding mixin parameters to avoid precision loss in `#rrggbb` round trips)
    pub(super) color: Option<Color>,
    /// The body carried when the argument is a ruleset (detached ruleset / argument-less mixin call)
    pub(super) ruleset: Option<Rc<RuleSet<'a>>>,
    /// Unevaluated division (kept as an Operation under math=parens-division): the parameter is bound to the AST,
    /// so it is evaluated inside `(@b)` and still emitted verbatim outside parentheses
    pub(super) division: Option<&'a ComponentValue<'a>>,
}

/// Candidate definition + bound parameters (the guard result decides whether it expands).
pub(super) struct Candidate<'a> {
    pub(super) def: MixinDef<'a>,
    /// The group obtained from the two-pass `default()` evaluation
    pub(super) group: u8,
    pub(super) frame: Vars<'a>,
    pub(super) arguments: Vec<String>,
}

impl<'a> Compiler<'a> {
    /// Value node → ruleset (detached ruleset / argument-less mixin call / variable / nested lookup).
    pub(super) fn ruleset_of(
        &self,
        node: &'a ComponentValue<'a>,
        depth: usize,
    ) -> Option<RuleSet<'a>> {
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

    /// Block-level ruleset: captures the current scope (a detached ruleset's environment is the one at the moment it is evaluated).
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

    /// Argument-less mixin call → the ruleset for its body (`each(.set(), …)` / `@p: .mk-map()` / `#ns[prop]`).
    ///
    /// All matching definitions are merged in definition order (less `MixinCall.eval` concatenates the rules of every candidate).
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
                // Bind arguments to parameters by position/name so `@param` references in the body can be evaluated during lookup
                let (frame, arguments, _) = self.bind_call_args(&def, &args);
                RulePart {
                    block: def.block,
                    frames: self.make_inject(&def, frame, arguments),
                }
            })
            .collect();
        (!parts.is_empty()).then_some(RuleSet { parts })
    }

    /// Definitions matched by a call: all candidates at the innermost frame (filtered by arguments and guard), in definition order.
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

    /// Binds arguments to parameters by position/name (namespace lookup only needs text and guard values).
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

    /// Evaluation scope of one body part: captured frames + body variable frame (later body variable declarations win).
    pub(super) fn part_scope(&self, part: &RulePart<'a>) -> Inject<'a> {
        let mut frames = (*part.frames).clone();
        frames.push(body_vars(part.block));
        Rc::new(frames)
    }

    /// Full evaluation environment of one body part: scope frames + this level's property table (`$prop` looks up declarations in the same block).
    pub(super) fn rule_env(&self, rule: &FlatRule<'a>) -> Env<'a> {
        let mut env = self.env.scoped(&rule.scope);
        if let Some(frame) = env.frames.last_mut() {
            frame.props = (*rule.props).clone();
        }
        env
    }

    /// Property table of a block level (for `$prop` / `[]` lookup).
    pub(super) fn block_props(
        &self,
        block: &'a SimpleBlock<'a>,
    ) -> HashMap<String, Vec<PropSource<'a>>> {
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

    /// Rule sequence for lookup: nested mixin / detached ruleset calls are expanded in place
    /// (less already concatenates mixin output into the rule array during `Ruleset.eval`).
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

    pub(super) fn namespace_lookup(
        &self,
        ns: &'a LessNamespaceValue<'a>,
        depth: usize,
    ) -> Option<Lookup<'a>> {
        self.namespace_lookup_checked(ns, depth).ok()
    }

    /// Namespace lookup (with error messages): missed variables/properties are a NameError in less,
    /// and the message is returned to the caller here (previously they were silently treated as empty values).
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

    /// First-level lookup for `@p[text]` / `#ns[prop]` (following less `NamespaceValue.eval`).
    pub(super) fn lookup_in(
        &self,
        rs: &RuleSet<'a>,
        lookup: &LessLookup<'a>,
        depth: usize,
    ) -> Result<Lookup<'a>, String> {
        let name = match &lookup.name {
            // An empty lookup (`@p[]`) takes the value of the last declaration
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

    pub(super) fn ruleset_var(
        &self,
        rs: &RuleSet<'a>,
        name: &str,
        depth: usize,
    ) -> Option<Lookup<'a>> {
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

    /// Variable declaration value → ruleset; a detached ruleset body uses the scope of the declaring body (not the call site).
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

    pub(super) fn ruleset_prop(
        &self,
        rs: &RuleSet<'a>,
        name: &str,
        depth: usize,
    ) -> Option<Lookup<'a>> {
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

    /// less `Ruleset.lastDeclaration`: the last declaration (variable declarations count too — `@r: 20px` is
    /// a variable declaration); an empty `[]` lookup takes its value.
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

    /// Final text of a namespace lookup (a ruleset result takes the last declaration as less does).
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
