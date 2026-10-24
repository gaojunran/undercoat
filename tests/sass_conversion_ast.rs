//! `.sass` 转换器的客观门禁：**AST 等价**。
//!
//! `oxc-css-parser` 的 `Syntax::Sass` 与 `Syntax::Scss` 产出同构 AST（`=m` / `+m` 与
//! `@mixin` / `@include` 只差 `AtRule.name.raw`）。所以转换器是否正确可以这样判定：
//!
//! 1. `.sass` 原文 → `Syntax::Sass` 解析 → AST_original
//! 2. 转换器 → SCSS 文本 → `Syntax::Scss` 解析 → AST_converted
//! 3. 两棵 AST 抹掉 `span` 与所有 `raw`（源码拼写）后，`Debug` 字符串必须完全相等
//!
//! oxc 的 `Syntax::Sass` 有两处**自身**的解析差异（由下面的 `oxc_*` 探针逐条钉住，
//! oxc 修好后探针会失败，提示删除对应豁免）：
//!
//! - `oxc-1` 同缩进续行：dart 允许选择器列表的续行与首行同缩进，oxc 只认**更深**缩进，
//!   于是 `.a,` + `.b` 被拆成「空块规则 + 下一条规则」。豁免办法：把续行折成一行
//!   （`fold_selector_continuations`）再解析 oxc，等价性照旧逐棵比较。
//! - `oxc-2` 未闭合 `/*`：dart 按缩进结束注释体，oxc 直接吃到文件末尾（AST 变空），
//!   这类文件的 Sass AST 无法作为 oracle，显式跳过并计数。
//! - `oxc-3` 块内的 `//` 注释行：oxc 把它当成块的结束（后续语句跳出块）；块里只有注释时
//!   还会在 EOF 处硬报 `Unexpected("<eof>", "<dedent>")`。豁免办法：把 `//` 注释行
//!   （连同它吞掉的更深续行）删掉再解析——注释不进 AST，删掉不影响等价性比较。
//! - `oxc-4` 不带引号的 `@import ../a/b.sass`（dart 接受）：oxc 在 `.` 处报错。
//!   豁免办法：把 URL 补上引号再解析（`ImportPrelude.href` 的值不变）。
//!
//! 语料目录用 `SCSS_SASS_CORPUS` 覆盖（默认 `/tmp/opencode/scss-corpus`），
//! 目录不存在时跳过语料部分，只跑本仓库 fixtures。

use std::path::{Path, PathBuf};

use oxc_css_parser::ast::Stylesheet;
use oxc_css_parser::{Allocator, Parser, Syntax};
use undercoat::scss::indented::{fold_selector_continuations, to_scss};

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures-scss-sass")
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n == "node_modules") {
                continue;
            }
            collect(&path, out);
        } else if path.extension().is_some_and(|e| e == "sass") {
            out.push(path);
        }
    }
}

fn parse(src: &str, syntax: Syntax) -> Result<String, String> {
    let alloc = Allocator::default();
    Parser::new(&alloc, src, syntax)
        .parse::<Stylesheet>()
        .map(|ast| format!("{ast:?}"))
        .map_err(|e| format!("{:?} @ {}", e.kind, e.span.start))
}

/// 抹掉 `span` 与 `raw`：`Span { start: 1, end: 2 }` → `Span`，`raw: "x"` → 删掉。
///
/// 其余内容一字不动——包括 `comma_spans` 的**个数**（多一个逗号就说明选择器列表
/// 结构变了，必须暴露出来）。
fn normalize(debug: &str) -> String {
    let mut out = String::with_capacity(debug.len());
    let bytes = debug.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if debug[i..].starts_with("Span {") {
            let end = debug[i..].find('}').map_or(bytes.len(), |at| i + at + 1);
            out.push_str("Span");
            i = end;
            continue;
        }
        if debug[i..].starts_with("raw: \"") {
            i += "raw: \"".len();
            while i < bytes.len() {
                match bytes[i] {
                    b'\\' => i += 2,
                    b'"' => {
                        i += 1;
                        break;
                    }
                    _ => i += 1,
                }
            }
            continue;
        }
        let ch = debug[i..].chars().next().expect("char");
        out.push(ch);
        i += ch.len_utf8();
    }
    out.replace(", span", "").replace("span, ", "")
}

#[derive(Debug, PartialEq)]
enum Status {
    /// 直接等价。
    Equal,
    /// 折掉同缩进续行后等价（`oxc-1`）。
    EqualAfterFold,
    /// 再删掉 `//` 注释行后等价（`oxc-3`）。
    EqualAfterStrip,
    /// 再给 `@import` 的 URL 补引号后等价（`oxc-4`）。
    EqualAfterQuote,
    /// 未闭合 `/*`，oxc 的 Sass AST 不可用（`oxc-2`）。
    SkipUnterminatedComment,
    /// oxc 的 Sass 解析器对所有变体都不接受这份源码。
    SkipOxcUnparseable,
    Mismatch(String),
}

fn status(src: &str) -> Status {
    if has_unterminated_comment(src) {
        return Status::SkipUnterminatedComment;
    }
    let converted = to_scss(src);
    let conv_ast = match parse(&converted, Syntax::Scss) {
        Ok(ast) => normalize(&ast),
        Err(e) => return Status::Mismatch(format!("转换后 SCSS 解析失败: {e}")),
    };
    // 逐个变体试：先原样，再逐个叠加豁免（修改量单调递增）。
    let folded = fold_selector_continuations(src);
    let stripped = strip_line_comments(&folded);
    let quoted = quote_unquoted_imports(src);
    let quoted_folded = fold_selector_continuations(&quoted);
    let quoted_stripped = strip_line_comments(&quoted_folded);
    let candidates = [
        (src, Status::Equal),
        (folded.as_str(), Status::EqualAfterFold),
        (stripped.as_str(), Status::EqualAfterStrip),
        (quoted.as_str(), Status::EqualAfterQuote),
        (quoted_folded.as_str(), Status::EqualAfterQuote),
        (quoted_stripped.as_str(), Status::EqualAfterQuote),
    ];
    let mut parsed_any = false;
    let mut first_diff = None;
    for (variant, kind) in candidates {
        let Ok(ast) = parse(variant, Syntax::Sass) else {
            continue;
        };
        parsed_any = true;
        let normalized = normalize(&ast);
        if normalized == conv_ast {
            return kind;
        }
        first_diff.get_or_insert(normalized);
    }
    match first_diff {
        Some(a) => {
            let at = a
                .bytes()
                .zip(conv_ast.bytes())
                .position(|(x, y)| x != y)
                .unwrap_or_else(|| a.len().min(conv_ast.len()));
            let lo = at.saturating_sub(140);
            Status::Mismatch(format!(
                "第一处差异 @ {at}\n  sass: …{}\n  conv: …{}",
                &a[lo..(at + 140).min(a.len())],
                &conv_ast[lo..(at + 140).min(conv_ast.len())]
            ))
        }
        None if parsed_any => Status::Mismatch("变体解析成功但不可比较".to_string()),
        None => Status::SkipOxcUnparseable,
    }
}

/// 给不带引号的 `@import` URL 补引号（dart 的缩进语法允许，oxc 不认；见 `oxc-4`）。
fn quote_unquoted_imports(src: &str) -> String {
    src.split('\n')
        .map(|line| {
            let trimmed = line.trim_start();
            let Some(rest) = trimmed.strip_prefix("@import ") else {
                return line.to_string();
            };
            let url = rest.trim();
            let quoted = url.starts_with(['"', '\'']) || url.starts_with("url(");
            if quoted || url.is_empty() {
                return line.to_string();
            }
            let indent = &line[..line.len() - trimmed.len()];
            format!("{indent}@import \"{url}\"")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 删掉 `//` 注释行，以及被它们吞掉的更深续行（dart 的 comment continuation）。
fn strip_line_comments(src: &str) -> String {
    let indent = |line: &str| line.len() - line.trim_start_matches([' ', '\t']).len();
    let mut out: Vec<&str> = Vec::new();
    let mut swallowed: Option<usize> = None;
    for line in src.split('\n') {
        if line.trim().is_empty() {
            out.push(line);
            continue;
        }
        if swallowed.is_some_and(|base| indent(line) > base) {
            continue;
        }
        swallowed = None;
        if line.trim_start().starts_with("//") {
            swallowed = Some(indent(line));
            continue;
        }
        out.push(line);
    }
    out.join("\n")
}

/// `/*` 未在本行闭合（dart 按缩进收尾，oxc 吃到 EOF）。
fn has_unterminated_comment(src: &str) -> bool {
    let mut open = false;
    for line in src.split('\n') {
        let mut rest = line;
        loop {
            if open {
                match rest.find("*/") {
                    Some(at) => {
                        open = false;
                        rest = &rest[at + 2..];
                    }
                    None => break,
                }
            } else {
                match rest.find("/*") {
                    Some(at) => {
                        open = true;
                        rest = &rest[at + 2..];
                    }
                    None => break,
                }
            }
        }
    }
    open
}

struct Report {
    checked: usize,
    equal: usize,
    after_fold: Vec<PathBuf>,
    after_strip: Vec<PathBuf>,
    after_quote: Vec<PathBuf>,
    skip_comment: Vec<PathBuf>,
    skip_oxc: Vec<(PathBuf, String)>,
    mismatched: Vec<(PathBuf, String)>,
}

fn check(files: &[PathBuf]) -> Report {
    let mut report = Report {
        checked: 0,
        equal: 0,
        after_fold: Vec::new(),
        after_strip: Vec::new(),
        after_quote: Vec::new(),
        skip_comment: Vec::new(),
        skip_oxc: Vec::new(),
        mismatched: Vec::new(),
    };
    for path in files {
        let Ok(src) = std::fs::read_to_string(path) else {
            continue;
        };
        report.checked += 1;
        match status(&src) {
            Status::Equal => report.equal += 1,
            Status::EqualAfterFold => {
                report.equal += 1;
                report.after_fold.push(path.clone());
            }
            Status::EqualAfterStrip => {
                report.equal += 1;
                report.after_strip.push(path.clone());
            }
            Status::EqualAfterQuote => {
                report.equal += 1;
                report.after_quote.push(path.clone());
            }
            Status::SkipUnterminatedComment => report.skip_comment.push(path.clone()),
            Status::SkipOxcUnparseable => {
                let err = parse(&src, Syntax::Sass).err().unwrap_or_default();
                report.skip_oxc.push((path.clone(), err));
            }
            Status::Mismatch(diff) => report.mismatched.push((path.clone(), diff)),
        }
    }
    report
}

fn assert_clean(report: &Report, label: &str) {
    for (path, err) in &report.skip_oxc {
        eprintln!("[skip: oxc Syntax::Sass 不接受] {}: {err}", path.display());
    }
    for path in &report.skip_comment {
        eprintln!("[skip: 未闭合 /*] {}", path.display());
    }
    for path in &report.after_fold {
        eprintln!("[ok via oxc-1 折行] {}", path.display());
    }
    for path in &report.after_strip {
        eprintln!("[ok via oxc-3 去注释] {}", path.display());
    }
    for path in &report.after_quote {
        eprintln!("[ok via oxc-4 补引号] {}", path.display());
    }
    for (path, diff) in &report.mismatched {
        eprintln!("[FAIL: AST 不等价] {}\n{diff}", path.display());
    }
    assert!(
        report.mismatched.is_empty(),
        "{label}: {}/{} 等价（其中 {} 个折行后等价），{} 因未闭合注释跳过，{} 个 oxc 不接受，{} 不等价",
        report.equal,
        report.checked,
        report.after_fold.len(),
        report.skip_comment.len(),
        report.skip_oxc.len(),
        report.mismatched.len()
    );
}

#[test]
fn fixtures_ast_is_equivalent_after_conversion() {
    let mut files = Vec::new();
    collect(&fixture_root(), &mut files);
    files.sort();
    assert!(files.len() >= 13, "fixtures 数量异常: {}", files.len());
    let report = check(&files);
    assert_clean(&report, "fixtures");
    assert_eq!(report.equal, files.len(), "每个 fixture 都必须 AST 等价");
}

#[test]
fn corpus_ast_is_equivalent_after_conversion() {
    let dir = PathBuf::from(
        std::env::var("SCSS_SASS_CORPUS").unwrap_or_else(|_| "/tmp/opencode/scss-corpus".into()),
    );
    if !dir.is_dir() {
        eprintln!("跳过语料门禁：{} 不存在", dir.display());
        return;
    }
    let mut files = Vec::new();
    collect(&dir, &mut files);
    files.sort();
    assert!(files.len() >= 90, "语料数量异常: {}", files.len());
    let report = check(&files);
    assert_clean(&report, "corpus");
    eprintln!(
        "语料 AST 等价：{}/{}（{} 个折行后等价，{} 个去注释后等价，{} 个补引号后等价）",
        report.equal,
        report.checked,
        report.after_fold.len(),
        report.after_strip.len(),
        report.after_quote.len()
    );
}

/// `oxc-1` 探针：oxc 的 Sass 解析器不认同缩进的续行（dart 认）。
/// 折行后必须与转换结果等价；若 oxc 修好了，这里会失败 → 删除 `EqualAfterFold` 豁免。
#[test]
fn oxc_1_requires_deeper_continuation() {
    let src = ".a,\n.b\n  c: 1\n";
    let converted = to_scss(src);
    assert_eq!(converted, ".a,\n.b{\n  c: 1;\n}\n");
    let original = parse(src, Syntax::Sass).expect("oxc sass");
    let conv = parse(&converted, Syntax::Scss).expect("scss");
    assert_ne!(
        normalize(&original),
        normalize(&conv),
        "oxc 已支持同缩进续行：请删除 EqualAfterFold 豁免"
    );
    let folded = fold_selector_continuations(src);
    assert_eq!(folded, ".a, .b\n  c: 1\n");
    assert_eq!(
        normalize(&parse(&folded, Syntax::Sass).expect("folded")),
        normalize(&conv),
        "折行后必须与转换结果等价"
    );
}

/// `oxc-3` 探针：块内第一行 `//` 注释在 oxc 里会结束该块。
/// 去注释后必须与转换结果等价；oxc 修好后这里会失败 → 删除 `EqualAfterStrip` 豁免。
#[test]
fn oxc_3_line_comment_ends_block() {
    let src = ".p\n  // note\n  a: 1\n";
    let converted = to_scss(src);
    assert_eq!(converted, ".p{\n  // note\n  a: 1;\n}\n");
    let conv = normalize(&parse(&converted, Syntax::Scss).expect("scss"));
    let original = normalize(&parse(src, Syntax::Sass).expect("sass"));
    assert_ne!(
        original, conv,
        "oxc 已正确处理块内首行 // 注释：请删除 EqualAfterStrip 豁免"
    );
    let stripped = strip_line_comments(src);
    assert_eq!(stripped, ".p\n  a: 1\n");
    assert_eq!(
        normalize(&parse(&stripped, Syntax::Sass).expect("stripped")),
        conv,
        "去注释后必须与转换结果等价"
    );
}

/// `oxc-2` 探针：未闭合 `/*` 在 oxc 里吃到 EOF（dart 按缩进收尾）。
#[test]
fn oxc_2_unterminated_block_comment_eats_everything() {
    let src = "/* a\n   b\n.c\n  color: red\n";
    assert!(has_unterminated_comment(src));
    let ast = parse(src, Syntax::Sass).expect("oxc sass");
    assert!(
        ast.contains("statements: Vec([])"),
        "oxc 已按缩进结束 /*：请删除 SkipUnterminatedComment 豁免"
    );
    // 转换器按 dart 语义在缩进回退处收尾，规则仍然保留。
    assert_eq!(to_scss(src), "/* a\n   b */\n.c{\n  color: red;\n}\n");
}
