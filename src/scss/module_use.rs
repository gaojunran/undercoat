//! 模块系统：`@use` / `@forward`、命名空间与成员过滤、冲突检查。
//!
//! 由 `scss/mod.rs` 拆分而来（纯搬运）。

use super::*;

impl<'a> Converter<'a> {
    /// `@use`：注册命名空间（`as` / `as *`），`with` 配置交给目标模块。
    /// 语句本身不产出 CSS。
    pub(super) fn use_rule(&mut self, at: &oxc_css_parser::ast::AtRule<'a>) -> Result<(), Box<CompileError>> {
        let spec = self.module_spec(at)?;
        let url = spec.path.as_str();
        let file = self.file_at(at.span.start);
        let alias = spec.alias.map(|(text, _)| text).unwrap_or("");
        // `@use ... show/hide` 还没实现：显式报错，避免把过滤掉的成员静默暴露出去。
        if spec.filter.is_some() {
            return Err(self.err(at.span.start, "SCSS 特性尚未实现（S4）: @use ... show/hide"));
        }
        // 内置模块：**不参与文件解析**，直接得到一个静态成员表。
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
            // 只并入本文件的作用域：不写进模块的对外成员表，否则下游
            // `@use this as *` 会读到并不属于本模块的成员。
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

    /// `@forward`：把目标模块的成员导出给下游（惰性解析，本模块内不可见）。
    /// `with` 的配置在目标模块求值前登记；`!default` 表示调用方可以覆盖。
    pub(super) fn forward_rule(
        &mut self,
        at: &oxc_css_parser::ast::AtRule<'a>,
    ) -> Result<(), Box<CompileError>> {
        let spec = self.module_spec(at)?;
        if spec.path.starts_with("sass:") {
            return Err(self.err(
                at.span.start,
                "SCSS 特性尚未实现（S4）: @forward \"sass:*\"",
            ));
        }
        let module = self.loaded_module(at, &spec.path)?;
        let prefix = spec
            .alias
            .map(|(text, _)| text.strip_suffix('*').unwrap_or("").to_string())
            .unwrap_or_default();
        // `show` / `hide` 名单在 `as p-*` 下写的是**加前缀之后**的名字（dart 实测）。
        let (show, hide) = spec.filter.as_ref().map(member_filter).unwrap_or_default();
        let edge = eval::ForwardEdge {
            scope: module.scope,
            at: at.span.start,
            prefix,
            show,
            hide,
        };

        // 先应用本语句自己的 `with`（`!default` 是转发默认值，调用方可覆盖）。
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

        // 下游对本模块的配置沿转发边下传（配置本体仍留在本模块：目标模块不声明这个
        // 变量时，本模块后面的 `!default` 声明照样消费它——dart 的 `Configuration`
        // 是共享对象，谁先消费谁生效）。
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
        // `@import` 内联进来的文件没有自己的作用域：它 `@forward` 的成员在导入处
        // 也必须是可见的（dart 实测变量 / 函数 / mixin 都可见）。
        let file = self.file_at(at.span.start);
        if self.module_file.get(&scope).copied().unwrap_or(0) != file {
            self.eval.import_star_module(module.scope);
        }
        self.eval.add_forward(scope, edge);
        Ok(())
    }

    /// 两个不同的模块被转发到同一作用域且定义了同名成员时报错（dart 实测；
    /// 同一模块经菱形转发出现两次不算冲突）。
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

    /// 语句自己的 `@use` / `@forward` prelude。
    fn module_spec(
        &self,
        at: &oxc_css_parser::ast::AtRule<'a>,
    ) -> Result<imports::ModuleSpec<'a>, Box<CompileError>> {
        let src = self.src();
        // oxc 的 at-rule span 会截断 prelude，语句尾自己扫到顶层 `;`。
        let end = imports::statement_end(src, at.span.end);
        let text = src.get(at.span.start..end).unwrap_or_default();
        imports::parse_module_spec(text, at.span.start)
            .map_err(|(at, message)| self.err(at, message))
    }

    /// `@use` / `@forward` 的 URL → 已装配的模块（装配层已递归载入并记好作用域）。
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

/// `@use` 默认命名空间：文件名去掉前导 `_` 与扩展名。
/// `@forward` 的 show/hide 名单 → 求值器的过滤器（名字归一化）。
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
