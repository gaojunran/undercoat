//! The `.sass` indented syntax -> equivalent SCSS text.
//!
//! Why the text is rewritten at load time: `@import` is **text-level** inlining (see
//! `imports.rs`), and the whole pipeline shares one merged text and one oxc parse
//! (`Syntax::Scss`). Folding the indented syntax into SCSS at load time leaves
//! evaluation/serialization/sourcemap untouched, and `.scss` and `.sass` importing or
//! `@use`-ing each other (observed to be allowed in dart 1.93.2) works naturally.
//!
//! The rewrite only does "structural insertion": insert `{` at the end of a block-head
//! line, `;` at the end of a statement line, and `}` at an insertable position of the
//! last line when a block closes (before a trailing `//` comment), and normalize `=m` /
//! `+m` into `@mixin m` / `@include m`.
//! It **adds and removes no lines**, so line numbers correspond one-to-one with the
//! source (both the error locations in `imports.rs` and `align_to_disk` of
//! `compiler-style` can be reused directly), and the column positions of existing
//! characters do not move.
//!
//! Structural decisions follow dart-sass 1.93.2 as observed, item by item:
//! - Deeper indentation starts a child block; dedenting closes blocks (possibly several
//!   levels at once).
//! - Declarations and selectors are told apart by "whether whitespace immediately
//!   follows the first top-level colon": `color: red` is a declaration, while `a:hover` /
//!   `tr:not(.x)` / `color:red` are all selectors (the old `:color red` form is a
//!   selector as well, consistent with 1.93.2; it is not implemented as a property).
//! - Continuation lines: an unclosed `(`/`[`/`#{`, a trailing operator (`1 +`), and a
//!   trailing `and`/`or`/`not`/`in`/`from`/`through`/`to` of a directive or declaration;
//!   comma continuation belongs to selector lists only (and the next non-empty line must
//!   not dedent); a continuation must be deeper than the statement's first line.
//! - A leading `//` comment swallows deeper continuation lines (dart's comment
//!   continuation); here those lines keep the `//` prefix as a comment so that they do
//!   not turn into real statements.
//! - `//` is a line comment only outside parentheses (`url(http://x)` is not one); when a
//!   `/* */` does not close on the same line, the comment body is delimited by
//!   indentation and `*/` is appended on the last line.
//! - A statement without a child block gets an empty block or a semicolon (`{}` is
//!   dropped by the serialization layer, consistent with dart); an unquoted URL such as
//!   `@import ../a/b.sass` gets quotes and is then handed to the unified parser.
//!
//! `oxc-css-parser`'s `Syntax::Sass` differs from dart in two behaviors (pinned item by
//! item by the `oxc_*` probes in `tests/sass_conversion_ast.rs`): it only accepts
//! selector continuations with strictly deeper indentation, and it treats a `//` comment
//! line inside a block as the end of the block. This module follows dart's semantics, and
//! the AST gate exempts these differences by folding lines / stripping comments.
//!
//! Known deviations (none of them affects semantics):
//! - Line numbers and columns before the insertion point are exact; a `=m` / `+m` line is
//!   rewritten into `@mixin ` / `@include ` of a different length, so error columns on
//!   that line are off by 6 / 8 (line numbers are still exact).
//! - Continuation lines of a multi-line `/* */` keep their original indentation, while
//!   dart re-indents them as ` * …` (a comment, so semantically equivalent).

/// One output line. `code` is the "insertable position": before a trailing `//` comment
/// / unclosed `/*`, which is where terminators and block closings are inserted (anything
/// inserted after the comment would be eaten by it).
struct Line {
    text: String,
    code: usize,
    appendable: bool,
}

impl Line {
    fn new(text: String, code: usize) -> Self {
        Self {
            text,
            code,
            appendable: true,
        }
    }

    fn push_code(&mut self, s: &str) {
        self.text.insert_str(self.code, s);
        self.code += s.len();
    }
}

/// Statement kind: decides comma continuation and `=`/`+` normalization.
#[derive(Clone, Copy, PartialEq)]
enum Kind {
    AtRule,
    MixinDef,
    Include,
    Decl,
    Selector,
    Comment,
}

/// An unfinished statement.
struct Pending {
    indent: usize,
    kind: Kind,
}

/// A multi-line `/*` comment: the indentation of its first line decides where it ends;
/// `last` is the current last line (where the closing `*/` goes).
struct OpenComment {
    indent: usize,
    last: usize,
}

/// An open block.
struct Block {
    indent: usize,
    line: usize,
}

/// Scan state carried across lines (strings / parentheses / block comments inside
/// continuation lines).
#[derive(Default)]
struct Scan {
    quote: Option<u8>,
    paren: i32,
    in_block_comment: bool,
}

struct ScanOut {
    /// Terminator insertion point.
    code: usize,
    /// The statement may end on this line (no unclosed string / parentheses / block
    /// comment).
    complete: bool,
}

/// Rewrite a piece of `.sass` source text into equivalent SCSS.
#[doc(hidden)]
pub fn to_scss(src: &str) -> String {
    let lines: Vec<&str> = src.split('\n').collect();
    let blank: Vec<bool> = lines.iter().map(|l| l.trim().is_empty()).collect();
    let indents: Vec<usize> = lines
        .iter()
        .map(|l| l.len() - l.trim_start_matches([' ', '\t']).len())
        .collect();
    // next[i]: indentation of the first non-empty line after i (used to decide "child
    // block" and comma continuation).
    let mut next: Vec<Option<usize>> = vec![None; lines.len()];
    let mut upcoming = None;
    for i in (0..lines.len()).rev() {
        next[i] = upcoming;
        if !blank[i] {
            upcoming = Some(indents[i]);
        }
    }

    let mut out: Vec<Line> = Vec::with_capacity(lines.len());
    let mut blocks: Vec<Block> = Vec::new();
    let mut last_appendable: Option<usize> = None;
    let mut pending: Option<Pending> = None;
    let mut line_comment: Option<usize> = None;
    // A statement-level `/*` comment: when it does not close on this line, the body is
    // delimited by indentation (dart's indented-syntax behavior).
    let mut block_comment: Option<OpenComment> = None;
    let mut scan = Scan::default();

    for i in 0..lines.len() {
        let raw = lines[i];
        let indent = indents[i];

        // Dedenting ends an unclosed `/*` comment (dart: the body of `/*` is an indented
        // block).
        if block_comment
            .as_ref()
            .is_some_and(|c| !blank[i] && indent <= c.indent)
        {
            if let Some(c) = block_comment.take() {
                let target = &mut out[c.last];
                let at = target.code;
                target.text.insert_str(at, " */");
                target.code += 3;
            }
            scan.in_block_comment = false;
        }

        // The deeper continuation lines swallowed by a leading `//` comment stay comments.
        if let Some(base) = line_comment {
            if !blank[i] && indent <= base {
                line_comment = None;
            } else {
                let (text, code) = if blank[i] {
                    (raw.to_string(), raw.len())
                } else {
                    (format!("{}// {}", &raw[..indent], &raw[indent..]), indent)
                };
                // Nothing can be appended to a comment line: a `}` inserted before `//`
                // would be eaten by the comment, and inserting it after is meaningless.
                let mut line = Line::new(text, code);
                line.appendable = false;
                out.push(line);
                continue;
            }
        }

        if blank[i] {
            out.push(Line::new(raw.to_string(), raw.len()));
            last_appendable = Some(out.len() - 1);
            continue;
        }

        // The line starts inside a block comment: nothing can be appended to the whole
        // line; the statement (if any) continues after the comment closes.
        if scan.in_block_comment {
            let done = scan_line(raw, &mut scan);
            out.push(Line {
                text: raw.to_string(),
                code: done.code,
                appendable: false,
            });
            if scan.in_block_comment {
                if let Some(c) = block_comment.as_mut() {
                    c.last = out.len() - 1;
                }
            } else {
                block_comment = None;
            }
            if done.complete && pending.is_some() {
                let line = out.len() - 1;
                finish_statement(line, &mut out, &mut pending, &mut blocks, next[i]);
                last_appendable = Some(line);
            }
            continue;
        }

        if pending.is_none() {
            let kind = kind_of(&raw[indent..]);
            if kind == Kind::Comment {
                // A statement-level comment: produces no statement and takes no part in the
                // block structure.
                let done = scan_line(raw, &mut scan);
                out.push(Line {
                    text: raw.to_string(),
                    code: done.code,
                    appendable: false,
                });
                if raw[indent..].starts_with("//") {
                    line_comment = Some(indent);
                } else if scan.in_block_comment {
                    block_comment = Some(OpenComment {
                        indent,
                        last: out.len() - 1,
                    });
                }
                continue;
            }
            // Close blocks: exit once the indentation is not deeper than the block head
            // (possibly several levels at once).
            while blocks.last().is_some_and(|b| b.indent >= indent) {
                close_block(&mut out, &mut blocks, &mut last_appendable);
            }
            pending = Some(Pending { indent, kind });
        }

        let kind = pending.as_ref().map_or(Kind::Selector, |p| p.kind);
        let stmt_indent = pending.as_ref().map_or(indent, |p| p.indent);
        let done = scan_line(raw, &mut scan);

        let mut line = Line::new(raw.to_string(), done.code);
        if indent < done.code {
            let leading = &raw.as_bytes()[indent..];
            match (kind, leading.first()) {
                (Kind::MixinDef, Some(b'=')) => {
                    line.text.replace_range(indent..indent + 1, "@mixin ");
                    line.code += "@mixin ".len() - 1;
                }
                (Kind::Include, Some(b'+')) => {
                    line.text.replace_range(indent..indent + 1, "@include ");
                    line.code += "@include ".len() - 1;
                }
                (Kind::AtRule, Some(b'@')) => {
                    quote_import(&mut line, indent);
                }
                _ => {}
            }
        }
        line.appendable = !scan.in_block_comment;
        let appendable = line.appendable;
        let code = line.code;
        out.push(line);
        if appendable {
            last_appendable = Some(out.len() - 1);
        }

        if !done.complete || continues(&out[out.len() - 1].text[..code], kind, next[i], stmt_indent)
        {
            continue;
        }
        let line = out.len() - 1;
        finish_statement(line, &mut out, &mut pending, &mut blocks, next[i]);
        last_appendable = Some(line);
    }

    while !blocks.is_empty() {
        close_block(&mut out, &mut blocks, &mut last_appendable);
    }

    let mut text = out
        .iter()
        .map(|l| l.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    if src.ends_with('\n') && !text.ends_with('\n') {
        text.push('\n');
    }
    text
}

/// Fold continuation lines of a selector list into one line (only used as the oracle of
/// the AST gate).
///
/// oxc's `Syntax::Sass` only treats the next line as a statement continuation when it is
/// **indented deeper** (dart also continues at the same indentation), so the
/// "converter vs oxc Sass" AST conformance run must first fold continuation lines into
/// one line before comparing the same semantics. Only lines with `kind == Selector` that
/// end in a top-level comma are folded; continuations of declarations/directives are
/// unaffected.
#[doc(hidden)]
pub fn fold_selector_continuations(src: &str) -> String {
    let lines: Vec<&str> = src.split('\n').collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut scan = Scan::default();
    let mut i = 0;
    while i < lines.len() {
        let raw = lines[i];
        let done = scan_line(raw, &mut scan);
        let trimmed = raw[..done.code].trim();
        let foldable = !trimmed.is_empty()
            && !trimmed.starts_with("//")
            && !trimmed.starts_with("/*")
            && !scan.in_block_comment
            && scan.paren == 0
            && trimmed.ends_with(',')
            && kind_of(trimmed) == Kind::Selector;
        if !foldable {
            out.push(raw.to_string());
            i += 1;
            continue;
        }
        // The continuation must be **merged into the same line**: leaving an empty line
        // would make oxc consider the statement finished.
        let mut joined = raw.trim_end().to_string();
        i += 1;
        while let Some(j) = (i..lines.len()).find(|&j| !lines[j].trim().is_empty()) {
            let next = lines[j].trim();
            joined.push(' ');
            joined.push_str(next);
            i = j + 1;
            if !next.ends_with(',') {
                break;
            }
        }
        out.push(joined);
    }
    out.join("\n")
}

/// SCSS parse failure after conversion: map the error back to the `.sass` source position
/// and dart's message.
///
/// Shapes that can be reproduced exactly take priority (observed with dart 1.93.2):
/// - `{`: `Expected newline.` @ `{`
/// - a top-level comma at the end of a statement line (not a selector list): dart's error
///   position is **right after the comma**
///   - declaration + deeper continuation line / directive -> `Expected identifier.`
///   - declaration + not deeper -> `expected selector.`
///   - variable declaration (`$x: 1,`) -> `Nothing may be indented beneath a variable
///     declaration.`
///     @ the content start of the next deeper line
///
/// Otherwise only the **line number** is guaranteed to match the source (the rewrite adds
/// and removes no lines), and the column is clamped to the end of that line's code.
pub(crate) fn parse_error(
    src: &str,
    converted: &str,
    at: usize,
    kind: oxc_css_parser::error::ErrorKind,
) -> (usize, String) {
    if let Some(brace) = stray_brace(src) {
        return (brace, "Expected newline.".to_string());
    }
    let index = crate::LineIndex::new(converted);
    let (line, col) = index.line_col(at, converted);
    let (line, col) = (line as usize, col as usize);
    if let Some((off, message)) = trailing_comma_error(src, line) {
        return (off, message);
    }
    (
        source_line_end(src, line, col),
        format!("SCSS parse failure: {kind}"),
    )
}

/// Column `col` on the 0-based line `line` (clamped to the end of the code when it
/// overflows the line).
fn source_line_end(src: &str, line: usize, col: usize) -> usize {
    let mut start = 0;
    for (i, raw) in src.split('\n').enumerate() {
        if i == line {
            return start + col.min(raw.trim_end().len());
        }
        start += raw.len() + 1;
    }
    src.len()
}

/// A top-level comma at the end of a statement line (that is where dart reports).
/// Returns `(offset, dart message)`.
fn trailing_comma_error(src: &str, fail_line: usize) -> Option<(usize, String)> {
    let lines: Vec<&str> = src.split('\n').collect();
    let indent_of = |i: usize| lines[i].len() - lines[i].trim_start_matches([' ', '\t']).len();
    let next_line = |i: usize| (i + 1..lines.len()).find(|&j| !lines[j].trim().is_empty());
    let line_start = |i: usize| lines[..i].iter().map(|l| l.len() + 1).sum::<usize>();

    let mut scan = Scan::default();
    let mut offset = 0usize;
    let mut fallback: Option<(usize, String)> = None;
    for (i, raw) in lines.iter().enumerate() {
        let start = offset;
        offset += raw.len() + 1;
        let done = scan_line(raw, &mut scan);
        let code = &raw[..done.code];
        let trimmed = code.trim();
        if trimmed.is_empty()
            || trimmed.starts_with("//")
            || trimmed.starts_with("/*")
            || scan.in_block_comment
            || !trimmed.ends_with(',')
            || scan.paren != 0
        {
            continue;
        }
        // dart's position is **after** the comma (where the missing identifier should be),
        // so no -1 is applied.
        let comma = start + code.trim_end().len();
        let deeper = next_line(i).is_some_and(|j| indent_of(j) > indent_of(i));
        let (at, message) = if trimmed.starts_with('$') {
            let at = next_line(i).map_or(comma, |j| line_start(j) + indent_of(j));
            (
                at,
                "Nothing may be indented beneath a variable declaration.",
            )
        } else if kind_of(trimmed) == Kind::AtRule || deeper {
            (comma, "Expected identifier.")
        } else {
            (comma, "expected selector.")
        };
        let message = message.to_string();
        if i <= fail_line {
            fallback = Some((at, message));
        } else {
            return fallback.or(Some((at, message)));
        }
    }
    fallback
}

/// The first `{` that is not part of `#{}` / a string / a comment (writing `{}` in
/// `.sass` is a hard error).
pub(crate) fn stray_brace(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut i = 0;
    let mut quote: Option<u8> = None;
    let mut comment = false;
    while i < bytes.len() {
        let c = bytes[i];
        if comment {
            if c == b'*' && bytes.get(i + 1) == Some(&b'/') {
                comment = false;
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        if let Some(q) = quote {
            match c {
                b'\\' => i += 2,
                c if c == q => {
                    quote = None;
                    i += 1;
                }
                _ => i += 1,
            }
            continue;
        }
        match c {
            b'\'' | b'"' => {
                quote = Some(c);
                i += 1;
            }
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                comment = true;
                i += 2;
            }
            b'#' if bytes.get(i + 1) == Some(&b'{') => i += 2,
            b'{' => return Some(i),
            _ => i += 1,
        }
    }
    None
}

/// The indented syntax allows an unquoted URL such as `@import ../a/b.sass` (observed
/// with dart 1.93.2; the same form in SCSS reports `Expected string`); here quotes are
/// added and it is handed to the unified `@import` parsing.
fn quote_import(line: &mut Line, indent: usize) {
    let body = &line.text[indent..line.code];
    let Some(rest) = body.strip_prefix("@import") else {
        return;
    };
    if rest.contains(['"', '\'']) || rest.contains("url(") {
        return;
    }
    let quoted = rest
        .split(',')
        .map(|part| {
            let part = part.trim();
            if part.is_empty() {
                String::new()
            } else {
                format!("\"{part}\"")
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    if quoted.is_empty() {
        return;
    }
    line.text
        .replace_range(indent + "@import".len()..line.code, &format!(" {quoted}"));
    line.code = indent + "@import".len() + 1 + quoted.len();
}

/// End of a statement: `{` when the indentation is deeper (opens a block), otherwise `;`.
fn finish_statement(
    line: usize,
    out: &mut [Line],
    pending: &mut Option<Pending>,
    blocks: &mut Vec<Block>,
    next_indent: Option<usize>,
) {
    let Some(p) = pending.take() else { return };
    if next_indent.is_some_and(|n| n > p.indent) {
        out[line].push_code("{");
        blocks.push(Block {
            indent: p.indent,
            line,
        });
        return;
    }
    // A statement without a child block: selectors and directives that need a block get
    // `{}` (dart emits no CSS for such empty blocks), the rest (`@import` / `@use` /
    // `@return` etc.) get `;`.
    let head = out[line].text.get(..out[line].code).unwrap_or_default();
    let needs_block =
        matches!(p.kind, Kind::Selector | Kind::MixinDef) || needs_block_at_rule(head);
    out[line].push_code(if needs_block { "{}" } else { ";" });
}

/// Whether the directive is carried by a "block" (dart: without a child block the whole
/// directive is dropped, which is equivalent to an empty block).
fn needs_block_at_rule(head: &str) -> bool {
    let Some(name) = head.trim_start().strip_prefix('@') else {
        return false;
    };
    let name = name
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '-')
        .next()
        .unwrap_or("");
    matches!(
        name.to_ascii_lowercase().as_str(),
        "media"
            | "supports"
            | "if"
            | "else"
            | "each"
            | "for"
            | "while"
            | "mixin"
            | "function"
            | "at-root"
            | "layer"
            | "container"
            | "scope"
            | "document"
            | "page"
            | "property"
            | "starting-style"
            | "keyframes"
            | "font-face"
    )
}

fn close_block(out: &mut [Line], blocks: &mut Vec<Block>, last_appendable: &mut Option<usize>) {
    let Some(block) = blocks.pop() else { return };
    let target = match *last_appendable {
        Some(t) if t >= block.line => t,
        _ => block.line,
    };
    out[target].push_code("}");
    *last_appendable = Some(target);
}

/// Whether the statement continues onto the next line.
///
/// A comma continues only inside a selector list (dart: after `&:focus,` both the same
/// and a deeper indentation continue, dedenting does not); a trailing operator (`1 +`)
/// and keywords such as `and` / `in` / `from` continue as well.
fn continues(code: &str, kind: Kind, next_indent: Option<usize>, stmt_indent: usize) -> bool {
    let code = code.trim_end();
    let Some(&last) = code.as_bytes().last() else {
        return false;
    };
    if kind == Kind::Selector {
        return match last {
            // Comma continuation: the next non-empty line does not dedent (dart: dedenting
            // means the comma is the end).
            b',' => next_indent.is_some_and(|n| n >= stmt_indent),
            // Only a combinator on a line of its own (a `>` followed by a deeper next line)
            // continues; a trailing combinator (`.a >`) is a **complete statement**
            // (observed with dart: lines after `.a >` are treated as nested, not joined into
            // the selector).
            b'>' | b'+' | b'~' => {
                code.trim().len() == 1 && next_indent.is_some_and(|n| n >= stmt_indent)
            }
            _ => false,
        };
    }
    // Any other continuation must be deeper than the first line of the statement (dart
    // decides continuation by indentation; after a complete statement such as
    // `@use "x" as *`, a dedented next line must not count as a continuation).
    if !next_indent.is_some_and(|n| n > stmt_indent) {
        return false;
    }
    match last {
        // Arithmetic / comparison operators (`%` does not count: a trailing `96%` is a
        // complete value, not a modulo).
        b'+' | b'*' | b'/' | b'=' | b'>' | b'<' => true,
        // `-` only counts as an operator when whitespace is on both sides (`margin: -1`
        // does not continue by mistake).
        b'-' => code
            .as_bytes()
            .get(code.len().wrapping_sub(2))
            .is_some_and(|c| c.is_ascii_whitespace()),
        _ => {
            // Keyword continuation belongs to directives and declarations
            // (`@for $i from 1 through`, `@if a and`, `$x: a and` + an indented continuation
            // line).
            if !matches!(kind, Kind::AtRule | Kind::Decl) {
                return false;
            }
            let word = code
                .rsplit(|c: char| !c.is_ascii_alphanumeric())
                .next()
                .unwrap_or("");
            matches!(
                word,
                "and" | "or" | "not" | "in" | "from" | "through" | "to"
            ) && code.ends_with(&format!(" {word}"))
        }
    }
}

/// Scan one line: update the cross-line state and report the terminator insertion point
/// and whether "the statement may end here".
fn scan_line(raw: &str, st: &mut Scan) -> ScanOut {
    let bytes = raw.as_bytes();
    let mut i = 0;
    let mut code = raw.len();
    let mut comment_at: Option<usize> = None;
    while i < bytes.len() {
        if st.in_block_comment {
            if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
                st.in_block_comment = false;
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        let c = bytes[i];
        if let Some(q) = st.quote {
            if c == b'\\' {
                i += 2;
                continue;
            }
            if c == q {
                st.quote = None;
            }
            i += 1;
            continue;
        }
        match c {
            b'\'' | b'"' => {
                st.quote = Some(c);
                i += 1;
            }
            b'\\' => i += 2,
            // `//` is a line comment only outside parentheses (`url(http://x)` is not one).
            b'/' if bytes.get(i + 1) == Some(&b'/') && st.paren == 0 => {
                code = raw[..i].trim_end().len();
                break;
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => match raw[i + 2..].find("*/") {
                Some(rel) => i = i + 2 + rel + 2,
                None => {
                    st.in_block_comment = true;
                    comment_at.get_or_insert(raw[..i].trim_end().len());
                    i = bytes.len();
                }
            },
            b'(' | b'[' => {
                st.paren += 1;
                i += 1;
            }
            b')' | b']' => {
                st.paren = (st.paren - 1).max(0);
                i += 1;
            }
            b'#' if bytes.get(i + 1) == Some(&b'{') => {
                st.paren += 1;
                i += 2;
            }
            b'}' => {
                st.paren = (st.paren - 1).max(0);
                i += 1;
            }
            _ => i += 1,
        }
    }
    if st.in_block_comment {
        if let Some(at) = comment_at {
            code = at;
        }
    }
    ScanOut {
        code,
        complete: st.quote.is_none() && st.paren <= 0 && !st.in_block_comment,
    }
}

/// Statement kind (observed with dart 1.93.2: in the indented syntax `prop:value`
/// without a space is a selector).
fn kind_of(body: &str) -> Kind {
    let bytes = body.as_bytes();
    match bytes.first() {
        None => Kind::Selector,
        Some(b'@') => Kind::AtRule,
        Some(b'=') => Kind::MixinDef,
        Some(b'+') if bytes.get(1).is_some_and(|c| is_name_start(*c)) => Kind::Include,
        Some(b'/') if matches!(bytes.get(1), Some(b'/') | Some(b'*')) => Kind::Comment,
        _ => match first_top_level_colon(body) {
            Some(at) if bytes.get(at + 1).is_none_or(|c| c.is_ascii_whitespace()) => Kind::Decl,
            _ => Kind::Selector,
        },
    }
}

fn is_name_start(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'-' || c >= 0x80
}

/// Position of the first top-level colon (not inside a string / parentheses /
/// interpolation).
fn first_top_level_colon(body: &str) -> Option<usize> {
    let bytes = body.as_bytes();
    let mut i = 0;
    let mut paren = 0i32;
    let mut quote: Option<u8> = None;
    while i < bytes.len() {
        let c = bytes[i];
        if let Some(q) = quote {
            match c {
                b'\\' => i += 2,
                c if c == q => {
                    quote = None;
                    i += 1;
                }
                _ => i += 1,
            }
            continue;
        }
        match c {
            b'\'' | b'"' => {
                quote = Some(c);
                i += 1;
            }
            b'\\' => i += 2,
            b'(' | b'[' => {
                paren += 1;
                i += 1;
            }
            b')' | b']' => {
                paren -= 1;
                i += 1;
            }
            b'#' if bytes.get(i + 1) == Some(&b'{') => {
                paren += 1;
                i += 2;
            }
            b'}' => {
                paren -= 1;
                i += 1;
            }
            b':' if paren == 0 => return Some(i),
            _ => i += 1,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::to_scss;

    /// Insert `{` at the end of a block-head line, `;` at the end of a statement line, and
    /// `}` at the end of the last statement when closing a block (carried by the empty line
    /// at the end of the file).
    #[test]
    fn nested_rule_and_decl() {
        assert_eq!(to_scss(".a\n  color: red\n"), ".a{\n  color: red;\n}\n");
    }

    #[test]
    fn mixin_shorthand() {
        assert_eq!(
            to_scss("=m($p: 1px)\n  padding: $p\n.a\n  +m(2px)\n"),
            "@mixin m($p: 1px){\n  padding: $p;}\n.a{\n  @include m(2px);\n}\n"
        );
    }

    #[test]
    fn selector_comma_continuation_keeps_lines() {
        assert_eq!(
            to_scss(".a,\n.b\n  color: red\n"),
            ".a,\n.b{\n  color: red;\n}\n"
        );
    }

    /// Observed with dart 1.93.2: `prop:value` (no whitespace after the colon) in a
    /// selector position is not a declaration; a nested selector without a child block
    /// gets an empty block (neither side emits CSS).
    #[test]
    fn declaration_without_space_is_selector() {
        assert_eq!(to_scss(".a\n  color:red\n"), ".a{\n  color:red{}\n}\n");
    }

    #[test]
    fn nested_property_block() {
        assert_eq!(
            to_scss(".a\n  font:\n    family: serif\n"),
            ".a{\n  font:{\n    family: serif;\n}}\n"
        );
    }

    #[test]
    fn if_else_chain() {
        assert_eq!(
            to_scss("@if $x\n  a: 1\n@else\n  a: 2\n"),
            "@if $x{\n  a: 1;}\n@else{\n  a: 2;\n}\n"
        );
    }

    /// A leading `//` comment swallows deeper continuation lines (dart's comment
    /// continuation); they stay comments here.
    #[test]
    fn line_comment_block_swallows_deeper_lines() {
        assert_eq!(
            to_scss(".a\n  color: red\n  // note\n    deeper\n.b\n  color: blue\n"),
            ".a{\n  color: red;}\n  // note\n    // deeper\n.b{\n  color: blue;\n}\n"
        );
    }

    /// The terminator goes before a trailing `//` comment, otherwise it would be eaten by
    /// it.
    #[test]
    fn trailing_comment_keeps_terminator_before_it() {
        assert_eq!(
            to_scss(".a\n  color: red // note\n  background: blue\n"),
            ".a{\n  color: red; // note\n  background: blue;\n}\n"
        );
    }

    /// A value continues through a trailing operator, and the continuation must be deeper.
    #[test]
    fn operator_continuation() {
        assert_eq!(
            to_scss("$x: 1 +\n  2\n.a\n  width: $x\n"),
            "$x: 1 +\n  2;\n.a{\n  width: $x;\n}\n"
        );
    }

    /// A dedented next line after a complete statement must not continue it (`@use "x" as
    /// *` is the typical case).
    #[test]
    fn complete_statement_never_continues_on_dedent() {
        assert_eq!(
            to_scss("@use \"a/star\" as *\n.a\n  color: red\n"),
            "@use \"a/star\" as *;\n.a{\n  color: red;\n}\n"
        );
    }

    #[test]
    fn blank_lines_are_preserved() {
        assert_eq!(
            to_scss(".a\n  color: red\n\n  background: blue\n"),
            ".a{\n  color: red;\n\n  background: blue;\n}\n"
        );
    }

    /// An unclosed `/*` is closed by dedentation (dart's indented-syntax behavior) and must
    /// not swallow the rest of the file.
    #[test]
    fn block_comment_closed_by_dedent() {
        assert_eq!(
            to_scss("/* a\n  b\n.c\n  color: red\n"),
            "/* a\n  b */\n.c{\n  color: red;\n}\n"
        );
    }

    /// `from` / `to` of `@keyframes` are selectors, not continuation keywords.
    #[test]
    fn keyframe_selectors_are_not_keyword_continuations() {
        assert_eq!(
            to_scss("@keyframes s\n  from\n    opacity: 0\n  to\n    opacity: 1\n"),
            "@keyframes s{\n  from{\n    opacity: 0;}\n  to{\n    opacity: 1;\n}}\n"
        );
    }

    /// The `//` in `url(http://x)` is not a line comment.
    #[test]
    fn slashes_inside_parens_are_not_comments() {
        assert_eq!(
            to_scss(".a\n  background: url(http://x/y.png)\n"),
            ".a{\n  background: url(http://x/y.png);\n}\n"
        );
    }

    /// A statement without a child block: selectors get an empty block (dart emits no CSS),
    /// `@import` gets a semicolon.
    #[test]
    fn bodyless_statement_gets_empty_block_or_semicolon() {
        assert_eq!(
            to_scss(".gone\n.kept\n  color: red\n"),
            ".gone{}\n.kept{\n  color: red;\n}\n"
        );
        assert_eq!(
            to_scss("@import \"a\"\n.b\n  color: red\n"),
            "@import \"a\";\n.b{\n  color: red;\n}\n"
        );
    }

    /// A declaration value can also continue through a trailing `and` plus deeper
    /// indentation (observed with dart).
    #[test]
    fn declaration_value_continues_on_keyword() {
        assert_eq!(
            to_scss("$a: true and\n  false\n.b\n  c: 1\n"),
            "$a: true and\n  false;\n.b{\n  c: 1;\n}\n"
        );
    }
}
