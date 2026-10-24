//! SCSS file assembly: text-level inlining of `@import` + module loading of `@use`.
//!
//! The product is a piece of **merged text** plus a segment table
//! ([`crate::merge::Seg`]) that maps every segment back to "which file + offset inside
//! the file". All later stages (evaluation, printing, sourcemap) only look at offsets
//! in the merged text, and the segment table converts them back to file/line/column on
//! error.
//!
//! Why text level: the semantics of `@import` are "move the file content verbatim to
//! this position and share the scope", so after splicing, nesting and variable sharing
//! hold naturally; `@use` is the opposite -- a module has its own scope and its CSS is
//! emitted only once, so here we only place module content before its dependents and
//! register a deduplication table.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use oxc_css_parser::ast::*;
use oxc_css_parser::pos::Span;
use oxc_css_parser::{Allocator, ParserBuilder, Syntax};

use crate::merge::{Merged, Part};
use crate::{CompileError, LineIndex, Options};

use super::indented;

/// A module loaded by `@use`: file id and the id of its own scope.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ModuleInfo {
    pub(crate) file: u32,
    pub(crate) scope: usize,
}

pub(crate) struct Loaded {
    pub(crate) merged: Merged,
    pub(crate) deps: Vec<String>,
    pub(crate) modules: HashMap<PathBuf, ModuleInfo>,
    /// File id -> scope id (files inlined by `@import` keep the importer's scope).
    pub(crate) scope_of: HashMap<u32, usize>,
    /// Number of scopes (0 = the root file).
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
                    format!("SCSS feature not implemented yet (S1): built-in module @use \"{url}\"")
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
    /// In-memory text of the entry file (`compile(.., source, ..)` does not touch disk).
    entry: String,
    /// Files registered so far and the merged text (assembled during `run`).
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
        std::fs::read_to_string(path).map_err(|e| {
            CompileError::new(format!("Failed to read {}: {e}", path.display())).into()
        })
    }

    /// Full content of one file (`@use` dependencies first, then its own statements);
    /// returns the file id and the segment.
    fn pieces_for(
        &mut self,
        path: &Path,
        scope: usize,
    ) -> Result<(u32, Vec<Part>), Box<CompileError>> {
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

        // The `.sass` indented syntax is folded into equivalent SCSS at load time: the
        // whole pipeline only ever sees SCSS text.
        // The rewrite neither adds nor removes lines, so the offsets below can still be
        // mapped to line/column through the **original** text.
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

        // Parse all preludes first: as soon as a file contains a `with` configuration or
        // a `@forward`, the dependency must be placed at the **statement position** (not
        // before the whole file), so that configuration expressions are evaluated before
        // the module body and `@forward` can register the forward edge before the target
        // module is evaluated.
        let mut sites: Vec<(Span, ModuleSpec<'_>, bool)> = Vec::new();
        // Duplicate namespaces are a static check done **once per file** (dart's
        // `_environment.namespaces`): an error only when two `@use` in the same file
        // declare the same namespace; several files inlined by `@import` each using the
        // same name is legal.
        let mut declared: Vec<String> = Vec::new();
        for span in uses {
            let raw = &scss[span.start..span.end];
            let forward = at_name(raw).eq_ignore_ascii_case("forward");
            let spec = parse_module_spec(raw, span.start)
                .map_err(|(at, message)| self.error_at_text(path, &text, at, &message))?;
            // Built-in modules cannot be configured (dart's `BuiltInModule` has no
            // configuration entry point).
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
                    if declared.contains(&namespace) {
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
            // A built-in module (`sass:math`) is neither on disk nor resolved as a file:
            // the `@use` statement is left to the evaluator, and `Converter::use_rule`
            // registers the static member table.
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
                // An already loaded module cannot be configured again (dart's
                // `_moduleCache` check).
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

    /// File body: replace each Sass `@import` with the segments of the imported files
    /// and copy the rest verbatim.
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

    fn error_at_text(
        &self,
        path: &Path,
        text: &str,
        at: usize,
        message: &str,
    ) -> Box<CompileError> {
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

    /// Path resolution: direct candidates (including `_partial`) come first; the
    /// directory index is only tried when none of them exists.
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

/// Public path resolution (`@use` must find files by the same rules during evaluation).
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

/// Whether this is `.sass` (indented syntax).
fn is_indented(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("sass"))
}

/// `.scss` parse error: location + message (checked against dart-sass 1.93.2).
fn parse_error(text: &str, at: usize, kind: oxc_css_parser::error::ErrorKind) -> (usize, String) {
    use oxc_css_parser::error::ErrorKind;
    // Pure indentation written in `.scss`: dart treats the whole thing as one multi-line
    // selector and only complains `expected "{"` at the end.
    if matches!(
        kind,
        ErrorKind::UnexpectedWhitespace | ErrorKind::UnexpectedWhitespaceOrComments
    ) && !text.contains('{')
    {
        return (text.trim_end().len(), "expected \"{\".".to_string());
    }
    (at, format!("SCSS parse failure: {kind}"))
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

/// `foo/bar.scss` -> `foo/_bar.scss` (no candidate when it already starts with `_`).
fn partial_variant(path: &Path) -> Option<PathBuf> {
    let name = path.file_name()?.to_str()?;
    if name.starts_with('_') {
        return None;
    }
    Some(path.parent()?.join(format!("_{name}")))
}

/// Prelude range of a statement from after `@name` to the end of the statement (leading
/// and trailing whitespace and the trailing `;` removed).
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

/// Split on top-level commas and return the absolute range of every segment.
fn split_top_level_comma_ranges(text: &str, start: usize, end: usize) -> Vec<(usize, usize)> {
    crate::text::split_top_level(text.get(start..end).unwrap_or(""), b',')
        .into_iter()
        .map(|(s, e)| (start + s, start + e))
        .collect()
}

/// Default namespace for `@use` (file name without the leading `_` and without the
/// extension; built-in modules use their canonical name).
fn default_namespace(path: &str) -> String {
    if let Some(module) = crate::scss::modules::Builtin::from_url(path) {
        return module.default_namespace().to_string();
    }
    let name = path.rsplit('/').next().unwrap_or(path);
    let name = name.strip_prefix('_').unwrap_or(name);
    name.split('.').next().unwrap_or(name).to_string()
}

/// at-rule name inside the statement text (`@use ...` -> `use`).
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

/// Parse result of a `@use` / `@forward` prelude. All offsets are absolute offsets in
/// the merged text.
pub(crate) struct ModuleSpec<'a> {
    pub(crate) path: String,
    /// Raw text after `as` (`*` / namespace / prefix `p-*`) and its start.
    pub(crate) alias: Option<(&'a str, usize)>,
    pub(crate) filter: Option<Filter<'a>>,
    pub(crate) config: Option<Config<'a>>,
}

/// Member list of `@forward ... show / hide`. Variables start with `$`; other names
/// refer to both functions and mixins.
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

/// One `$name: value [!default]` configuration.
pub(crate) struct ConfigEntry<'a> {
    pub(crate) name: &'a str,
    pub(crate) value: &'a str,
    /// Start of `$name` (error location).
    pub(crate) at: usize,
    /// Start of the value text (merged offset used when evaluating the expression).
    pub(crate) value_at: usize,
    /// `!default` in `@forward ... with`: the caller may override it.
    pub(crate) default: bool,
    /// Position of the `!` in `!default` (`@use` does not accept it).
    pub(crate) default_at: usize,
}

/// Prelude of `@use` / `@forward` (`text` starts at `@`; `base` is its absolute offset).
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

/// Configuration entries inside `with (...)` (split on top-level commas).
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

/// End position of a `@use` / `@forward` statement (oxc's at-rule span truncates the
/// prelude, so scan forward from span.end to the top-level `;`).
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

/// Next insertion point at the end of the statement (`@use`'s span may exclude the
/// trailing `;`).
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

/// A keyword (only counted when followed by whitespace / `(` / the end).
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
        .is_none_or(|c| c.is_whitespace() || c == '(')
}

/// Find the matching `)` starting from `open` (which must be `(`).
fn matching_paren(text: &str, open: usize) -> Option<usize> {
    crate::text::matching(text, open).map(|end| end - 1)
}

/// Insert every dependency segment after its `@use` / `@forward` statement (splitting
/// at in-file offsets).
fn splice_pieces(pieces: Vec<Part>, file: u32, mut splices: Vec<(usize, Vec<Part>)>) -> Vec<Part> {
    splices.sort_by_key(|(at, _)| *at);
    let mut out = Vec::with_capacity(pieces.len() + splices.len() * 2);
    let mut next = 0usize;
    for piece in pieces {
        // At load time every segment comes from one contiguous piece of a file's source.
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

/// Collect the ranges of all `@import` statements at every level (including the
/// trailing `;`).
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

/// Ranges of top-level `@use` / `@forward`; an error if they appear after other
/// statements (returns that statement's start).
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
            // `@charset` does not take a position (a preceding `@use` is still legal).
            Statement::AtRule(at) if at.name.name.eq_ignore_ascii_case("charset") => {}
            // Variable declarations and comments do not take a position (observed with
            // dart-sass: `$x: 1; @use "m";` is legal).
            Statement::SassVariableDeclaration(_) => {}
            _ => seen_other = true,
        }
    }
    Ok(uses)
}
