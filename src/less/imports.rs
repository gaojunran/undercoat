use std::collections::HashMap;
use std::path::{Path, PathBuf};

use oxc_css_parser::ast::*;
use oxc_css_parser::{Allocator, ParserBuilder, Syntax};

use crate::Options;
use crate::merge::{Merged, Part};

use super::Compiler;
use super::prelude::{REFERENCE_AT, is_remote, split_signed_dimensions};

/// 把 `@import` 展开成文本内联（less 的导入就是共享作用域的合并）。
///
/// 只处理静态路径：带插值的导入留在 AST 里，由 `emit_import` 在求值后处理；
/// `.css` 与 `(css)` 导入保持原样（作为 CSS `@import` 输出）。
///
/// 展开前先对每个文件单独做 `split_signed_dimensions`，因此段表的偏移与各文件
/// 的行表自洽（合并后再补空格会让后续 token 的列漂移）。
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
    // 第二轮：路径插值。less 的变量是延迟求值的，所以路径里的变量可能来自
    // 后面的导入，只能在第一轮静态内联完成后再用根作用域求值。
    let vars = root_vars(&first.text);
    if vars.is_empty() || !first.text.contains("@{") {
        return first;
    }
    expand_import_pass(
        path, first, opts, seen, chain, deps, &vars, multiple, merged,
    )
}

/// 单轮导入内联：`vars` 非空时也处理带插值的路径。
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
        // 解析失败留给主流程报错，保持原有错误信息
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
        // 语句区间不含末尾分号
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

/// 单个导入的替换文本；`None` 表示保持原语句不动。
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
    // 路径插值在作用域里求值（less 延迟求值：变量可能来自别的导入）
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
    // (inline) 原文透传，由 emit_import 在读文件后直接写出（less 不解析它）
    if css_only || options.iter().any(|o| o == "inline") {
        return None;
    }
    let force_less = options.iter().any(|o| o == "less");
    // CSS import（`.css` / `?css` 等）不内联，保留成 CSS @import（less 同规则）
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
    // 循环导入：祖先链上已出现，不再内联
    if chain.contains(&key) {
        return Some(Part::empty());
    }
    // `(multiple)` 的整棵子树跳过 import-once 去重（less 的 context.importMultiple 向下传递）
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
        // (reference)：定义照常可见，但不产生输出
        Part::wrap(&format!("\n@{REFERENCE_AT} {{\n"), &body, "\n}\n", host, at)
    } else {
        body
    };
    Some(if trimmed.is_empty() {
        // 补一个换行，避免与相邻 token 粘连
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

/// `@{name}` 文本替换（导入路径用，未定义的原样保留）。
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

/// 根作用域的变量文本（导入路径插值用）：只处理字面量，迭代展开引用。
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

/// 去掉成对的引号（`"in"` → `in`）。
pub(super) fn unquote_text(text: &str) -> String {
    let bytes = text.as_bytes();
    match (bytes.first(), bytes.last()) {
        (Some(a), Some(b)) if a == b && matches!(a, b'"' | b'\'') && bytes.len() >= 2 => {
            text[1..text.len() - 1].to_string()
        }
        _ => text.to_string(),
    }
}

/// 拆出导入选项：`(reference) "x.less" screen` → (`["reference"]`, `"x.less" screen`)。
///
/// 只有已知选项名才算选项：路径后的 `(min-width: 600px)` 是媒体查询特征，
/// `url(…)` 是路径本身，都不能当选项吃掉。
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

/// 从 `"x.less" screen` 里取出引号路径与剩余媒体查询。
pub(super) fn split_quoted_path(rest: &str) -> Option<(String, String)> {
    // `url("x.less")`：括号也是路径的一部分，媒体查询在其后
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

/// less 的 CSS import 判定（tree/import.js:36）：`/[#.&?]css([?;].*)?$/`。
/// WDT 的 wxss 转义正是靠 `?css` 命中这一条，使导入以 `@import` 原样输出。
pub(super) fn is_css_path(path: &str) -> bool {
    let b = path.as_bytes();
    (1..b.len().saturating_sub(2)).any(|i| {
        matches!(b[i - 1], b'#' | b'.' | b'&' | b'?')
            && &b[i..i + 3] == b"css"
            && (i + 3 == b.len() || matches!(b[i + 3], b'?' | b';'))
    })
}

pub(super) fn resolve_import(base: &Path, href: &str, opts: &Options) -> Option<PathBuf> {
    let mut candidates = vec![base.join(href)];
    if Path::new(href).extension().is_none() {
        candidates.push(base.join(format!("{href}.less")));
    }
    candidates.extend(opts.paths.iter().map(|p| Path::new(p).join(href)));
    candidates.into_iter().find(|p| p.is_file())
}

impl<'a> Compiler<'a> {
    pub(super) fn resolve_import(&self, href: &str) -> Option<PathBuf> {
        let base = self.file_path.parent()?;
        let candidate = base.join(href);
        if candidate.is_file() {
            return Some(candidate);
        }
        // less 的默认补全：无后缀时试 .less
        if Path::new(href).extension().is_none() {
            let with_ext = base.join(format!("{href}.less"));
            if with_ext.is_file() {
                return Some(with_ext);
            }
        }
        // 插值路径的导入在 AST 层求值，已经不知道原文件目录了：
        // 依次试所有被内联过的文件所在目录（less 的导入是共享作用域的合并）。
        for dep in &self.deps {
            let dir = Path::new(dep).parent()?;
            for name in [href.to_string(), format!("{href}.less")] {
                let c = dir.join(&name);
                if c.is_file() {
                    return Some(c);
                }
            }
        }
        for p in &self.options.paths {
            let c = Path::new(p).join(href);
            if c.is_file() {
                return Some(c);
            }
        }
        None
    }
}
