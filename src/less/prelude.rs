use oxc_css_parser::ast::*;
use oxc_css_parser::pos::Span;

use super::Compiler;
use super::comments::block_comments;
use super::emit::attach_to_feature_values;
use super::imports::{is_css_path, split_import_options, split_quoted_path};
use super::value;
use crate::doc::Comment;

/// at-rule name that a `(reference)` import is wrapped in within the text: definitions inside the block are available but not emitted.
pub(super) const REFERENCE_AT: &str = "-style-ref";

/// less units are `%|[a-z_]+`, so in less `6px-1px` is `6px` minus `1px`;
/// while CSS tokenization treats `px-1px` as one unit. Here a space is inserted
/// between a numeric+alphabetic unit and a following `-number` so the resulting AST matches less (strings and comments are untouched).
///
/// Returns a copy of the original string when there is nothing to patch (callers still parse/print the new string, with consistent offsets).
pub(super) fn split_signed_dimensions(src: &str) -> String {
    let bytes = src.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(src.len());
    let mut i = 0usize;
    let mut quote = 0u8;
    while i < bytes.len() {
        let c = bytes[i];
        if quote != 0 {
            out.push(c);
            if c == b'\\' {
                if let Some(&next) = bytes.get(i + 1) {
                    out.push(next);
                    i += 2;
                    continue;
                }
            } else if c == quote {
                quote = 0;
            }
            i += 1;
            continue;
        }
        if c == b'/' && matches!(bytes.get(i + 1), Some(b'/') | Some(b'*')) {
            let line = bytes[i + 1] == b'/';
            out.extend_from_slice(&bytes[i..i + 2]);
            i += 2;
            while i < bytes.len() {
                if line {
                    if bytes[i] == b'\n' {
                        break;
                    }
                } else if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
                    out.extend_from_slice(b"*/");
                    i += 2;
                    break;
                }
                out.push(bytes[i]);
                i += 1;
            }
            continue;
        }
        if c == b'"' || c == b'\'' {
            quote = c;
            out.push(c);
            i += 1;
            continue;
        }
        if c.is_ascii_digit() || (c == b'.' && bytes.get(i + 1).is_some_and(u8::is_ascii_digit)) {
            // Inside an identifier (`.bug-100cm-1m`, `a100px-1px`) this is not a numeric context
            if i > 0
                && matches!(bytes[i - 1], b if b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'#' | b'\\') || b >= 0x80)
            {
                out.push(c);
                i += 1;
                continue;
            }
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            if bytes.get(i) == Some(&b'.') {
                i += 1;
                while i < bytes.len() && bytes[i].is_ascii_digit() {
                    i += 1;
                }
            }
            let unit_start = i;
            while i < bytes.len() && (bytes[i].is_ascii_alphabetic() || bytes[i] == b'_') {
                i += 1;
            }
            let unit = &src[unit_start..i];
            // The exponent of `1e-3` is not a unit
            let signed = !unit.is_empty()
                && !unit.eq_ignore_ascii_case("e")
                && bytes.get(i) == Some(&b'-')
                && bytes.get(i + 1).is_some_and(|b| {
                    b.is_ascii_digit()
                        || (*b == b'.' && bytes.get(i + 2).is_some_and(u8::is_ascii_digit))
                });
            out.extend_from_slice(&bytes[start..i]);
            if signed {
                // Spaces on both sides are required: `6px -1px` makes the parser treat `-1px` as a missing operator
                out.extend_from_slice(b" - ");
                i += 1;
            }
            continue;
        }
        out.push(c);
        i += 1;
    }
    String::from_utf8(out).expect("utf8")
}

/// The source contains an `@plugin` directive (at any nesting level): plugins can inject variables during evaluation, so strict errors must be disabled.
pub(super) fn has_plugin(stmts: &[Statement]) -> bool {
    stmts.iter().any(|st| match st {
        Statement::AtRule(a) => {
            a.name.name.eq_ignore_ascii_case("plugin")
                || a.block.as_ref().is_some_and(|b| has_plugin(&b.statements))
        }
        Statement::LessMixinDefinition(d) => has_plugin(&d.block.statements),
        Statement::QualifiedRule(r) => has_plugin(&r.block.statements),
        Statement::LessConditionalQualifiedRule(r) => has_plugin(&r.block.statements),
        Statement::LessVariableDeclaration(d) => match &d.value {
            ComponentValue::LessDetachedRuleset(dr) => has_plugin(&dr.block.statements),
            _ => false,
        },
        _ => false,
    })
}

pub(super) fn is_reference(at: &AtRule) -> bool {
    at.name.name == REFERENCE_AT
}

pub(super) fn is_charset(at: &AtRule) -> bool {
    at.name.name.eq_ignore_ascii_case("charset")
}

/// Remote URL (`https://` / `//host/…`): there is no downloader, so it is kept as a CSS import verbatim.
pub(super) fn is_remote(href: &str) -> bool {
    href.contains("://") || href.starts_with("//")
}

/// Only "CSS imports kept verbatim" are hoisted before other nodes by less (`importNodeIndex`);
/// inlined `.less` / `(inline)` imports stay in source order.
pub(super) fn is_hoisted_import(text: &str) -> bool {
    let (options, rest) = split_import_options(text);
    if options.iter().any(|o| o == "inline" || o == "less") {
        return false;
    }
    split_quoted_path(&rest)
        .is_some_and(|(href, _)| is_remote(&href) || is_css_path(&href) || href.ends_with(".wxss"))
}

pub(super) fn is_import(at: &AtRule) -> bool {
    at.name.name.eq_ignore_ascii_case("import")
}

/// at-rules whose body inherits the parent selectors (the ones with `isRooted = false` in less).
pub(super) fn at_rule_keeps_parents(name: &str) -> bool {
    let bare = name.trim_start_matches('@');
    let bare = match bare.strip_prefix('-') {
        Some(rest) => rest.split_once('-').map(|(_, n)| n).unwrap_or(bare),
        None => bare,
    };
    matches!(
        bare.to_ascii_lowercase().as_str(),
        "media" | "document" | "supports" | "layer"
    )
}

/// at-rule prelude normalization: `prop:value` inside parentheses → `prop: value` (the less output form).
pub(super) fn normalize_prelude(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() + 4);
    let chars: Vec<char> = raw.chars().collect();
    for (i, &ch) in chars.iter().enumerate() {
        out.push(ch);
        if ch == ':' {
            let prev = i.checked_sub(1).and_then(|j| chars.get(j));
            let next = chars.get(i + 1);
            if prev != Some(&':') && next != Some(&' ') && next.is_some_and(|c| *c != ':') {
                out.push(' ');
            }
        }
    }
    out
}

/// `~"..."` → inner text: escape strings in a media prelude are emitted as plain text.
pub(super) fn strip_escapes(raw: &str) -> String {
    if !raw.contains('~') {
        return raw.to_string();
    }
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.char_indices().peekable();
    while let Some((i, ch)) = chars.next() {
        if ch != '~' {
            out.push(ch);
            continue;
        }
        let Some(quote @ ('"' | '\'')) = chars.peek().map(|(_, c)| *c) else {
            out.push('~');
            continue;
        };
        chars.next();
        let start = i + 2;
        let mut end = None;
        for (j, c) in chars.by_ref() {
            if c == quote {
                end = Some(j);
                break;
            }
        }
        match end {
            Some(j) => out.push_str(&raw[start..j]),
            None => out.push_str(&raw[i..]),
        }
        if end.is_none() {
            return out;
        }
    }
    out
}

impl<'a> Compiler<'a> {
    /// Condition text of @media (evaluated + less feature normalization).
    ///
    /// Returns `(text, comments to move into the body)`. less clears the commentStore after
    /// reading the at-rule name (comments before the name are dropped); later comments in a list-like prelude
    /// such as @media/@keyframes become the first nodes of the body's rule list; prelude comments of other at-rules stay in the text.
    pub(super) fn at_prelude(&mut self, at: &AtRule) -> (String, Vec<Comment>) {
        let start = at.name.span().end;
        let end = match &at.block {
            Some(block) => block.span.start,
            None => at.prelude.as_ref().map_or(start, |p| p.span().end),
        };
        if end <= start || end > self.src.len() {
            return (String::new(), Vec::new());
        }
        let span = Span { start, end };
        let full = self.slice(span);
        // `raw` is trimmed: indices of comments in `raw` must be offset by the trimmed leading whitespace to get absolute positions.
        let base = span.start + full.len() - full.trim_start().len();
        let raw = full.trim().to_string();
        let name = at.name.name.to_ascii_lowercase();
        let bare = name.trim_start_matches('@');
        let bare = match bare.strip_prefix('-') {
            Some(rest) => rest.split_once('-').map(|(_, n)| n).unwrap_or(bare),
            None => bare,
        };
        let listy = matches!(bare, "media" | "keyframes" | "counter-style");
        let is_media = bare == "media";
        let comments = block_comments(&raw);
        if comments.is_empty() {
            return (self.finish_prelude(at, raw, span), Vec::new());
        }
        let mut moved = Vec::new();
        let mut plain = String::with_capacity(raw.len());
        let mut cursor = 0;
        let mut content_seen = false;
        // less attaches prelude comments to the "next value": in an @media feature, after `: `;
        // when no value is found, they land at the head of the enclosing block.
        let mut pending: Vec<(usize, Comment)> = Vec::new();
        for &(start, end) in &comments {
            let piece = &raw[cursor..start];
            if !piece.trim().is_empty() {
                content_seen = true;
            }
            let text = &raw[start..end];
            let abs = base + start;
            self.mark_comments(Span {
                start: abs,
                end: abs + text.len(),
            });
            plain.push_str(piece);
            cursor = end;
            // Comments after the name and before the first content are cleared by less
            if !content_seen {
                continue;
            }
            let comment = self.comment(abs, abs + text.len());
            match (is_media, listy) {
                (true, _) => {
                    // The comment holds a space so the two tokens do not merge into `and(max-width` when it is removed
                    plain.push(' ');
                    pending.push((plain.len(), comment));
                }
                (false, true) => moved.push(comment),
                (false, false) => plain.push_str(text),
            }
        }
        let rest = &raw[cursor.min(raw.len())..];
        plain.push_str(rest);
        let plain = if is_media {
            let (text, leftover) = attach_to_feature_values(&plain, &pending);
            moved.extend(leftover);
            text.split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .replace(" ,", ",")
        } else if listy {
            plain
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .replace(" ,", ",")
        } else {
            plain.trim().to_string()
        };
        (self.finish_prelude(at, plain, span), moved)
    }

    pub(super) fn finish_prelude(&self, at: &AtRule, raw: String, span: Span) -> String {
        // `@var` / `@{var}` in a prelude are evaluated in the current scope (less evaluates at the prelude level)
        let raw = value::substitute(&raw, self.src, &self.env, 0, Some(span));
        // Only @media features are normalized by less (`(min-width:100px)` → a space is added);
        // the @supports prelude is passed through verbatim by less.
        if at.name.name.eq_ignore_ascii_case("media") {
            normalize_prelude(&strip_escapes(&raw))
        } else {
            raw
        }
    }
}
