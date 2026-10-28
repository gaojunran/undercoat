use std::collections::HashMap;
use std::path::{Path, PathBuf};

use oxc_css_parser::ast::*;
use oxc_css_parser::{Allocator, ParserBuilder, Syntax};

use crate::Options;
use crate::merge::{Merged, Part};

use super::Compiler;
use super::prelude::{REFERENCE_AT, is_remote, split_signed_dimensions};

/// Expands `@import` into text inlining (a less import is a merge into a shared scope).
///
/// Only static paths are handled: imports with interpolation stay in the AST and are
/// processed by `emit_import` after evaluation; `.css` and `(css)` imports are kept as-is (emitted as a CSS `@import`).
///
/// Each file runs through `split_signed_dimensions` separately before expansion, so segment-table offsets stay
/// consistent with each file's line table (padding after merging would shift the columns of later tokens).
#[allow(clippy::too_many_arguments)]
pub(super) fn expand_imports(
    path: &Path,
    source: &str,
    opts: &Options,
    seen: &mut Vec<PathBuf>,
    chain: &mut Vec<PathBuf>,
    deps: &mut Vec<String>,
    multiple: bool,
    merged: &mut Merged,
) -> Part {
    let text = split_signed_dimensions(source);
    let file = merged.add_file(path, text.clone(), source.to_string());
    let first = expand_import_pass(
        path,
        Part::whole(file, text),
        opts,
        seen,
        chain,
        deps,
        &HashMap::new(),
        multiple,
        merged,
    );
    // Second pass: path interpolation. less variables are lazily evaluated, so a variable in a path may come from
    // a later import and can only be evaluated in the root scope after the first static inlining pass.
    let vars = root_vars(&first.text);
    if vars.is_empty() || !first.text.contains("@{") {
        return first;
    }
    expand_import_pass(
        path, first, opts, seen, chain, deps, &vars, multiple, merged,
    )
}

/// One inlining pass: also handles interpolated paths when `vars` is non-empty.
#[allow(clippy::too_many_arguments)]
pub(super) fn expand_import_pass(
    path: &Path,
    input: Part,
    opts: &Options,
    seen: &mut Vec<PathBuf>,
    chain: &mut Vec<PathBuf>,
    deps: &mut Vec<String>,
    vars: &HashMap<String, String>,
    multiple: bool,
    merged: &mut Merged,
) -> Part {
    let alloc = Allocator::default();
    let mut parser = ParserBuilder::new(&alloc, &input.text)
        .syntax(Syntax::Less)
        .build();
    let Ok(ast) = parser.parse::<Stylesheet>() else {
        // Leave parse failures to the main flow so the original error message is kept
        return input;
    };
    let mut edits: Vec<(usize, usize, &AtRule)> = Vec::new();
    collect_imports(&ast.statements, &mut edits);
    if edits.is_empty() {
        return input;
    }
    let base = path.parent().unwrap_or(Path::new("."));
    let host = merged.file_id(path).unwrap_or(0);
    let mut out = Part::empty();
    let mut cursor = 0;
    for (start, end, at) in edits {
        let Some(prelude) = &at.prelude else {
            continue;
        };
        let raw = input
            .text
            .get(prelude.span().start..prelude.span().end)
            .unwrap_or("");
        // The statement span does not include the trailing semicolon
        let mut stop = end;
        let rest = &input.text[end..];
        if let Some(pos) = rest.find(|c: char| !c.is_whitespace()) {
            if rest.as_bytes().get(pos) == Some(&b';') {
                stop = end + pos + 1;
            }
        }
        let Some(replacement) = import_replacement(
            raw, base, opts, seen, chain, deps, vars, multiple, merged, host, start,
        ) else {
            continue;
        };
        out.append_range(&input, cursor, start);
        out.append(&replacement);
        cursor = stop;
    }
    out.append_range(&input, cursor, input.text.len());
    out
}

pub(super) fn collect_imports<'a, 'b>(
    stmts: &'b [Statement<'a>],
    out: &mut Vec<(usize, usize, &'b AtRule<'a>)>,
) {
    for st in stmts {
        match st {
            Statement::AtRule(at) if at.name.name.eq_ignore_ascii_case("import") => {
                out.push((at.span.start, at.span.end, at));
            }
            Statement::QualifiedRule(r) => collect_imports(&r.block.statements, out),
            Statement::AtRule(at) => {
                if let Some(block) = &at.block {
                    collect_imports(&block.statements, out);
                }
            }
            _ => {}
        }
    }
}

/// Replacement text for a single import; `None` keeps the statement unchanged.
#[allow(clippy::too_many_arguments)]
pub(super) fn import_replacement(
    raw: &str,
    base: &Path,
    opts: &Options,
    seen: &mut Vec<PathBuf>,
    chain: &mut Vec<PathBuf>,
    deps: &mut Vec<String>,
    vars: &HashMap<String, String>,
    multiple: bool,
    merged: &mut Merged,
    host: u32,
    at: usize,
) -> Option<Part> {
    let (options, rest) = split_import_options(raw);
    let (href, media) = split_quoted_path(&rest)?;
    // Path interpolation is evaluated in the scope (less lazy evaluation: the variable may come from another import)
    let href = if href.contains("@{") {
        let resolved = substitute_vars(&href, vars);
        if resolved.contains("@{") {
            return None;
        }
        resolved
    } else {
        href
    };
    if is_remote(&href) {
        return None;
    }
    let css_only = options.iter().any(|o| o == "css");
    let optional = options.iter().any(|o| o == "optional");
    // (inline) is passed through verbatim and written out by emit_import after reading the file (less does not parse it)
    if css_only || options.iter().any(|o| o == "inline") {
        return None;
    }
    let force_less = options.iter().any(|o| o == "less");
    // CSS imports (`.css` / `?css`, etc.) are not inlined and are kept as CSS @import (same rule as less)
    if !force_less && is_css_path(&href) {
        return None;
    }
    let Some(resolved) = resolve_import(base, &href, opts) else {
        return optional.then(Part::empty);
    };
    let ext = resolved.extension().and_then(|e| e.to_str()).unwrap_or("");
    if matches!(ext, "css" | "wxss") && !force_less {
        return None;
    }
    let key = resolved.canonicalize().unwrap_or_else(|_| resolved.clone());
    // Circular import: already present on the ancestor chain, no further inlining
    if chain.contains(&key) {
        return Some(Part::empty());
    }
    // The whole subtree of `(multiple)` skips import-once deduplication (less's context.importMultiple propagates down)
    let multiple = multiple || options.iter().any(|o| o == "multiple");
    if !multiple && seen.contains(&key) {
        return Some(Part::empty());
    }
    let Ok(text) = std::fs::read_to_string(&resolved) else {
        return optional.then(Part::empty);
    };
    seen.push(key.clone());
    deps.push(resolved.to_string_lossy().to_string());
    let trimmed = media.trim();
    chain.push(key);
    let body = expand_imports(&resolved, &text, opts, seen, chain, deps, multiple, merged);
    chain.pop();
    let body = if options.iter().any(|o| o == "reference") {
        // (reference): definitions remain visible as usual but produce no output
        Part::wrap(&format!("\n@{REFERENCE_AT} {{\n"), &body, "\n}\n", host, at)
    } else {
        body
    };
    Some(if trimmed.is_empty() {
        // Add a newline to avoid sticking to adjacent tokens
        Part::wrap("\n", &body, "\n", host, at)
    } else {
        Part::wrap(
            &format!("\n@media {trimmed} {{\n"),
            &body,
            "\n}\n",
            host,
            at,
        )
    })
}

/// `@{name}` text substitution (for import paths; undefined names are kept verbatim).
pub(super) fn substitute_vars(text: &str, vars: &HashMap<String, String>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(pos) = rest.find("@{") {
        let Some(end) = rest[pos + 2..].find('}') else {
            break;
        };
        out.push_str(&rest[..pos]);
        match vars.get(&rest[pos + 2..pos + 2 + end]) {
            Some(value) => out.push_str(value),
            None => out.push_str(&rest[pos..pos + 3 + end]),
        }
        rest = &rest[pos + 3 + end..];
    }
    out.push_str(rest);
    out
}

/// Variable text in the root scope (for import path interpolation): only literals are handled, and references are expanded iteratively.
pub(super) fn root_vars(source: &str) -> HashMap<String, String> {
    let alloc = Allocator::default();
    let mut parser = ParserBuilder::new(&alloc, source)
        .syntax(Syntax::Less)
        .build();
    let Ok(ast) = parser.parse::<Stylesheet>() else {
        return HashMap::new();
    };
    let mut vars: HashMap<String, String> = ast
        .statements
        .iter()
        .filter_map(|st| match st {
            Statement::LessVariableDeclaration(d) => {
                let raw = source.get(d.value.span().start..d.value.span().end)?;
                Some((d.name.name.name.to_string(), raw.trim().to_string()))
            }
            _ => None,
        })
        .map(|(name, raw)| (name, unquote_text(&raw)))
        .collect();
    for _ in 0..8 {
        let snapshot = vars.clone();
        let mut changed = false;
        for value in vars.values_mut() {
            if value.contains("@{") {
                let next = substitute_vars(value, &snapshot);
                changed |= next != *value;
                *value = next;
            }
        }
        if !changed {
            break;
        }
    }
    vars
}

/// Removes a matching pair of quotes (`"in"` → `in`).
pub(super) fn unquote_text(text: &str) -> String {
    let bytes = text.as_bytes();
    match (bytes.first(), bytes.last()) {
        (Some(a), Some(b)) if a == b && matches!(a, b'"' | b'\'') && bytes.len() >= 2 => {
            text[1..text.len() - 1].to_string()
        }
        _ => text.to_string(),
    }
}

/// Splits out import options: `(reference) "x.less" screen` → (`["reference"]`, `"x.less" screen`).
///
/// Only known option names count as options: a `(min-width: 600px)` after the path is a media query feature
/// and `url(…)` is the path itself; neither may be consumed as options.
pub(super) fn split_import_options(raw: &str) -> (Vec<String>, String) {
    let mut options = Vec::new();
    let head_end = raw.find(['"', '\'']).unwrap_or(raw.len());
    let mut rest = String::new();
    let mut chars = raw.char_indices();
    while let Some((i, c)) = chars.next() {
        if c == '('
            && i < head_end
            && let Some(end) = raw[i..].find(')')
            && is_import_options(&raw[i + 1..i + end])
        {
            options.extend(
                raw[i + 1..i + end]
                    .split(',')
                    .map(|o| o.trim().to_ascii_lowercase()),
            );
            for _ in 0..raw[i + 1..i + end].chars().count() + 1 {
                chars.next();
            }
            continue;
        }
        rest.push(c);
    }
    (options, rest)
}

pub(super) fn is_import_options(inner: &str) -> bool {
    inner.split(',').all(|o| {
        matches!(
            o.trim().to_ascii_lowercase().as_str(),
            "reference" | "inline" | "less" | "css" | "once" | "multiple" | "optional"
        )
    })
}

/// Extracts the quoted path and the remaining media query from `"x.less" screen`.
pub(super) fn split_quoted_path(rest: &str) -> Option<(String, String)> {
    // `url("x.less")`: the parentheses are part of the path and the media query follows
    if let Some(at) = rest.find("url(") {
        let inner = &rest[at + 4..];
        let close = inner.find(')')?;
        let href = inner[..close].trim().trim_matches(['"', '\'']).to_string();
        return Some((href, rest[at + 5 + close..].to_string()));
    }
    let start = rest.find(['"', '\''])?;
    let quote = rest.as_bytes()[start] as char;
    let tail = &rest[start + 1..];
    let end = tail.find(quote)?;
    let href = tail[..end].to_string();
    Some((href, tail[end + 1..].to_string()))
}

/// less's CSS import test (tree/import.js:36): `/[#.&?]css([?;].*)?$/`.
/// WDT's wxss escape works precisely through `?css` matching this rule, so the import is emitted verbatim as `@import`.
pub(super) fn is_css_path(path: &str) -> bool {
    let b = path.as_bytes();
    (1..b.len().saturating_sub(2)).any(|i| {
        matches!(b[i - 1], b'#' | b'.' | b'&' | b'?')
            && &b[i..i + 3] == b"css"
            && (i + 3 == b.len() || matches!(b[i + 3], b'?' | b';'))
    })
}

/// Node's `path.join(base, href)` semantics: a leading `/` in `href` does not discard `base`.
///
/// less resolves through `[""] + paths`, joining every entry as `path.join(root, "/x")`,
/// so `/x` means "x under that root". The std `Path::join` replaces `base` wholesale when
/// `href` is absolute, which is not the same thing.
fn join_under(base: &Path, href: &str) -> PathBuf {
    if base.as_os_str().is_empty() {
        return PathBuf::from(href);
    }
    base.join(href.trim_start_matches('/'))
}

pub(super) fn resolve_import(base: &Path, href: &str, opts: &Options) -> Option<PathBuf> {
    let with_ext = Path::new(href).extension().is_none();
    let mut candidates = vec![base.join(href)];
    if with_ext {
        candidates.push(base.join(format!("{href}.less")));
    }
    for root in &opts.paths {
        candidates.push(join_under(root, href));
        if with_ext {
            candidates.push(join_under(root, &format!("{href}.less")));
        }
    }
    candidates.into_iter().find(|p| p.is_file())
}

impl<'a> Compiler<'a> {
    pub(super) fn resolve_import(&self, href: &str) -> Option<PathBuf> {
        let base = self.file_path.parent()?;
        let candidate = base.join(href);
        if candidate.is_file() {
            return Some(candidate);
        }
        // less's default completion: try .less when there is no extension
        if Path::new(href).extension().is_none() {
            let with_ext = base.join(format!("{href}.less"));
            if with_ext.is_file() {
                return Some(with_ext);
            }
        }
        // Imports with interpolated paths are evaluated at the AST layer and no longer know the original file directory:
        // try the directory of every inlined file in turn (a less import is a merge into a shared scope).
        for dep in &self.deps {
            let dir = Path::new(dep).parent()?;
            for name in [href.to_string(), format!("{href}.less")] {
                let c = dir.join(&name);
                if c.is_file() {
                    return Some(c);
                }
            }
        }
        let with_ext = Path::new(href).extension().is_none();
        for root in &self.options.paths {
            let c = join_under(root, href);
            if c.is_file() {
                return Some(c);
            }
            if with_ext {
                let c = join_under(root, &format!("{href}.less"));
                if c.is_file() {
                    return Some(c);
                }
            }
        }
        None
    }
}
