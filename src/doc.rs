//! Output intermediate representation and printer: the output layer shared by both
//! dialects (scss / less).
//!
//! The evaluation stage produces an [`Item`] tree, and [`print`] writes it out as CSS in the
//! shape described by [`Style`] (feeding the sourcemap at the same time). Dialect differences
//! may only live in [`Style`] fields; semantic differences (dropping empty rules, dropping
//! empty `@media`, keeping `{}` for `@font-face`, discarding `//` comments, etc.) stay on
//! their respective evaluation side.

use oxc_css_parser::pos::Span;

use crate::Options;
use crate::merge::Merged;
use crate::print::{Printer, SourceSegment};

/// Output-shape configuration: the only place where the two dialects are allowed to differ.
pub(crate) struct Style {
    /// Whether to add a blank line after each style rule (including groups marked by
    /// group_end).
    pub(crate) blank_line_after_rule: bool,
    /// Whether selector-list continuation lines are indented to the current level; false =
    /// continuation lines start at column 0.
    pub(crate) indent_selector_continuation: bool,
    /// Whether to prepend `@charset "UTF-8";` when the content is non-ASCII.
    pub(crate) charset: bool,
    /// Whether to add a trailing newline at the end of the output.
    pub(crate) trailing_newline: bool,
    /// Whether the sourcemap includes `sourcesContent`. The two sides have **opposite**
    /// conditions (recorded in P11): dart (scss) always writes it (WDT's render parameters
    /// always carry `sourceMapContents: true`), whereas less 4.1.3 writes it only with
    /// multiple sources (`outputSourceFiles` follows `@import` inlining) and a single-source
    /// `.map` has no such field at all. The less side overrides this from `multi_source` when
    /// calling.
    pub(crate) sources_content: bool,
    /// Whether block comments are written verbatim (preserving the source's newlines and
    /// continuation-line indentation).
    ///
    /// true (less): less comment nodes write the original text straight back out, without
    /// reflowing continuation lines; false (dart): `write_with_indent` strips
    /// `min(minimum indentation, comment start column)` and then re-indents.
    pub(crate) verbatim_comments: bool,
}

impl Style {
    /// The expanded output shape of dart-sass 1.35.2 (SCSS pipeline).
    pub(crate) const SASS: Style = Style {
        blank_line_after_rule: true,
        indent_selector_continuation: false,
        charset: true,
        trailing_newline: false,
        sources_content: true,
        verbatim_comments: false,
    };

    /// The output shape of less 4.1.3 (LESS pipeline). All fields are inferred from
    /// measurements of lessc:
    ///
    /// - `blank_line_after_rule = false`: only a newline appears between adjacent rules /
    ///   comments / `@media` (`tests/fixtures/basic.css` has `}\n.a {`; in the 144329-byte
    ///   bootstrap 3.4.1 output, `\n\n\n` occurs 0 times).
    /// - `indent_selector_continuation = true`: selector continuation lines are indented to
    ///   the current level (`@media print {` in `tests/fixtures/media-indent.css` contains
    ///   `  *,\n  *:before`; at the root level continuation lines start at column 0, i.e.
    ///   `.a,\n.b`).
    /// - `charset = false`: less does not add `@charset` for non-ASCII content — the output
    ///   of weui-wxss 2.6.26 contains non-ASCII comments but no `@charset`; an `@charset`
    ///   written in the source is emitted as-is by the evaluation side as an ordinary
    ///   at-rule (less keeps only the first one, see `less::Compiler::charset`).
    /// - `trailing_newline = true`: non-empty output always ends with a single `\n` (as in
    ///   every fixture), while empty input produces 0 bytes (nothing is added when `items`
    ///   is empty, see [`print`]).
    /// - `sources_content`: the baseline is `false` (not written for a single source), and
    ///   the less side overrides it from `multi_source` when calling — only after `@import`
    ///   inlining does it write `sourcesContent` like dart (measured: the single-source map
    ///   of `basic.less` has no such field, while `import-multiple.less` has it).
    /// - `verbatim_comments = true`: less block comments write the source bytes straight
    ///   back out (including continuation-line indentation), without dart's reflowing of
    ///   continuation lines.
    pub(crate) const LESS: Style = Style {
        blank_line_after_rule: false,
        indent_selector_continuation: true,
        charset: false,
        trailing_newline: true,
        sources_content: false,
        verbatim_comments: true,
    };
}

/// A selector list: the text of **each output line** plus that line's source span.
///
/// Splitting into lines exists solely so the printer can attach a segment to continuation
/// lines too: dart maps every line of the list to the list's start (when one piece of text
/// spans several lines, each line gets the same source position), so all lines here share one
/// `Span`. The text is byte-for-byte equal to the unsplit `a,\nb` form.
pub(crate) type SelectorLines = Vec<(String, Span)>;

/// Split the whole selector text into [`SelectorLines`] by output line (one shared span per
/// line).
pub(crate) fn selector_lines(text: &str, span: Span) -> SelectorLines {
    text.split('\n')
        .map(|line| (line.to_string(), span))
        .collect()
}

#[derive(Clone)]
pub(crate) struct Rule {
    pub(crate) selector: SelectorLines,
    pub(crate) body: Body,
    /// dart-sass's `isGroupEnd`: the last node of this source rule (including its nested
    /// output), followed by a blank line after serialization.
    pub(crate) group_end: bool,
    /// The id used by the extend engine (`@extend` writes selectors back through it).
    pub(crate) extend_id: Option<usize>,
    /// After extension only placeholder selectors remain: this rule produces no output.
    pub(crate) placeholder: bool,
}

/// A keyframe rule inside `@keyframes`: unlike style rules, there are **no** blank lines
/// between them.
#[derive(Clone)]
pub(crate) struct Keyframe {
    pub(crate) selector: SelectorLines,
    pub(crate) body: Body,
}

#[derive(Clone)]
pub(crate) struct AtRule {
    pub(crate) name: String,
    pub(crate) name_span: Span,
    pub(crate) prelude: String,
    pub(crate) body: Option<Body>,
    pub(crate) group_end: bool,
    /// Marks a node that should be lifted outside the enclosing media to become a sibling
    /// after nested `@media` merging.
    pub(crate) hoist: bool,
}

#[derive(Clone)]
pub(crate) struct Comment {
    pub(crate) text: String,
    pub(crate) span: Span,
    /// The 0-based column of `/*` in the source (dart's comment re-indentation is bounded by
    /// it).
    pub(crate) column: usize,
}

#[derive(Clone)]
pub(crate) struct Decl {
    pub(crate) name: String,
    pub(crate) name_span: Span,
    pub(crate) value: String,
    pub(crate) value_span: Span,
    /// Whether this is a custom property (`--x: ...`) in the source. Decided purely from the
    /// source: in `#{$p}: $v`, `$v` is ordinary SassScript and the output still has `: `.
    pub(crate) custom: bool,
    /// Re-layout parameters for a custom property value containing newlines (dart's
    /// `_writeReindentedValue`); the indentation depends on the output level of the
    /// declaration, so only the number of columns to strip is recorded.
    pub(crate) custom_strip: Option<CustomStrip>,
}

/// How a custom property value is re-laid out (the three branches of dart's
/// `_writeReindentedValue`).
#[derive(Clone, Copy)]
pub(crate) enum CustomStrip {
    /// Has newlines but no indented lines: `trim_end` and append one space.
    Trailing,
    /// Strip `n` columns from each line (`min(minimum indentation, declaration name
    /// column)`).
    Indent(usize),
}

#[derive(Clone)]
pub(crate) enum Item {
    Rule(Rule),
    Keyframe(Keyframe),
    AtRule(AtRule),
    Comment(Comment),
    /// A fragment output verbatim (used during the less migration): text + internal
    /// `(byte offset, span)` anchors.
    Raw(Raw),
}

/// A verbatim fragment: arbitrary CSS text + internal `(byte offset, span)` anchors (`marks`
/// is sorted by offset ascending, and each piece runs from that offset to the next mark or
/// the end of the text).
///
/// Used only for the four kinds of text that "cannot be expressed with structured nodes"
/// (on the less side, see `less/emit.rs`):
///
/// 1. compact `(reference)` bodies (`a: 1;b: 2;`): less's `ruleset.genCSS` inserts no
///    newline for invisible declarations, whereas printing `Body::Decls` is always one per
///    line;
/// 2. the file contents of `@import (inline) "x"`: not parsed, written out verbatim;
/// 3. the sub-compilation output of `@import "x.less"`: a complete piece of CSS (with its
///    own newlines and indentation);
/// 4. the body of a bare root-level function call `e("…")`: the evaluation result is
///    itself a piece of CSS text.
///
/// Comments/declarations/rules always go through structured nodes (shape differences are
/// expressed by [`Style`]).
#[derive(Clone)]
pub(crate) struct Raw {
    pub(crate) text: String,
    pub(crate) marks: Vec<(usize, Span)>,
}

impl From<Raw> for Item {
    fn from(raw: Raw) -> Self {
        Item::Raw(raw)
    }
}

#[derive(Clone)]
pub(crate) enum Body {
    Rules(Vec<Item>),
    Decls(Vec<DeclItem>),
}

#[derive(Clone)]
pub(crate) enum DeclItem {
    Decl(Decl),
    Comment(Comment),
    /// A blockless at-rule inside a rule (dart keeps only `@import "x.css"`, with its
    /// indentation inside the braces).
    AtRule(AtRule),
}

impl Item {
    pub(crate) fn is_hoisted(&self) -> bool {
        matches!(self, Item::AtRule(at) if at.hoist)
    }

    /// Clear the hoist flag; no blank lines are left between the segments split out of one
    /// source rule.
    pub(crate) fn clear_hoist(&mut self) {
        if let Item::AtRule(at) = self {
            at.hoist = false;
            at.group_end = false;
        }
    }

    fn is_group_end(&self) -> bool {
        match self {
            Item::Rule(rule) => rule.group_end,
            Item::AtRule(at) => at.group_end,
            Item::Keyframe(_) | Item::Comment(_) | Item::Raw(_) => false,
        }
    }

    /// Scan for non-ASCII before serializing: decides whether to add `@charset "UTF-8";`.
    pub(crate) fn is_ascii(&self) -> bool {
        match self {
            Item::Rule(r) => {
                r.selector.iter().all(|(text, _)| text.is_ascii()) && r.body.is_ascii()
            }
            Item::Keyframe(k) => {
                k.selector.iter().all(|(text, _)| text.is_ascii()) && k.body.is_ascii()
            }
            Item::AtRule(a) => {
                a.name.is_ascii()
                    && a.prelude.is_ascii()
                    && a.body.as_ref().is_none_or(Body::is_ascii)
            }
            Item::Comment(c) => c.text.is_ascii(),
            Item::Raw(r) => r.text.is_ascii(),
        }
    }
}

impl Body {
    pub(crate) fn is_ascii(&self) -> bool {
        match self {
            Body::Rules(items) => items.iter().all(Item::is_ascii),
            Body::Decls(items) => items.iter().all(|item| match item {
                DeclItem::Decl(d) => d.name.is_ascii() && d.value.is_ascii(),
                DeclItem::Comment(c) => c.text.is_ascii(),
                DeclItem::AtRule(a) => a.name.is_ascii() && a.prelude.is_ascii(),
            }),
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        match self {
            Body::Rules(items) => items.is_empty(),
            Body::Decls(items) => items.is_empty(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Ctx {
    Rules,
    Keyframes,
}

/// Print the whole stylesheet. `style` decides blank lines / continuation-line indentation /
/// `@charset` / trailing newline / `sourcesContent` / whether comments are written verbatim.
///
/// With multiple files (`@import` inlining), all source files and the segment table are
/// handed to the printer: every token span is in merged-text coordinates, and the finishing
/// pass binary-searches back to "which file + offset within that file" before converting to
/// line/column.
pub(crate) fn print(
    items: &[Item],
    style: &Style,
    entry: &str,
    merged: &Merged,
    opts: &Options,
) -> (String, Option<String>) {
    let entry_text = merged.files.first().map_or("", |f| f.text.as_str());
    let mut printer = Printer::new(entry, entry_text, opts.source_map);
    for file in merged.files.iter().skip(1) {
        printer.add_file(&file.path.to_string_lossy(), &file.text);
    }
    // `sourcesContent`: the two sides have opposite emission conditions (see
    // `Style::sources_content`). What is written is `original`: less's `text` is the lexically
    // patched version (the span coordinate system), and the original is stored separately.
    if style.sources_content {
        for (i, file) in merged.files.iter().enumerate() {
            printer.set_content(i as u32, &file.original);
        }
    }
    if merged.files.len() > 1 {
        printer.set_segments(
            merged
                .segs
                .iter()
                .map(|s| SourceSegment {
                    start: s.start,
                    end: s.end,
                    file: s.file,
                    orig_start: s.orig,
                })
                .collect(),
        );
    }
    if style.charset && !items.iter().all(Item::is_ascii) {
        printer.raw("@charset \"UTF-8\";\n");
    }
    emit_items(items, style, Ctx::Rules, 0, &mut printer);
    if style.trailing_newline && !items.is_empty() {
        printer.newline();
    }
    printer.finish(opts)
}

fn emit_items(items: &[Item], style: &Style, ctx: Ctx, indent: usize, printer: &mut Printer) {
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            printer.newline();
            // The blank line belongs to "the end of the previous rule group (source rule + its
            // nested output)".
            if style.blank_line_after_rule && ctx == Ctx::Rules && items[i - 1].is_group_end() {
                printer.newline();
            }
        }
        emit_item(item, style, indent, printer);
    }
}

fn emit_item(item: &Item, style: &Style, indent: usize, printer: &mut Printer) {
    match item {
        Item::Rule(rule) => {
            emit_selector(&rule.selector, style, indent, printer);
            printer.raw(" {");
            emit_body(&rule.body, style, indent + 1, Ctx::Rules, printer);
            printer.raw("}");
        }
        Item::Keyframe(keyframe) => {
            emit_selector(&keyframe.selector, style, indent, printer);
            printer.raw(" {");
            emit_body(&keyframe.body, style, indent + 1, Ctx::Keyframes, printer);
            printer.raw("}");
        }
        Item::AtRule(at) => emit_at_rule(at, style, indent, printer),
        Item::Comment(comment) => emit_comment(comment, style, indent, printer),
        Item::Raw(raw) => emit_raw(raw, indent, printer),
    }
}

/// Selector output: the first line is indented; continuation lines start at column 0 by
/// default (measured with dart: multi-line selectors inside `@media` are not indented
/// either), and are indented to the current level when `indent_selector_continuation` is on.
/// Each line gets its own `token`, so continuation lines have segments.
fn emit_selector(selector: &SelectorLines, style: &Style, indent: usize, printer: &mut Printer) {
    for (i, (text, span)) in selector.iter().enumerate() {
        if i == 0 {
            printer.indent(indent);
        } else {
            printer.newline();
            if style.indent_selector_continuation {
                printer.indent(indent);
            }
        }
        printer.token(text, *span);
    }
}

/// A verbatim fragment: the whole piece is written once at the current indentation, and
/// `marks` splits it by byte offset with a mapping for each part.
fn emit_raw(raw: &Raw, indent: usize, printer: &mut Printer) {
    printer.indent(indent);
    let text = raw.text.as_str();
    let mut cursor = 0;
    for (i, &(offset, span)) in raw.marks.iter().enumerate() {
        if offset > cursor {
            printer.raw(&text[cursor..offset]);
        }
        let end = raw.marks.get(i + 1).map_or(text.len(), |&(next, _)| next);
        printer.token(&text[offset..end], span);
        cursor = end;
    }
    if cursor < text.len() {
        printer.raw(&text[cursor..]);
    }
}

/// Comment serialization. The dart side uses the same re-layout as custom property values
/// (dart's `_writeWithIndent`): continuation lines first have
/// `min(minimum indentation, comment start column)` stripped, then get the current
/// indentation, and empty lines stay truly empty; the less side
/// ([`Style::verbatim_comments`]) writes the source bytes straight back out.
///
/// dart attaches a segment to every line of a comment (all pointing at the `/*` start), so
/// each line is `token`ed here; in the old implementation less comments were unmapped raw
/// text, so the verbatim branch emits the whole thing with `raw` (keeping the map bytes).
fn emit_comment(comment: &Comment, style: &Style, indent: usize, printer: &mut Printer) {
    let text = format!("/*{}*/", comment.text);
    printer.indent(indent);
    if style.verbatim_comments {
        printer.raw(&text);
        return;
    }
    match minimum_indentation(&text) {
        None => printer.token(&text, comment.span),
        Some(min) => {
            let strip = min.min(comment.column);
            printer.token(
                text.split_once('\n').map_or(&text, |(first, _)| first),
                comment.span,
            );
            // `write_with_indent` starts with a `\n`: continuation lines are split out one by
            // one, and empty lines keep only the newline.
            let rest = write_with_indent(&text, strip, indent);
            for line in rest.split('\n').skip(1) {
                printer.newline();
                if !line.is_empty() {
                    printer.token(line, comment.span);
                }
            }
        }
    }
}

/// The indentation of the "shallowest non-empty line" after a newline (columns counted in
/// characters). Returns `None` when there is no newline.
fn minimum_indentation(text: &str) -> Option<usize> {
    let mut lines = text.split('\n');
    lines.next()?;
    lines
        .filter_map(|line| {
            let lead = line.len() - line.trim_start_matches([' ', '\t']).len();
            (!line.trim().is_empty()).then_some(lead)
        })
        .min()
}

/// dart's `_writeWithIndent`: everything after the first line of [text] is stripped line by
/// line by [strip] and re-indented to level [indent] (the caller writes the first line).
/// Empty lines emit only the newline (no whitespace); text ending in whitespace is compressed
/// into a single space.
fn write_with_indent(text: &str, strip: usize, indent: usize) -> String {
    let pad = "  ".repeat(indent);
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len() + pad.len());
    let mut i = 0;
    // Skip to the end of the first line.
    while i < chars.len() && chars[i] != '\n' {
        i += 1;
    }
    while i < chars.len() {
        let mut newlines = 0usize;
        let mut line_start = i;
        loop {
            if i >= chars.len() {
                // Only whitespace remains at the end: dart replaces it with a single space
                // (custom properties rely on it to keep the separation).
                out.push(' ');
                return out;
            }
            let ch = chars[i];
            i += 1;
            match ch {
                ' ' | '\t' => continue,
                '\n' => {
                    newlines += 1;
                    line_start = i;
                }
                _ => {
                    i -= 1;
                    break;
                }
            }
        }
        for _ in 0..newlines {
            out.push('\n');
        }
        out.push_str(&pad);
        let skip = (line_start + strip).min(i);
        out.extend(chars[skip..].iter().take_while(|&&ch| ch != '\n'));
        while i < chars.len() && chars[i] != '\n' {
            i += 1;
        }
    }
    out
}

fn emit_at_rule(at: &AtRule, style: &Style, indent: usize, printer: &mut Printer) {
    printer.indent(indent);
    // dart maps the whole at-rule to the column of `@` (`name_span` points at the name
    // itself).
    printer.token(
        &format!("@{}", at.name),
        Span {
            start: at.name_span.start.saturating_sub(1),
            end: at.name_span.end,
        },
    );
    if !at.prelude.is_empty() {
        printer.raw(" ");
        printer.raw(&at.prelude);
    }
    match &at.body {
        None => printer.raw(";"),
        Some(body) if body.is_empty() => printer.raw(" {}"),
        Some(body) => {
            let ctx = if at.name.to_ascii_lowercase().ends_with("keyframes") {
                Ctx::Keyframes
            } else {
                Ctx::Rules
            };
            printer.raw(" {");
            emit_body(body, style, indent + 1, ctx, printer);
            printer.raw("}");
        }
    }
}

fn emit_body(body: &Body, style: &Style, indent: usize, ctx: Ctx, printer: &mut Printer) {
    if body.is_empty() {
        return;
    }
    printer.newline();
    match body {
        Body::Rules(items) => emit_items(items, style, ctx, indent, printer),
        Body::Decls(items) => {
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    printer.newline();
                }
                match item {
                    DeclItem::Decl(decl) => {
                        printer.indent(indent);
                        printer.token(&decl.name, decl.name_span);
                        if decl.custom {
                            // Custom properties keep their original text (whitespace already
                            // collapsed); no extra space after the colon.
                            printer.raw(":");
                            let tail = match decl.custom_strip {
                                None => std::borrow::Cow::Borrowed(decl.value.as_str()),
                                Some(CustomStrip::Trailing) => {
                                    std::borrow::Cow::Owned(format!("{} ", decl.value.trim_end()))
                                }
                                Some(CustomStrip::Indent(strip)) => {
                                    let first = decl
                                        .value
                                        .split_once('\n')
                                        .map_or(decl.value.as_str(), |(first, _)| first);
                                    printer.raw(first);
                                    std::borrow::Cow::Owned(write_with_indent(
                                        &decl.value,
                                        strip,
                                        indent,
                                    ))
                                }
                            };
                            printer.raw(&tail);
                        } else {
                            printer.raw(": ");
                            printer.token(&decl.value, decl.value_span);
                        }
                        printer.raw(";");
                    }
                    DeclItem::Comment(comment) => emit_comment(comment, style, indent, printer),
                    DeclItem::AtRule(at) => emit_at_rule(at, style, indent, printer),
                }
            }
        }
    }
    printer.newline();
    printer.indent(indent.saturating_sub(1));
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn render(src: &str) -> String {
        crate::scss::compile(std::path::Path::new("t.scss"), src, &Options::default())
            .unwrap()
            .css
    }

    /// Escape hatch: a whole `Raw` piece is printed verbatim (including non-ASCII), and
    /// `marks` attach a segment at the corresponding offsets.
    #[test]
    fn raw_is_printed_and_mapped() {
        let source = "abc€ raw";
        let mut merged = Merged::new();
        merged.add_file(Path::new("t.scss"), source.to_string(), source.to_string());
        let items = vec![Item::from(Raw {
            text: "abc€\n.x".to_string(),
            marks: vec![
                (0, Span { start: 0, end: 0 }),
                (7, Span { start: 3, end: 5 }),
            ],
        })];
        assert!(!items.iter().all(Item::is_ascii), "expected non-ASCII");

        let opts = Options {
            source_map: true,
            ..Default::default()
        };
        let (css, map) = print(&items, &Style::SASS, "t.scss", &merged, &opts);
        assert_eq!(css, "@charset \"UTF-8\";\nabc€\n.x");
        let map: serde_json::Value = serde_json::from_str(&map.expect("map")).unwrap();
        // One mapping per piece (the charset line has no mapping).
        let mapped_lines = map["mappings"]
            .as_str()
            .unwrap()
            .split(';')
            .filter(|line| !line.is_empty())
            .count();
        assert_eq!(mapped_lines, 2);
    }

    #[test]
    fn blank_line_after_style_rule() {
        assert_eq!(
            render(".a{color:red}.b{color:blue}"),
            ".a {\n  color: red;\n}\n\n.b {\n  color: blue;\n}"
        );
        assert_eq!(
            render(".a{color:red}@media screen{.b{color:blue}}"),
            ".a {\n  color: red;\n}\n\n@media screen {\n  .b {\n    color: blue;\n  }\n}"
        );
        // at-rule → rule: no blank line
        assert_eq!(
            render("@media screen{.a{color:red}}.b{color:blue}"),
            "@media screen {\n  .a {\n    color: red;\n  }\n}\n.b {\n  color: blue;\n}"
        );
        // blank line between adjacent rules inside @media
        assert_eq!(
            render("@media screen{.a{color:red}.b{color:blue}}"),
            "@media screen {\n  .a {\n    color: red;\n  }\n\n  .b {\n    color: blue;\n  }\n}"
        );
    }

    /// D1: comments inside a declaration (between property name and value / after the value
    /// up to `;` or `}`) are dropped entirely; only statement-level comments are kept.
    #[test]
    fn comments_inside_declarations_are_dropped() {
        assert_eq!(render(".a{width:1px /* t */ }"), ".a {\n  width: 1px;\n}");
        assert_eq!(
            render(".a{width:1px /* t */; color:red}"),
            ".a {\n  width: 1px;\n  color: red;\n}"
        );
        assert_eq!(render(".a{width: /* t */ 1px }"), ".a {\n  width: 1px;\n}");
        assert_eq!(
            render(".a{width:1px /* t */ !important}"),
            ".a {\n  width: 1px !important;\n}"
        );
        // Statement-level ones are kept (after `;`, at block start, at block end)
        assert_eq!(
            render(".a{color:red; /* t */}"),
            ".a {\n  color: red;\n  /* t */\n}"
        );
        assert_eq!(
            render(".a{ /* c1 */ color:red; /* c2 */ }"),
            ".a {\n  /* c1 */\n  color: red;\n  /* c2 */\n}"
        );
    }

    /// D2: only `nth-child` / `nth-last-child` normalize An+B; `nth-of-type` is left alone.
    #[test]
    fn anb_selector_normalization() {
        assert_eq!(
            render(":nth-child(2n + 1){x:1}"),
            ":nth-child(2n+1) {\n  x: 1;\n}"
        );
        assert_eq!(
            render(":nth-child( -n + 3 ){x:1}"),
            ":nth-child(-n+3) {\n  x: 1;\n}"
        );
        assert_eq!(
            render(":nth-child(3n- 1){x:1}"),
            ":nth-child(3n-1) {\n  x: 1;\n}"
        );
        assert_eq!(
            render(":nth-child(odd){x:1}"),
            ":nth-child(odd) {\n  x: 1;\n}"
        );
        assert_eq!(
            render(":nth-child(2n + 1 of .a, .b){x:1}"),
            ":nth-child(2n+1 of .a, .b) {\n  x: 1;\n}"
        );
        // nth-of-type / nth-last-of-type are not normalized in practice (whitespace is
        // collapsed as for a regular selector)
        assert_eq!(
            render(":nth-of-type( -n + 3 ){x:1}"),
            ":nth-of-type(-n + 3) {\n  x: 1;\n}"
        );
        assert_eq!(
            render(":nth-of-type(2N+1){x:1}"),
            ":nth-of-type(2N+1) {\n  x: 1;\n}"
        );
        // Non-An+B pseudo-class arguments are unaffected
        assert_eq!(render(":not(.a + .b){x:1}"), ":not(.a + .b) {\n  x: 1;\n}");
    }

    /// D3: root-level `@import`s are hoisted to after the leading comment block; the relative
    /// order of all other nodes is unchanged.
    #[test]
    fn root_imports_are_hoisted() {
        assert_eq!(
            render("@namespace svg url(http://x);@import url(x.css);.a{v:1}"),
            "@import url(x.css);\n@namespace svg url(http://x);\n.a {\n  v: 1;\n}"
        );
        assert_eq!(
            render("@layer a;@import url(x.css);.a{v:1}"),
            "@import url(x.css);\n@layer a;\n.a {\n  v: 1;\n}"
        );
        assert_eq!(
            render("@page{margin:1cm}@import url(x.css);@namespace svg url(http://x);"),
            "@import url(x.css);\n@page {\n  margin: 1cm;\n}\n@namespace svg url(http://x);"
        );
        // The leading comment is not crossed; the import is inserted after it
        assert_eq!(
            render("/* lead */@import url(a.css);.a{v:1}"),
            "/* lead */\n@import url(a.css);\n.a {\n  v: 1;\n}"
        );
        // A trailing import is hoisted, but comments keep their relative order with the
        // original statements
        assert_eq!(
            render(".a{v:1}/* c */@import url(x.css);"),
            "@import url(x.css);\n.a {\n  v: 1;\n}\n\n/* c */"
        );
        // An import nested inside @media is not hoisted
        assert_eq!(
            render("@media screen{@import url(a.css);}.a{v:1}"),
            "@media screen {\n  @import url(a.css);\n}\n.a {\n  v: 1;\n}"
        );
    }

    #[test]
    fn comment_rules() {
        assert_eq!(
            render("/*! keep */\n.a{color:red}"),
            "/*! keep */\n.a {\n  color: red;\n}"
        );
        assert_eq!(render("// drop\n.a{color:red}"), ".a {\n  color: red;\n}");
        assert_eq!(
            render(".a{color:red}/* c */.b{color:blue}"),
            ".a {\n  color: red;\n}\n\n/* c */\n.b {\n  color: blue;\n}"
        );
        // An empty rule with a comment is kept
        assert_eq!(render(".a{/* c */}"), ".a {\n  /* c */\n}");
    }

    #[test]
    fn empty_rules_removed() {
        assert_eq!(render(".a{}\n.b{color:red}"), ".b {\n  color: red;\n}");
        assert_eq!(render("@media screen{}"), "");
        // Empty blocks of non-conditional at-rules are kept
        assert_eq!(render("@font-face{}"), "@font-face {}");
        assert_eq!(render("@foo bar{}"), "@foo bar {}");
    }

    #[test]
    fn charset_and_no_trailing_newline() {
        assert_eq!(render(".a{color:red}"), ".a {\n  color: red;\n}");
        assert_eq!(
            render(".a{content:\"か\"}"),
            "@charset \"UTF-8\";\n.a {\n  content: \"か\";\n}"
        );
        assert!(!render(".a{color:red}").ends_with('\n'));
    }

    #[test]
    fn declarations_and_custom_properties() {
        assert_eq!(
            render(".a{color:red!important;--x: 1px  2px}"),
            ".a {\n  color: red !important;\n  --x: 1px 2px;\n}"
        );
        assert_eq!(
            render(".a{background:url(\"a b.png\") no-repeat}"),
            ".a {\n  background: url(\"a b.png\") no-repeat;\n}"
        );
    }

    #[test]
    fn attribute_selector_unquotes_identifiers() {
        assert_eq!(
            render("input[type=\"button\"], [class*=\"col-\"], [href^=\"javascript:\"]{color:red}"),
            "input[type=button], [class*=col-], [href^=\"javascript:\"] {\n  color: red;\n}"
        );
        assert_eq!(
            render("[type = \"button\"], [type=\"--x\"], [type=\"1\"]{color:red}"),
            "[type=button], [type=\"--x\"], [type=\"1\"] {\n  color: red;\n}"
        );
    }

    #[test]
    fn custom_property_whitespace() {
        assert_eq!(
            render(".a{--x: 1px  2px;--y:a,b }"),
            ".a {\n  --x: 1px 2px;\n  --y:a,b ;\n}"
        );
        assert_eq!(
            render(".a{--x: a !important}"),
            ".a {\n  --x: a !important;\n}"
        );
        // Continuation lines are reindented by "source column - declaration column + output
        // indent" (measured against dart-sass)
        assert_eq!(render(".a{--x: a\n    b}"), ".a {\n  --x: a\n   b;\n}");
    }

    #[test]
    fn keyframes_have_no_blank_lines_between_blocks() {
        assert_eq!(
            render("@keyframes x{from{opacity:0}to{opacity:1}}"),
            "@keyframes x {\n  from {\n    opacity: 0;\n  }\n  to {\n    opacity: 1;\n  }\n}"
        );
    }

    #[test]
    fn empty_at_rule_blocks() {
        // Empty blocks of media / supports are deleted entirely; layer / font-face etc. are
        // kept
        assert_eq!(render("@media screen{}"), "");
        assert_eq!(render("@supports (a:b){}"), "");
        assert_eq!(render("@layer base{}"), "@layer base {}");
    }

    #[test]
    fn raw_function_arguments() {
        assert_eq!(
            render(
                ".a{filter:alpha(opacity = 65);x:progid:DXImageTransform.Microsoft.gradient(enabled = false)}"
            ),
            ".a {\n  filter: alpha(opacity=65);\n  x: progid:DXImageTransform.Microsoft.gradient(enabled = false);\n}"
        );
    }

    #[test]
    fn source_map_records_declarations() {
        let opts = Options {
            source_map: true,
            ..Default::default()
        };
        let out =
            crate::scss::compile(std::path::Path::new("t.scss"), ".a{color:red}", &opts).unwrap();
        let map: serde_json::Value = serde_json::from_str(&out.map.unwrap()).unwrap();
        assert_eq!(map["version"], 3);
        assert_eq!(map["sources"][0], "t.scss");
        assert!(!map["mappings"].as_str().unwrap().is_empty());
    }

    #[test]
    fn selector_normalization() {
        assert_eq!(
            render(".a>.b,.c   .d{color:red}"),
            ".a > .b, .c .d {\n  color: red;\n}"
        );
        assert_eq!(render(".a,\n.b{color:red}"), ".a,\n.b {\n  color: red;\n}");
        assert_eq!(
            render(":not( .a , .b ){color:red}"),
            ":not(.a, .b) {\n  color: red;\n}"
        );
    }
}
