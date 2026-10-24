//! Module system: `@use` / `@forward`, namespaces and member filtering, conflict
//! checks.
//!
//! Split out of `scss/mod.rs` (pure code move).

use super::*;

impl<'a> Converter<'a> {
    /// `@use`: registers a namespace (`as` / `as *`); the `with` configuration goes
    /// to the target module. The statement itself emits no CSS.
    pub(super) fn use_rule(
        &mut self,
        at: &oxc_css_parser::ast::AtRule<'a>,
    ) -> Result<(), Box<CompileError>> {
        let spec = self.module_spec(at)?;
        let url = spec.path.as_str();
        let file = self.file_at(at.span.start);
        let alias = spec.alias.map(|(text, _)| text).unwrap_or("");
        // `@use ... show/hide` is not implemented yet: error out explicitly so the
        // filtered members are not silently exposed.
        if spec.filter.is_some() {
            return Err(self.err(
                at.span.start,
                "SCSS feature not implemented yet (S4): @use ... show/hide",
            ));
        }
        // Built-in modules: **not resolved from files**, they directly provide a
        // static member table.
        if let Some(module) = modules::Builtin::from_url(url) {
            if spec.config.is_some() {
                return Err(self.err(at.span.start, "Built-in modules can't be configured."));
            }
            if alias == "*" {
                self.eval.import_builtin_star(module);
                return Ok(());
            }
            let namespace = if alias.is_empty() {
                module.default_namespace().to_string()
            } else {
                alias.to_string()
            };
            return self
                .eval
                .add_builtin_namespace(&namespace, module, file)
                .map_err(|e| self.eval_err(e));
        }
        let module = self.loaded_module(at, url)?;
        if let Some(config) = &spec.config {
            for entry in
                imports::config_entries(config).map_err(|(at, message)| self.err(at, message))?
            {
                if entry.default {
                    return Err(self.err(entry.default_at, "expected \")\"."));
                }
                let value = self
                    .eval
                    .eval_text(entry.value, entry.value_at)
                    .map_err(|e| self.eval_err(e))?;
                if let Err(at) =
                    self.eval
                        .configure(module.scope, entry.name, value, entry.at, false)
                {
                    return Err(self.err(
                        at,
                        "This variable was not declared with !default in the @used module.",
                    ));
                }
            }
        }
        if alias == "*" {
            // Merge into this file's scope only: not into the module's exported
            // member table, otherwise a downstream `@use this as *` would see
            // members that don't belong to this module.
            self.eval.import_star_module(module.scope);
            return Ok(());
        }
        let namespace = if alias.is_empty() {
            default_namespace(Path::new(url))
        } else {
            alias.to_string()
        };
        self.eval
            .add_namespace(&namespace, module.scope, file)
            .map_err(|e| self.eval_err(e))
    }

    /// `@forward`: exports the target module's members to downstream code (lazy
    /// resolution, not visible inside this module).
    /// The `with` configuration is registered before the target module is
    /// evaluated; `!default` means the caller may override it.
    pub(super) fn forward_rule(
        &mut self,
        at: &oxc_css_parser::ast::AtRule<'a>,
    ) -> Result<(), Box<CompileError>> {
        let spec = self.module_spec(at)?;
        if spec.path.starts_with("sass:") {
            return Err(self.err(
                at.span.start,
                "SCSS feature not implemented yet (S4): @forward \"sass:*\"",
            ));
        }
        let module = self.loaded_module(at, &spec.path)?;
        let prefix = spec
            .alias
            .map(|(text, _)| text.strip_suffix('*').unwrap_or("").to_string())
            .unwrap_or_default();
        // Under `as p-*` the `show` / `hide` lists name members **after** the prefix
        // is applied (observed with dart).
        let (show, hide) = spec.filter.as_ref().map(member_filter).unwrap_or_default();
        let edge = eval::ForwardEdge {
            scope: module.scope,
            at: at.span.start,
            prefix,
            show,
            hide,
        };

        // Apply this statement's own `with` first (`!default` is the forwarded
        // default, which the caller may override).
        if let Some(config) = &spec.config {
            for entry in
                imports::config_entries(config).map_err(|(at, message)| self.err(at, message))?
            {
                let value = self
                    .eval
                    .eval_text(entry.value, entry.value_at)
                    .map_err(|e| self.eval_err(e))?;
                if let Err(at) =
                    self.eval
                        .configure(module.scope, entry.name, value, entry.at, entry.default)
                {
                    return Err(self.err(
                        at,
                        "This variable was not declared with !default in the @used module.",
                    ));
                }
            }
        }

        // Downstream configurations of this module are passed down along forward
        // edges (the configuration itself stays in this module: when the target
        // module does not declare the variable, a later `!default` declaration here
        // still consumes it -- dart's `Configuration` is a shared object, whoever
        // consumes first wins).
        let scope = self.eval.active_scope();
        for mut entry in self.eval.pending_configs(scope) {
            if self.eval.config_used(entry.id) {
                continue;
            }
            if !edge.allows(eval::MemberKind::Var, &entry.name) {
                continue;
            }
            let Some(inner) = edge.strip(&entry.name) else {
                continue;
            };
            entry.name = eval::normalize_name(inner);
            if let Err(at) = self.eval.push_config(module.scope, entry) {
                return Err(self.err(
                    at,
                    "This variable was not declared with !default in the @used module.",
                ));
            }
        }
        // A file inlined by `@import` has no scope of its own: the members it
        // forwards must also be visible at the import site (observed with dart for
        // variables, functions and mixins).
        let file = self.file_at(at.span.start);
        if self.module_file.get(&scope).copied().unwrap_or(0) != file {
            self.eval.import_star_module(module.scope);
        }
        self.eval.add_forward(scope, edge);
        Ok(())
    }

    /// Error when two different modules are forwarded into the same scope and define
    /// a member with the same name (observed with dart; the same module appearing
    /// twice through a diamond forward is not a conflict).
    pub(super) fn check_forward_conflicts(&self) -> Option<Box<CompileError>> {
        for scope in 0..self.eval.scope_count() {
            let mut vars: HashMap<String, (usize, String)> = HashMap::new();
            let mut funcs: HashMap<String, (usize, String)> = HashMap::new();
            for edge in self.eval.forwards(scope) {
                for (name, _) in self.eval.export_var_names(edge.scope) {
                    let outward = format!("{}{}", edge.prefix, name);
                    if !edge.allows(eval::MemberKind::Var, &outward) {
                        continue;
                    }
                    let origin = self.eval.export_var_origin(edge.scope, &name);
                    if let Some(origin) = origin {
                        if let Some(prev) = vars.get(&outward) {
                            if *prev != origin {
                                return Some(
                                    self.merged
                                        .error_at(
                                            edge.at,
                                            format!(
                                                "Two forwarded modules both define a variable named ${outward}."
                                            ),
                                        )
                                        .into(),
                                );
                            }
                        } else {
                            vars.insert(outward, origin);
                        }
                    }
                }
                let mut mixin_names = HashMap::new();
                self.export_mixin_origins(edge.scope, 0, &mut mixin_names);
                for (name, origin) in mixin_names {
                    let outward = format!("{}{}", edge.prefix, name);
                    if !edge.allows(eval::MemberKind::Member, &outward) {
                        continue;
                    }
                    if let Some(prev) = funcs.get(&outward) {
                        if *prev != origin {
                            return Some(
                                self.merged
                                    .error_at(
                                        edge.at,
                                        format!(
                                            "Two forwarded modules both define a function named {outward}."
                                        ),
                                    )
                                    .into(),
                            );
                        }
                    } else {
                        funcs.insert(outward, origin);
                    }
                }
                for name in self.eval.export_function_names(edge.scope) {
                    let outward = format!("{}{}", edge.prefix, name);
                    if !edge.allows(eval::MemberKind::Member, &outward) {
                        continue;
                    }
                    let Some(origin) = self.eval.export_function_origin(edge.scope, &name) else {
                        continue;
                    };
                    if let Some(prev) = funcs.get(&outward) {
                        if *prev != origin {
                            return Some(
                                self.merged
                                    .error_at(
                                        edge.at,
                                        format!(
                                            "Two forwarded modules both define a function named {outward}."
                                        ),
                                    )
                                    .into(),
                            );
                        }
                    } else {
                        funcs.insert(outward, origin);
                    }
                }
            }
        }
        None
    }

    /// The statement's own `@use` / `@forward` prelude.
    fn module_spec(
        &self,
        at: &oxc_css_parser::ast::AtRule<'a>,
    ) -> Result<imports::ModuleSpec<'a>, Box<CompileError>> {
        let src = self.src();
        // oxc's at-rule span truncates the prelude, so scan to the top-level `;`
        // ourselves at the end of the statement.
        let end = imports::statement_end(src, at.span.end);
        let text = src.get(at.span.start..end).unwrap_or_default();
        imports::parse_module_spec(text, at.span.start)
            .map_err(|(at, message)| self.err(at, message))
    }

    /// `@use` / `@forward` URL -> the assembled module (the assembly layer has
    /// already loaded it recursively and recorded its scope).
    fn loaded_module(
        &self,
        at: &oxc_css_parser::ast::AtRule<'a>,
        url: &str,
    ) -> Result<ModuleInfo, Box<CompileError>> {
        let file_id = self.file_at(at.span.start) as usize;
        let dir = self
            .merged
            .files
            .get(file_id)
            .and_then(|f| f.path.parent())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let resolved = imports::resolve(&dir, url, &self.opts.paths)
            .map_err(|e| self.err(at.span.start, e.message(url)))?;
        let canonical = std::fs::canonicalize(&resolved).unwrap_or_else(|_| resolved.clone());
        self.modules.get(&canonical).copied().ok_or_else(|| {
            self.err(
                at.span.start,
                format!("Can't find stylesheet to import: {}", resolved.display()),
            )
        })
    }
}

/// Default namespace for `@use`: the file name without the leading `_` and
/// without the extension.
/// `@forward` show/hide lists -> the evaluator's filter (names normalized).
fn member_filter(filter: &imports::Filter<'_>) -> (Option<eval::MemberFilter>, eval::MemberFilter) {
    let list = eval::MemberFilter {
        vars: filter
            .vars
            .iter()
            .map(|name| eval::normalize_name(name))
            .collect(),
        members: filter
            .members
            .iter()
            .map(|name| eval::normalize_name(name))
            .collect(),
    };
    if filter.show {
        (Some(list), eval::MemberFilter::default())
    } else {
        (None, list)
    }
}

fn default_namespace(path: &Path) -> String {
    let name = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    name.trim_start_matches('_').to_string()
}
