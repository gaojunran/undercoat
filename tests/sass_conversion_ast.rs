//! Objective gate for the `.sass` converter: **AST equivalence**.
//!
//! `oxc-css-parser`'s `Syntax::Sass` and `Syntax::Scss` produce isomorphic ASTs (`=m` / `+m`
//! differ from `@mixin` / `@include` only in `AtRule.name.raw`). So converter correctness can
//! be decided like this:
//!
//! 1. `.sass` source → parsed with `Syntax::Sass` → AST_original
//! 2. converter → SCSS text → parsed with `Syntax::Scss` → AST_converted
//! 3. after erasing `span` and every `raw` (source spelling) from both ASTs, the `Debug`
//!    strings must be exactly equal
//!
//! oxc's `Syntax::Sass` has two parsing quirks **of its own** (each pinned down by an
//! `oxc_*` probe below; once oxc fixes them the probe fails and tells you to delete the
//! corresponding exemption):
//!
//! - `oxc-1` continuation lines at the same indent: dart allows the continuation lines of a
//!   selector list to be indented like the first line, while oxc only accepts **deeper**
//!   indentation, so `.a,` + `.b` is split into "an empty block rule + the next rule".
//!   Workaround: fold the continuation lines into one line
//!   (`fold_selector_continuations`) before parsing with oxc, and compare equivalence tree
//!   by tree as usual.
//! - `oxc-2` unterminated `/*`: dart ends the comment body at the indentation, while oxc
//!   runs to end of file (the AST becomes empty), so the Sass AST of such files cannot
//!   serve as an oracle; they are explicitly skipped and counted.
//! - `oxc-3` a `//` comment line inside a block: oxc treats it as ending the block (later
//!   statements escape the block); when the block contains only comments it even hard-fails
//!   at EOF with `Unexpected("<eof>", "<dedent>")`. Workaround: delete the `//` comment
//!   lines (together with the deeper continuation lines they swallow) before parsing —
//!   comments never enter the AST, so deleting them does not affect the comparison.
//! - `oxc-4` an unquoted `@import ../a/b.sass` (dart accepts it): oxc fails at the `.`.
//!   Workaround: quote the URL before parsing (`ImportPrelude.href` keeps its value).
//!
//! The corpus directory can be overridden with `SCSS_SASS_CORPUS` (default
//! `/tmp/opencode/scss-corpus`); when the directory does not exist the corpus part is
//! skipped and only this repository's fixtures run.

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

/// Erase `span` and `raw`: `Span { start: 1, end: 2 }` → `Span`, `raw: "x"` → dropped.
///
/// Everything else is left untouched — including the **number** of `comma_spans` (one extra
/// comma means the selector list structure changed and must be exposed).
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
    /// Equivalent as is.
    Equal,
    /// Equivalent after folding same-indent continuation lines (`oxc-1`).
    EqualAfterFold,
    /// Equivalent after also stripping `//` comment lines (`oxc-3`).
    EqualAfterStrip,
    /// Equivalent after also quoting `@import` URLs (`oxc-4`).
    EqualAfterQuote,
    /// Unterminated `/*`, so oxc's Sass AST is unusable (`oxc-2`).
    SkipUnterminatedComment,
    /// oxc's Sass parser rejects this source in every variant.
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
        Err(e) => return Status::Mismatch(format!("converted SCSS failed to parse: {e}")),
    };
    // Try the variants one by one: first as is, then stacking exemptions (the amount of
    // modification grows monotonically).
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
                "first difference @ {at}\n  sass: …{}\n  conv: …{}",
                &a[lo..(at + 140).min(a.len())],
                &conv_ast[lo..(at + 140).min(conv_ast.len())]
            ))
        }
        None if parsed_any => {
            Status::Mismatch("a variant parsed but is not comparable".to_string())
        }
        None => Status::SkipOxcUnparseable,
    }
}

/// Quote unquoted `@import` URLs (dart's indented syntax allows them, oxc does not; see
/// `oxc-4`).
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

/// Delete `//` comment lines together with the deeper continuation lines they swallow
/// (dart's comment continuation).
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

/// `/*` is not closed on this line (dart closes it at the indentation, oxc runs to EOF).
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
        eprintln!(
            "[skip: oxc Syntax::Sass rejects it] {}: {err}",
            path.display()
        );
    }
    for path in &report.skip_comment {
        eprintln!("[skip: unterminated /*] {}", path.display());
    }
    for path in &report.after_fold {
        eprintln!("[ok via oxc-1 fold] {}", path.display());
    }
    for path in &report.after_strip {
        eprintln!("[ok via oxc-3 comment strip] {}", path.display());
    }
    for path in &report.after_quote {
        eprintln!("[ok via oxc-4 quote] {}", path.display());
    }
    for (path, diff) in &report.mismatched {
        eprintln!("[FAIL: AST not equivalent] {}\n{diff}", path.display());
    }
    assert!(
        report.mismatched.is_empty(),
        "{label}: {}/{} equivalent ({} of them after folding), {} skipped for an unterminated comment, {} rejected by oxc, {} not equivalent",
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
    assert!(
        files.len() >= 13,
        "unexpected number of fixtures: {}",
        files.len()
    );
    let report = check(&files);
    assert_clean(&report, "fixtures");
    assert_eq!(
        report.equal,
        files.len(),
        "every fixture must be AST-equivalent"
    );
}

#[test]
fn corpus_ast_is_equivalent_after_conversion() {
    let dir = PathBuf::from(
        std::env::var("SCSS_SASS_CORPUS").unwrap_or_else(|_| "/tmp/opencode/scss-corpus".into()),
    );
    if !dir.is_dir() {
        eprintln!("skipping the corpus gate: {} does not exist", dir.display());
        return;
    }
    let mut files = Vec::new();
    collect(&dir, &mut files);
    files.sort();
    assert!(
        files.len() >= 90,
        "unexpected number of corpus files: {}",
        files.len()
    );
    let report = check(&files);
    assert_clean(&report, "corpus");
    eprintln!(
        "corpus AST equivalence: {}/{} ({} after folding, {} after stripping comments, {} after quoting)",
        report.equal,
        report.checked,
        report.after_fold.len(),
        report.after_strip.len(),
        report.after_quote.len()
    );
}

/// `oxc-1` probe: oxc's Sass parser does not accept continuation lines at the same indent
/// as their first line (dart does). After folding it must be equivalent to the converted
/// result; if oxc fixes this, this test fails → delete the `EqualAfterFold` exemption.
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
        "oxc now supports same-indent continuation lines: please delete the EqualAfterFold exemption"
    );
    let folded = fold_selector_continuations(src);
    assert_eq!(folded, ".a, .b\n  c: 1\n");
    assert_eq!(
        normalize(&parse(&folded, Syntax::Sass).expect("folded")),
        normalize(&conv),
        "folding must still be equivalent to the converted result"
    );
}

/// `oxc-3` probe: a `//` comment as the first line of a block ends that block in oxc.
/// After stripping comments it must be equivalent to the converted result; once oxc fixes
/// this it fails here → delete the `EqualAfterStrip` exemption.
#[test]
fn oxc_3_line_comment_ends_block() {
    let src = ".p\n  // note\n  a: 1\n";
    let converted = to_scss(src);
    assert_eq!(converted, ".p{\n  // note\n  a: 1;\n}\n");
    let conv = normalize(&parse(&converted, Syntax::Scss).expect("scss"));
    let original = normalize(&parse(src, Syntax::Sass).expect("sass"));
    assert_ne!(
        original, conv,
        "oxc now handles a leading // comment inside a block correctly: please delete the EqualAfterStrip exemption"
    );
    let stripped = strip_line_comments(src);
    assert_eq!(stripped, ".p\n  a: 1\n");
    assert_eq!(
        normalize(&parse(&stripped, Syntax::Sass).expect("stripped")),
        conv,
        "stripping comments must still be equivalent to the converted result"
    );
}

/// `oxc-2` probe: an unterminated `/*` runs to EOF in oxc (dart closes it at the
/// indentation).
#[test]
fn oxc_2_unterminated_block_comment_eats_everything() {
    let src = "/* a\n   b\n.c\n  color: red\n";
    assert!(has_unterminated_comment(src));
    let ast = parse(src, Syntax::Sass).expect("oxc sass");
    assert!(
        ast.contains("statements: Vec([])"),
        "oxc now ends /* at the indentation: please delete the SkipUnterminatedComment exemption"
    );
    // The converter closes at the indentation fallback per dart semantics, so the rule is
    // still kept.
    assert_eq!(to_scss(src), "/* a\n   b */\n.c{\n  color: red;\n}\n");
}
