//! SCSS 文件装配：`@import` 文本级内联 + `@use` 模块加载。
//!
//! 装配产物是一段**合并文本**外加一张分段表（[`crate::merge::Seg`]），每个分段映射回
//! 「哪个文件 + 文件内偏移」。后续所有阶段（求值、打印、sourcemap）都只看
//! 合并文本的偏移，出错时再由分段表换算出文件与行列。
//!
//! 为什么是文本级：`@import` 的语义就是「把文件内容原样搬到这个位置、共享
//! 作用域」，splice 之后嵌套与变量共享都自然成立；`@use` 则相反，模块有独立
//! 作用域、CSS 只输出一次，所以这里只负责把模块内容排到依赖者之前并登记去重表。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use oxc_css_parser::ast::*;
use oxc_css_parser::pos::Span;
use oxc_css_parser::{Allocator, ParserBuilder, Syntax};

use crate::merge::{Merged, Part};
use crate::{CompileError, LineIndex, Options};

use super::indented;

/// `@use` 加载到的模块：文件 id 与独立作用域编号。
#[derive(Debug, Clone, Copy)]
pub(crate) struct ModuleInfo {
    pub(crate) file: u32,
    pub(crate) scope: usize,
}

pub(crate) struct Loaded {
    pub(crate) merged: Merged,
    pub(crate) deps: Vec<String>,
    pub(crate) modules: HashMap<PathBuf, ModuleInfo>,
    /// 文件 id → 作用域编号（`@import` 内联的文件沿用导入者的作用域）。
    pub(crate) scope_of: HashMap<u32, usize>,
    /// 作用域个数（0 = 根文件）。
    pub(crate) scopes: usize,
}

pub(crate) enum ResolveError {
    Ambiguous(Vec<PathBuf>),
    NotFound,
}

impl ResolveError {
    pub(crate) fn message(&self, url: &str) -> String {
        match self {
            ResolveError::Ambiguous(found) => {
                let mut list = String::from("It's not clear which file to import. Found:");
                for p in found {
                    list.push_str(&format!("\n  {}", p.display()));
                }
                list
            }
            ResolveError::NotFound => {
                if url.starts_with("sass:") {
                    format!("SCSS 特性尚未实现（S1）: 内置模块 @use \"{url}\"")
                } else {
                    "Can't find stylesheet to import.".to_string()
                }
            }
        }
    }
}

pub(crate) struct Loader<'a> {
    opts: &'a Options,
    root: PathBuf,
    /// 入口文件的在内存文本（`compile(.., source, ..)` 不落盘）。
    entry: String,
    /// 已登记的文件与合并串（`run` 时拼装）。
    merged: Merged,
    deps: Vec<String>,
    modules: HashMap<PathBuf, ModuleInfo>,
    scope_of: HashMap<u32, usize>,
    next_scope: usize,
    stack: Vec<PathBuf>,
}

impl<'a> Loader<'a> {
    pub(crate) fn new(root: &Path, source: &str, opts: &'a Options) -> Self {
        let mut loader = Self {
            opts,
            root: root.to_path_buf(),
            entry: source.to_string(),
            merged: Merged::new(),
            deps: Vec::new(),
            modules: HashMap::new(),
            scope_of: HashMap::new(),
            next_scope: 1,
            stack: Vec::new(),
        };
        let id = loader.intern(root, source.to_string());
        loader.scope_of.insert(id, 0);
        loader
    }

    pub(crate) fn run(mut self) -> Result<Loaded, Box<CompileError>> {
        let root = self.root.clone();
        let (_, pieces) = self.pieces_for(&root, 0)?;
        for piece in pieces {
            self.merged.append_part(piece);
        }
        self.merged.normalize();
        Ok(Loaded {
            merged: self.merged,
            deps: self.deps,
            modules: self.modules,
            scope_of: self.scope_of,
            scopes: self.next_scope,
        })
    }

    fn intern(&mut self, path: &Path, text: String) -> u32 {
        self.merged.add_file(path, text.clone(), text)
    }

    fn read(&self, path: &Path) -> Result<String, Box<CompileError>> {
        std::fs::read_to_string(path)
            .map_err(|e| CompileError::new(format!("读取 {} 失败: {e}", path.display())).into())
    }

    /// 一个文件的完整内容（`@use` 依赖在前，随后是自身语句）；返回文件 id 与片段。
    fn pieces_for(&mut self, path: &Path, scope: usize) -> Result<(u32, Vec<Part>), Box<CompileError>> {
        let key = canonical(path);
        if self.stack.contains(&key) {
            return Err(CompileError::new(format!(
                "This file is already being loaded: {}",
                path.display()
            ))
            .into());
        }
        self.stack.push(key);
        let text = if path == self.root {
            self.entry.clone()
        } else {
            self.read(path)?
        };
        let id = self.intern(path, text.clone());
        self.scope_of.entry(id).or_insert(scope);
        let dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();

        // `.sass` 缩进语法在装载期折成等义 SCSS：整条流水线只面对 SCSS 文本。
        // 改写不增删行，所以下面的偏移仍可按**原文**换算行列。
        let indented = is_indented(path);
        let converted;
        let scss = if indented {
            converted = indented::to_scss(&text);
            converted.as_str()
        } else {
            text.as_str()
        };

        let alloc = Allocator::default();
        let mut parser = ParserBuilder::new(&alloc, scss)
            .syntax(Syntax::Scss)
            .comments()
            .build();
        let ast = parser.parse::<Stylesheet>().map_err(|e| {
            let (at, message) = if indented {
                indented::parse_error(&text, scss, e.span.start, e.kind)
            } else {
                parse_error(&text, e.span.start, e.kind)
            };
            self.error_at_text(path, &text, at, &message)
        })?;

        let uses = top_level_uses(&ast.statements, scss).map_err(|at| {
            self.error_at_text(
                path,
                &text,
                at,
                "@use rules must be written before any other rules.",
            )
        })?;

        // 先解析全部 prelude：只要文件里出现 `with` 配置或 `@forward`，依赖就要装到
        // **语句位置**（而不是整个文件之前），这样配置表达式先于模块体求值、
        // `@forward` 也能在目标模块求值前登记转发边。
        let mut sites: Vec<(Span, ModuleSpec<'_>, bool)> = Vec::new();
        // 命名空间重名是**每个文件一次**的静态检查（dart 的 `_environment.namespaces`）：
        // 同一个文件里两个 `@use` 声明同一命名空间才报错；`@import` 内联的多个文件
        // 各自 `@use` 同一个名字是合法的。
        let mut declared: Vec<String> = Vec::new();
        for span in uses {
            let raw = &scss[span.start..span.end];
            let forward = at_name(raw).eq_ignore_ascii_case("forward");
            let spec = parse_module_spec(raw, span.start)
                .map_err(|(at, message)| self.error_at_text(path, &text, at, &message))?;
            // 内置模块不允许配置（dart 的 `BuiltInModule` 没有配置入口）。
            if spec.config.is_some() && spec.path.starts_with("sass:") {
                return Err(self.error_at_text(
                    path,
                    &text,
                    span.start,
                    "Built-in modules can't be configured.",
                ));
            }
            if !forward {
                let namespace = match spec.alias {
                    Some((alias, _)) => alias.to_string(),
                    None => default_namespace(&spec.path),
                };
                if namespace != "*" {
                    if declared.iter().any(|name| *name == namespace) {
                        return Err(self.error_at_text(
                            path,
                            &text,
                            span.start,
                            &format!("There's already a module with namespace \"{namespace}\"."),
                        ));
                    }
                    declared.push(namespace);
                }
            }
            sites.push((span, spec, forward));
        }
        let place_at_statement = sites
            .iter()
            .any(|(_, spec, forward)| *forward || spec.config.is_some());

        let mut prologue = Vec::new();
        let mut splices: Vec<(usize, Vec<Part>)> = Vec::new();
        for (span, spec, _) in &sites {
            // 内置模块（`sass:math`）不落盘也不解析成文件：`@use` 语句原样留给求值器，
            // 由 `Converter::use_rule` 注册静态成员表。
            if spec.path.starts_with("sass:") {
                continue;
            }
            let resolved = match self.resolve(&dir, &spec.path) {
                Ok(p) => p,
                Err(e) => {
                    return Err(self.resolve_error(path, &text, span.start, &spec.path, e));
                }
            };
            if self.modules.contains_key(&canonical(&resolved)) {
                // 已装载的模块不能再配置（dart 的 `_moduleCache` 检查）。
                if spec.config.is_some() {
                    return Err(self.error_at_text(
                        path,
                        &text,
                        span.start,
                        "This module was already loaded, so it can't be configured using \"with\".",
                    ));
                }
                continue;
            }
            let child_scope = self.next_scope;
            self.next_scope += 1;
            let (file, pieces) = self.pieces_for(&resolved, child_scope)?;
            self.modules.insert(
                canonical(&resolved),
                ModuleInfo {
                    file,
                    scope: child_scope,
                },
            );
            self.push_dep(&resolved);
            if place_at_statement {
                splices.push((after_semicolon(scss, span.end), pieces));
            } else {
                prologue.extend(pieces);
            }
        }

        let mut body = Vec::new();
        self.body_pieces(scss, id, &dir, &ast.statements, &mut body)?;
        if !splices.is_empty() {
            body = splice_pieces(body, id, splices);
        }
        prologue.extend(body);
        self.stack.pop();
        Ok((id, prologue))
    }

    /// 文件正文：把 Sass `@import` 处换成被导入文件的片段，其余原文照搬。
    fn body_pieces(
        &mut self,
        text: &str,
        file: u32,
        dir: &Path,
        stmts: &[Statement<'_>],
        out: &mut Vec<Part>,
    ) -> Result<(), Box<CompileError>> {
        let mut imports = Vec::new();
        collect_imports(stmts, &mut imports);
        imports.sort_by_key(|span| span.start);
        let path = self.merged.files[file as usize].path.clone();
        let mut cursor = 0usize;
        for span in imports {
            push_text(out, file, text, cursor, span.start);
            let prelude = at_prelude_range(text, span.start, span.end, "import");
            let parts = split_top_level_comma_ranges(text, prelude.0, prelude.1);
            let kinds: Vec<ImportKind> = parts
                .iter()
                .map(|(s, e)| import_kind(&text[*s..*e]))
                .collect();
            if kinds.iter().all(|k| matches!(k, ImportKind::Css)) {
                push_text(out, file, text, span.start, span.end);
            } else {
                for ((start, end), kind) in parts.iter().zip(&kinds) {
                    let part = text[*start..*end].trim();
                    let at =
                        *start + (text[*start..*end].len() - text[*start..*end].trim_start().len());
                    match kind {
                        ImportKind::Css => {
                            if !part.is_empty() {
                                push_synth(out, file, at, &format!("@import {part};"));
                            }
                        }
                        ImportKind::Sass(url) => {
                            let resolved = match self.resolve(dir, url) {
                                Ok(p) => p,
                                Err(e) => {
                                    return Err(self.resolve_error(&path, text, at, url, e));
                                }
                            };
                            let scope = *self.scope_of.get(&file).unwrap_or(&0);
                            let (_, pieces) = self.pieces_for(&resolved, scope)?;
                            out.extend(pieces);
                            self.push_dep(&resolved);
                        }
                    }
                }
            }
            cursor = span.end;
        }
        push_text(out, file, text, cursor, text.len());
        Ok(())
    }

    fn push_dep(&mut self, path: &Path) {
        let name = path.to_string_lossy().into_owned();
        if !self.deps.contains(&name) {
            self.deps.push(name);
        }
    }

    fn error_at_text(&self, path: &Path, text: &str, at: usize, message: &str) -> Box<CompileError> {
        let index = LineIndex::new(text);
        let (line, column) = index.line_col(at.min(text.len()), text);
        Box::new(CompileError {
            message: message.to_string(),
            path: Some(path.to_string_lossy().into_owned()),
            line: Some(line + 1),
            column: Some(column),
        })
    }

    fn resolve_error(
        &self,
        path: &Path,
        text: &str,
        at: usize,
        url: &str,
        err: ResolveError,
    ) -> Box<CompileError> {
        self.error_at_text(path, text, at, &err.message(url))
    }

    /// 路径解析：直接候选（含 `_partial`）优先，全部不存在才看目录 index。
    fn resolve(&self, dir: &Path, url: &str) -> Result<PathBuf, ResolveError> {
        resolve(dir, url, &self.opts.paths)
    }
}

fn push_text(out: &mut Vec<Part>, file: u32, text: &str, start: usize, end: usize) {
    if start >= end {
        return;
    }
    out.push(Part::source(file, text[start..end].to_string(), start));
}

fn push_synth(out: &mut Vec<Part>, file: u32, orig: usize, text: &str) {
    out.push(Part::source(file, format!("{text}\n"), orig));
}

/// 公开的路径解析（`@use` 在求值阶段也要按同一规则找文件）。
pub(crate) fn resolve(dir: &Path, url: &str, paths: &[PathBuf]) -> Result<PathBuf, ResolveError> {
    let mut roots = vec![dir.to_path_buf()];
    roots.extend(paths.iter().map(PathBuf::from));
    for root in roots {
        match resolve_in(&root, url) {
            Ok(path) => return Ok(path),
            Err(ResolveError::NotFound) => {}
            Err(e) => return Err(e),
        }
    }
    Err(ResolveError::NotFound)
}

/// 是否是 `.sass`（缩进语法）。
fn is_indented(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("sass"))
}

/// `.scss` 解析错误：定位 + 文案（对照 dart-sass 1.93.2）。
fn parse_error(text: &str, at: usize, kind: oxc_css_parser::error::ErrorKind) -> (usize, String) {
    use oxc_css_parser::error::ErrorKind;
    // `.scss` 里写纯缩进：dart 把整段当成一个跨行选择器，到结尾才喊 `expected "{"`。
    if matches!(
        kind,
        ErrorKind::UnexpectedWhitespace | ErrorKind::UnexpectedWhitespaceOrComments
    ) && !text.contains('{')
    {
        return (text.trim_end().len(), "expected \"{\".".to_string());
    }
    (at, format!("SCSS 解析失败: {kind}"))
}

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn resolve_in(dir: &Path, url: &str) -> Result<PathBuf, ResolveError> {
    let url_path = Path::new(url);
    let ext = url_path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());
    let has_ext = matches!(ext.as_deref(), Some("scss" | "sass" | "css"));

    let mut direct = Vec::new();
    if has_ext {
        let exact = dir.join(url);
        if exact.is_file() {
            direct.push(exact);
        }
        if let Some(partial) = partial_variant(&dir.join(url)) {
            if partial.is_file() {
                direct.push(partial);
            }
        }
    } else {
        for ext in ["sass", "scss", "css"] {
            let plain = dir.join(format!("{url}.{ext}"));
            let partial = partial_variant(&plain);
            if plain.is_file() {
                direct.push(plain);
            }
            if let Some(partial) = partial {
                if partial.is_file() {
                    direct.push(partial);
                }
            }
        }
    }
    direct.sort();
    direct.dedup();
    match direct.len() {
        0 => {}
        1 => return Ok(direct.remove(0)),
        _ => return Err(ResolveError::Ambiguous(display_paths(dir, direct))),
    }

    let mut index = Vec::new();
    for ext in ["sass", "scss", "css"] {
        for name in ["_index", "index"] {
            let base = dir.join(format!("{url}/{name}.{ext}"));
            if base.is_file() {
                index.push(base);
            }
        }
    }
    index.sort();
    match index.len() {
        0 => Err(ResolveError::NotFound),
        1 => Ok(index.remove(0)),
        _ => Err(ResolveError::Ambiguous(display_paths(dir, index))),
    }
}

fn display_paths(dir: &Path, paths: Vec<PathBuf>) -> Vec<PathBuf> {
    paths
        .into_iter()
        .map(|p| p.strip_prefix(dir).map(Path::to_path_buf).unwrap_or(p))
        .collect()
}

/// `foo/bar.scss` → `foo/_bar.scss`（已经带 `_` 则不给候选）。
fn partial_variant(path: &Path) -> Option<PathBuf> {
    let name = path.file_name()?.to_str()?;
    if name.starts_with('_') {
        return None;
    }
    Some(path.parent()?.join(format!("_{name}")))
}

/// 语句里 `@name` 之后到语句尾的 prelude 区间（去掉首尾空白与结尾 `;`）。
fn at_prelude_range(text: &str, start: usize, end: usize, name: &str) -> (usize, usize) {
    let raw = text.get(start..end).unwrap_or("");
    let after = raw
        .strip_prefix(&format!("@{name}"))
        .map(|rest| start + raw.len() - rest.len())
        .unwrap_or(start);
    let trimmed = text.get(after..end).unwrap_or("").trim_end();
    let trimmed = trimmed.strip_suffix(';').unwrap_or(trimmed).trim_end();
    (after, after + trimmed.len())
}

/// 顶层逗号切分并返回各段的绝对区间。
fn split_top_level_comma_ranges(text: &str, start: usize, end: usize) -> Vec<(usize, usize)> {
    crate::text::split_top_level(text.get(start..end).unwrap_or(""), b',')
        .into_iter()
        .map(|(s, e)| (start + s, start + e))
        .collect()
}

/// `@use` 的默认命名空间（文件名去前导 `_` 与扩展名；内置模块用规范名）。
fn default_namespace(path: &str) -> String {
    if let Some(module) = crate::scss::modules::Builtin::from_url(path) {
        return module.default_namespace().to_string();
    }
    let name = path.rsplit('/').next().unwrap_or(path);
    let name = name.strip_prefix('_').unwrap_or(name);
    name.split('.').next().unwrap_or(name).to_string()
}

/// 语句文本里的 at-rule 名（`@use ...` → `use`）。
fn at_name(raw: &str) -> &str {
    let rest = raw.trim_start().trim_start_matches('@');
    let end = rest
        .find(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '-' || ch == '_'))
        .unwrap_or(rest.len());
    &rest[..end]
}

enum ImportKind {
    Css,
    Sass(String),
}

fn import_kind(part: &str) -> ImportKind {
    let part = part.trim();
    let quote = match part.chars().next() {
        Some(c @ ('"' | '\'')) => c,
        _ => return ImportKind::Css,
    };
    let body = &part[1..];
    let Some(end) = find_quote_end(body, quote) else {
        return ImportKind::Css;
    };
    let path = value_unescape(&body[..end]);
    if !body[end + 1..].trim().is_empty() {
        return ImportKind::Css;
    }
    let lower = path.to_ascii_lowercase();
    if lower.ends_with(".css")
        || lower.ends_with(".wxss")
        || lower.ends_with(".less")
        || lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("//")
    {
        return ImportKind::Css;
    }
    ImportKind::Sass(path)
}

fn find_quote_end(text: &str, quote: char) -> Option<usize> {
    let bytes = text.as_bytes();
    let q = quote as u8;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b if b == q => return Some(i),
            _ => i += 1,
        }
    }
    None
}

fn value_unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some(next) => out.push(next),
            None => break,
        }
    }
    out
}

/// `@use` / `@forward` 的 prelude 解析结果。偏移都是合并文本里的绝对偏移。
pub(crate) struct ModuleSpec<'a> {
    pub(crate) path: String,
    /// `as` 后面的原文（`*` / 命名空间 / 前缀 `p-*`）与起点。
    pub(crate) alias: Option<(&'a str, usize)>,
    pub(crate) filter: Option<Filter<'a>>,
    pub(crate) config: Option<Config<'a>>,
}

/// `@forward ... show / hide` 的成员列表。变量以 `$` 开头，其余名字同时指函数与 mixin。
#[derive(Default)]
pub(crate) struct Filter<'a> {
    pub(crate) show: bool,
    pub(crate) vars: Vec<&'a str>,
    pub(crate) members: Vec<&'a str>,
}

pub(crate) struct Config<'a> {
    pub(crate) inner: &'a str,
    pub(crate) at: usize,
}

/// 一条 `$name: value [!default]` 配置。
pub(crate) struct ConfigEntry<'a> {
    pub(crate) name: &'a str,
    pub(crate) value: &'a str,
    /// `$name` 的起点（报错定位）。
    pub(crate) at: usize,
    /// 值文本的起点（表达式求值时的合并偏移）。
    pub(crate) value_at: usize,
    /// `@forward ... with` 里的 `!default`：调用方可以覆盖。
    pub(crate) default: bool,
    /// `!default` 里 `!` 的位置（`@use` 不接受它）。
    pub(crate) default_at: usize,
}

/// `@use` / `@forward` 的 prelude（`text` 从 `@` 开始，`base` 是它的绝对偏移）。
pub(crate) fn parse_module_spec<'a>(
    text: &'a str,
    base: usize,
) -> Result<ModuleSpec<'a>, (usize, String)> {
    let err = |at: usize, message: &str| (base + at, message.to_string());
    let mut i = 0usize;
    let bytes = text.as_bytes();
    if bytes.first() != Some(&b'@') {
        return Err(err(0, "Expected at-rule."));
    }
    i += 1;
    let name_start = i;
    while i < bytes.len() && is_ident_char(bytes[i]) {
        i += 1;
    }
    let forward = text[name_start..i].eq_ignore_ascii_case("forward");
    skip_ws(bytes, &mut i);
    let Some(&quote) = bytes.get(i) else {
        return Err(err(i, "Expected string."));
    };
    if quote != b'"' && quote != b'\'' {
        return Err(err(i, "Expected string."));
    }
    let Some(end) = find_quote_end(&text[i + 1..], quote as char) else {
        return Err(err(i, "Expected string."));
    };
    let path = value_unescape(&text[i + 1..i + 1 + end]);
    i += end + 2;

    let mut alias = None;
    let mut filter = None;
    let mut config = None;
    loop {
        skip_ws(bytes, &mut i);
        match bytes.get(i) {
            None | Some(b';') => break,
            _ => {}
        }
        if word_at(text, i, "as") {
            i += 2;
            skip_ws(bytes, &mut i);
            let start = i;
            while i < bytes.len() && text.as_bytes()[i] != b';' && !is_space(bytes[i]) {
                i += 1;
            }
            let spec = &text[start..i];
            if forward && !spec.ends_with('*') {
                return Err(err(i, "expected \"*\"."));
            }
            alias = Some((spec, base + start));
            continue;
        }
        if word_at(text, i, "show") || word_at(text, i, "hide") {
            let show = word_at(text, i, "show");
            i += 4;
            let mut list = Filter {
                show,
                ..Filter::default()
            };
            loop {
                skip_ws(bytes, &mut i);
                match bytes.get(i) {
                    None | Some(b';') => break,
                    Some(b',') => {
                        i += 1;
                        continue;
                    }
                    _ => {}
                }
                if bytes[i] == b'$' {
                    i += 1;
                    let name_start = i;
                    while i < bytes.len() && is_ident_char(bytes[i]) {
                        i += 1;
                    }
                    list.vars.push(&text[name_start..i]);
                } else if is_ident_start(bytes[i]) {
                    let name_start = i;
                    while i < bytes.len() && is_ident_char(bytes[i]) {
                        i += 1;
                    }
                    list.members.push(&text[name_start..i]);
                } else {
                    return Err(err(i, "expected \";\"."));
                }
            }
            filter = Some(list);
            continue;
        }
        if word_at(text, i, "with") {
            i += 4;
            skip_ws(bytes, &mut i);
            if bytes.get(i) != Some(&b'(') {
                return Err(err(i, "expected \"(\"."));
            }
            let Some(close) = matching_paren(text, i) else {
                return Err(err(i, "expected \")\"."));
            };
            config = Some(Config {
                inner: &text[i + 1..close],
                at: base + i + 1,
            });
            i = close + 1;
            continue;
        }
        return Err(err(i, "expected \";\"."));
    }
    skip_ws(bytes, &mut i);
    if i < bytes.len() && bytes[i] != b';' {
        return Err(err(i, "expected \";\"."));
    }
    Ok(ModuleSpec {
        path,
        alias,
        filter,
        config,
    })
}

/// `with (...)` 里的配置项（按顶层逗号切分）。
pub(crate) fn config_entries<'a>(
    config: &Config<'a>,
) -> Result<Vec<ConfigEntry<'a>>, (usize, String)> {
    let inner = config.inner;
    let mut out = Vec::new();
    for (start, end) in split_top_level_comma_ranges(inner, 0, inner.len()) {
        let raw = &inner[start..end];
        let lead = raw.len() - raw.trim_start().len();
        let entry = raw[lead..].trim_end();
        if entry.is_empty() {
            continue;
        }
        let at = config.at + start + lead;
        let Some(rest) = entry.strip_prefix('$') else {
            return Err((at, "expected \"$\".".to_string()));
        };
        let name_end = rest
            .find(|ch: char| !is_ident_char_utf8(ch))
            .unwrap_or(rest.len());
        let name = &rest[..name_end];
        if name.is_empty() {
            return Err((at, "expected \"$\".".to_string()));
        }
        let tail = rest[name_end..].trim_start();
        let tail_at = at + 1 + name_end + (rest[name_end..].len() - tail.len());
        let Some(after_colon) = tail.strip_prefix(':') else {
            return Err((tail_at, "expected \":\".".to_string()));
        };
        let after_at = tail_at + 1;
        let value_lead = after_colon.len() - after_colon.trim_start().len();
        let value_at = after_at + value_lead;
        let value = after_colon.trim();
        let (value, default, default_at) = match value.rfind("!default") {
            Some(pos) if value[pos..].trim_end() == "!default" => {
                (value[..pos].trim_end(), true, value_at + pos)
            }
            _ => (value, false, 0),
        };
        if value.is_empty() {
            return Err((value_at, "Expected expression.".to_string()));
        }
        out.push(ConfigEntry {
            name,
            value,
            at,
            value_at,
            default,
            default_at,
        });
    }
    Ok(out)
}

/// `@use` / `@forward` 语句的结束位置（oxc 的 at-rule span 会截断 prelude，
/// 这里从 span.end 往后扫到顶层 `;`）。
pub(crate) fn statement_end(text: &str, from: usize) -> usize {
    let bytes = text.as_bytes();
    let mut depth = 0usize;
    let mut quote: Option<u8> = None;
    let mut i = from.min(bytes.len());
    while i < bytes.len() {
        let b = bytes[i];
        if let Some(q) = quote {
            if b == b'\\' {
                i += 2;
                continue;
            }
            if b == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        match b {
            b'\'' | b'"' => quote = Some(b),
            b'(' | b'[' => depth += 1,
            b')' | b']' => depth = depth.saturating_sub(1),
            b';' if depth == 0 => return i + 1,
            b'{' | b'}' if depth == 0 => return i,
            _ => {}
        }
        i += 1;
    }
    text.len()
}

/// 语句尾的下一个插入点（`@use` 的 span 可能不含结尾 `;`）。
fn after_semicolon(text: &str, at: usize) -> usize {
    let bytes = text.as_bytes();
    let mut i = at;
    while i < bytes.len() && is_space(bytes[i]) {
        i += 1;
    }
    if i < bytes.len() && bytes[i] == b';' {
        i + 1
    } else {
        at
    }
}

fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | b'\x0c')
}

fn skip_ws(bytes: &[u8], i: &mut usize) {
    while *i < bytes.len() && is_space(bytes[*i]) {
        *i += 1;
    }
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_' || b == b'-' || b >= 0x80
}

fn is_ident_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b >= 0x80
}

fn is_ident_char_utf8(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' || !ch.is_ascii()
}

/// 关键字（后面跟空白 / `(` / 结尾才算）。
fn word_at(text: &str, i: usize, word: &str) -> bool {
    let Some(rest) = text.get(i..) else {
        return false;
    };
    let Some(after) = rest.strip_prefix(word) else {
        return false;
    };
    after
        .chars()
        .next()
        .map_or(true, |c| c.is_whitespace() || c == '(')
}

/// 从 `open`（必须是 `(`）找到配对的 `)`。
fn matching_paren(text: &str, open: usize) -> Option<usize> {
    crate::text::matching(text, open).map(|end| end - 1)
}

/// 把每个依赖片段插到对应 `@use` / `@forward` 语句之后（按文件内偏移拆段）。
fn splice_pieces(pieces: Vec<Part>, file: u32, mut splices: Vec<(usize, Vec<Part>)>) -> Vec<Part> {
    splices.sort_by_key(|(at, _)| *at);
    let mut out = Vec::with_capacity(pieces.len() + splices.len() * 2);
    let mut next = 0usize;
    for piece in pieces {
        // 装载期每个片段都来自一个文件的一段连续原文。
        let Some(first) = piece.segs.first().copied() else {
            continue;
        };
        if first.file != file {
            out.push(piece);
            continue;
        }
        let start = first.orig;
        let end = start + piece.text.len();
        let mut cursor = start;
        while next < splices.len() && splices[next].0 <= end {
            let at = splices[next].0;
            if at > cursor {
                out.push(Part::source(
                    file,
                    piece.text[cursor - start..at - start].to_string(),
                    cursor,
                ));
                cursor = at;
            }
            out.append(&mut splices[next].1);
            next += 1;
        }
        if cursor < end {
            out.push(Part::source(
                file,
                piece.text[cursor - start..].to_string(),
                cursor,
            ));
        }
    }
    out
}

/// 收集所有层级的 `@import` 语句区间（含末尾 `;`）。
fn collect_imports<'a>(stmts: &'a [Statement<'a>], out: &mut Vec<Span>) {
    for st in stmts {
        match st {
            Statement::AtRule(at) => {
                if at.name.name.eq_ignore_ascii_case("import") {
                    out.push(at.span);
                } else if let Some(block) = &at.block {
                    collect_imports(&block.statements, out);
                }
            }
            Statement::QualifiedRule(rule) => collect_imports(&rule.block.statements, out),
            Statement::KeyframeBlock(block) => collect_imports(&block.block.statements, out),
            _ => {}
        }
    }
}

/// 顶层 `@use` / `@forward` 的区间；出现在其它语句之后即报错（返回该语句起点）。
fn top_level_uses(stmts: &[Statement<'_>], text: &str) -> Result<Vec<Span>, usize> {
    let mut uses = Vec::new();
    let mut seen_other = false;
    for st in stmts {
        match st {
            Statement::AtRule(at)
                if at.name.name.eq_ignore_ascii_case("use")
                    || at.name.name.eq_ignore_ascii_case("forward") =>
            {
                if seen_other {
                    return Err(at.span.start);
                }
                uses.push(Span {
                    start: at.span.start,
                    end: statement_end(text, at.span.end),
                });
            }
            // `@charset` 不占位（前面的 `@use` 仍然合法）。
            Statement::AtRule(at) if at.name.name.eq_ignore_ascii_case("charset") => {}
            // 变量声明与注释不占位（dart-sass 实测：`$x: 1; @use "m";` 合法）。
            Statement::SassVariableDeclaration(_) => {}
            _ => seen_other = true,
        }
    }
    Ok(uses)
}
