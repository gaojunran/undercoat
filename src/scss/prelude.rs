//! at-rule preludes: interpolation/variable substitution, media merging,
//! `@supports` condition structuring and printing.
//!
//! Split out of `scss/mod.rs` (pure code move).

use super::*;

impl<'a> Converter<'a> {
    /// Values inside a `@media` prelude: the value of a media feature `name: value` is
    /// evaluated as SassScript (dart's `MediaQuery.value` is an expression), and only
    /// when that fails do we fall back to substituting a bare `$var`.
    pub(super) fn substitute_vars(
        &mut self,
        text: &str,
        base: usize,
    ) -> Result<String, Box<CompileError>> {
        let bytes = text.as_bytes();
        let mut out = String::with_capacity(text.len());
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'\'' | b'"' => {
                    let end = crate::text::skip_string(text, i);
                    out.push_str(&text[i..end]);
                    i = end;
                }
                b'(' => {
                    let close = paren_end(text, i).unwrap_or(bytes.len());
                    out.push('(');
                    out.push_str(&self.media_feature(&text[i + 1..close], base + i + 1)?);
                    out.push(')');
                    i = close + 1;
                }
                _ => {
                    let ch = text[i..].chars().next().expect("non-empty");
                    out.push(ch);
                    i += ch.len_utf8();
                }
            }
        }
        Ok(out)
    }

    /// One media feature (`min-width: $bp - 1px`): when the value contains `$`, the whole
    /// value is evaluated as SassScript.
    fn media_feature(&mut self, inner: &str, base: usize) -> Result<String, Box<CompileError>> {
        let Some(colon) = top_level_colon(inner) else {
            return self.substitute_vars_fallback(inner, base);
        };
        let (name, rest) = inner.split_at(colon);
        let value = &rest[1..];
        if value.contains('$') {
            if let Some(text) = self.media_value(value, base + colon + 1) {
                return Ok(format!("{name}:{text}"));
            }
        }
        Ok(format!(
            "{name}:{}",
            self.substitute_vars_fallback(value, base + colon + 1)?
        ))
    }

    /// Evaluate the value of a media feature: only expressions that parse completely as
    /// SassScript are accepted.
    fn media_value(&mut self, value: &str, at: usize) -> Option<String> {
        let trimmed = value.trim();
        let start = at + leading_ws(value);
        let evaluated = self.eval.eval_text(trimmed, start).ok()?;
        evaluated.to_css().ok()
    }

    /// Substitute a bare `$var` in prelude text (the fallback when `media_feature`
    /// evaluation fails).
    fn substitute_vars_fallback(
        &mut self,
        text: &str,
        base: usize,
    ) -> Result<String, Box<CompileError>> {
        let mut out = String::with_capacity(text.len());
        let mut chars = text.char_indices().peekable();
        while let Some((i, ch)) = chars.next() {
            if ch == '\'' || ch == '"' {
                let start = i;
                let mut end = i + ch.len_utf8();
                for (j, c) in chars.by_ref() {
                    end = j + c.len_utf8();
                    if c == ch {
                        break;
                    }
                }
                out.push_str(&text[start..end]);
                continue;
            }
            if ch != '$' {
                out.push(ch);
                continue;
            }
            let mut end = i + 1;
            while let Some(&(j, c)) = chars.peek() {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    end = j + c.len_utf8();
                    chars.next();
                } else {
                    break;
                }
            }
            let name = &text[i + 1..end];
            let Some(value) = self.eval.lookup(name) else {
                return Err(self.err(base + i, "Undefined variable."));
            };
            let rendered = value
                .interpolation()
                .map_err(|message| self.err(base + i, message))?;
            out.push_str(&rendered);
        }
        Ok(out)
    }

    /// Raw prelude from the end of the statement head to the start of the block (or to
    /// the end of the statement when there is no block).
    pub(super) fn prelude_raw(&self, at: &oxc_css_parser::ast::AtRule<'a>) -> String {
        let src = self.src();
        let end = match &at.block {
            Some(block) => block.span.start,
            None => at.span.end,
        };
        src.get(at.name.span.end..end)
            .unwrap_or("")
            .trim_end()
            .trim_end_matches(';')
            .to_string()
    }
}

/// `#{` at `start` of `text`; returns the byte index of the matching `}`.
pub(super) fn matching_brace(text: &str, start: usize) -> usize {
    crate::text::matching(text, start + 1).map_or_else(|| text.len() - 1, |end| end - 1)
}

/// `#{` at `start` of `chars`; returns the index of the matching `}`.
pub(super) fn interpolation_end(chars: &[(usize, char)], start: usize) -> usize {
    let mut depth = 0usize;
    let mut quote: Option<char> = None;
    let mut i = start + 1;
    while i < chars.len() {
        let ch = chars[i].1;
        if let Some(q) = quote {
            if ch == '\\' {
                i += 2;
                continue;
            }
            if ch == q {
                quote = None;
            }
        } else {
            match ch {
                '\'' | '"' => quote = Some(ch),
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return i;
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }
    chars.len() - 1
}

pub(super) fn wrap_media(rule: &OutAtRule, items: Vec<Item>) -> Item {
    Item::AtRule(OutAtRule {
        name: rule.name.clone(),
        name_span: rule.name_span,
        prelude: rule.prelude.clone(),
        body: Some(Body::Rules(items)),
        group_end: false,
        hoist: true,
    })
}

/// Merge the preludes of nested `@media` (dart `_mergeMediaQueries`):
/// take the cross product of the queries; if the media types are equal the pair is
/// kept, if they differ the combination is dropped; queries with a `not` / `only`
/// modifier do not participate in merging (dart keeps them nested). `None` means the
/// whole thing cannot be merged.
pub(super) fn merge_media(outer: &str, inner: &str) -> Option<String> {
    if has_media_modifier(outer) || has_media_modifier(inner) {
        return None;
    }
    let mut merged = Vec::new();
    for a in split_top_level_commas(outer) {
        for b in split_top_level_commas(inner) {
            if let Some(query) = merge_media_query(a.trim(), b.trim()) {
                merged.push(query);
            }
        }
    }
    Some(merged.join(", "))
}

fn has_media_modifier(prelude: &str) -> bool {
    split_top_level_commas(prelude).iter().any(|query| {
        let first = query.split_whitespace().next().unwrap_or("");
        first.eq_ignore_ascii_case("not") || first.eq_ignore_ascii_case("only")
    })
}

/// Merge a single query pair: `(a) and (b)`, taking the media type when both agree.
fn merge_media_query(outer: &str, inner: &str) -> Option<String> {
    let (outer_type, mut conditions) = media_query_parts(outer);
    let (inner_type, inner_conditions) = media_query_parts(inner);
    let media_type = match (outer_type, inner_type) {
        (Some(a), Some(b)) if a == b => Some(a),
        (Some(_), Some(_)) => return None,
        (a, b) => a.or(b),
    };
    conditions.extend(inner_conditions);
    let mut out = media_type.unwrap_or_default();
    for condition in conditions {
        if !out.is_empty() {
            out.push_str(" and ");
        }
        out.push_str(&condition);
    }
    Some(out)
}

/// Split one media query into a "type" and a list of "conditions" (separated by a
/// top-level `and`).
fn media_query_parts(query: &str) -> (Option<String>, Vec<String>) {
    let mut parts = Vec::new();
    let mut start = 0usize;
    let mut depth = 0usize;
    let bytes = query.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'(' => depth += 1,
            b')' => depth = depth.saturating_sub(1),
            b'a' if depth == 0 && is_and_at(query, i) => {
                parts.push(query[start..i].trim().to_string());
                i += 3;
                start = i;
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    parts.push(query[start..].trim().to_string());
    let first = parts.remove(0);
    if first.starts_with('(') {
        let mut conditions = vec![first];
        conditions.extend(parts);
        (None, conditions)
    } else {
        (Some(first), parts)
    }
}

fn is_and_at(text: &str, i: usize) -> bool {
    let bytes = text.as_bytes();
    bytes
        .get(i..i + 3)
        .is_some_and(|w| w.eq_ignore_ascii_case(b"and"))
        && (i == 0 || bytes[i - 1].is_ascii_whitespace())
        && bytes.get(i + 3).is_some_and(u8::is_ascii_whitespace)
}

/// Re-serialize `url(...)` in `@import` following dart's `_visitImport`: when the
/// argument is a quoted string, use double quotes uniformly (keep single quotes when
/// the string contains `"`), and collapse whitespace inside the parentheses
/// (`url( 'x' )` -> `url("x")`).
pub(super) fn normalize_import_urls(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i].eq_ignore_ascii_case(&b'u')
            && i + 3 < bytes.len()
            && bytes[i + 1].eq_ignore_ascii_case(&b'r')
            && bytes[i + 2].eq_ignore_ascii_case(&b'l')
            && bytes[i + 3] == b'('
            && (i == 0 || !bytes[i - 1].is_ascii_alphanumeric() && bytes[i - 1] != b'-')
        {
            let inner_start = i + 4;
            let mut depth = 1usize;
            let mut j = inner_start;
            let mut quote: Option<u8> = None;
            while j < bytes.len() {
                match quote {
                    Some(q) => {
                        if bytes[j] == b'\\' {
                            j += 1;
                        } else if bytes[j] == q {
                            quote = None;
                        }
                    }
                    None => match bytes[j] {
                        b'"' | b'\'' => quote = Some(bytes[j]),
                        b'(' => depth += 1,
                        b')' => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    },
                }
                j += 1;
            }
            if j < bytes.len() {
                let inner = text[inner_start..j].trim();
                let requoted = match inner.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')) {
                    // Keep it as is when it contains a double quote or an escape, so that
                    // changing the quotes cannot change the semantics.
                    Some(content) if !content.contains('"') && !content.contains('\\') => {
                        format!("\"{content}\"")
                    }
                    _ => inner.to_string(),
                };
                out.push_str("url(");
                out.push_str(&requoted);
                out.push(')');
                i = j + 1;
                continue;
            }
        }
        let ch = text[i..].chars().next().expect("character boundary");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// at-rule prelude normalization: collapse whitespace; add a space after the colon in
/// `(prop:value)`; one space after commas; no whitespace inside parentheses.
/// Escape sequences are re-serialized the way dart does (`\0` -> `\0 `, `\25` -> `\%`),
/// and the terminator space is not removed.
pub(super) fn normalize_prelude(text: &str) -> String {
    // First replace the terminator whitespace of escape sequences with a sentinel: in
    // `a\61  b` the first space belongs to the escape and the second is a real space, so
    // collapsing whitespace directly would merge the two (dart outputs `aa b`, whereas
    // only `aab` would be left after collapsing).
    let guarded = guard_escape_spaces(text);
    let collapsed = value::normalize_raw(&guarded).replace(ESCAPE_SPACE, " ");
    let chars: Vec<char> = collapsed.chars().collect();
    let mut out = String::with_capacity(collapsed.len());
    let mut parens = 0usize;
    // The terminator space of an escape (`0\0 )`) must not be eaten by the whitespace
    // removal at parentheses/commas.
    let mut protect = 0usize;
    // Start of an identifier: only a digit appearing here needs the `\31 ` form
    // (`x\31 0` -> `x10`).
    let mut value_start = true;
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        if ch == '\\' {
            i += 1;
            if i >= chars.len() {
                out.push('\\');
                break;
            }
            if chars[i].is_ascii_hexdigit() {
                let mut hex = String::new();
                while i < chars.len() && hex.len() < 6 && chars[i].is_ascii_hexdigit() {
                    hex.push(chars[i]);
                    i += 1;
                }
                // One whitespace after an escape belongs to the escape itself
                // (`\61 b` -> `ab`).
                if i < chars.len() && chars[i].is_whitespace() {
                    i += 1;
                }
                let cp = u32::from_str_radix(&hex, 16).unwrap_or(0xFFFD);
                push_escaped_char(&mut out, char::from_u32(cp), value_start, &mut protect);
            } else {
                let escaped = chars[i];
                i += 1;
                if escaped != '\n' {
                    push_escaped_char(&mut out, Some(escaped), value_start, &mut protect);
                }
            }
            value_start = false;
            continue;
        }
        // Keep the inside of url(...) verbatim (the colon in `https://` must not be
        // touched).
        if ch.is_ascii_alphabetic() {
            let word_start = i;
            while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '-') {
                i += 1;
            }
            let word: String = chars[word_start..i].iter().collect();
            if i < chars.len() && chars[i] == '(' && word.eq_ignore_ascii_case("url") {
                let mut url = String::from("url(");
                i += 1;
                let mut depth = 1;
                while i < chars.len() && depth > 0 {
                    let c = chars[i];
                    if c == '(' {
                        depth += 1;
                    } else if c == ')' {
                        depth -= 1;
                    }
                    url.push(c);
                    i += 1;
                    if depth == 0 {
                        break;
                    }
                }
                out.push_str(&url);
            } else {
                out.push_str(&word);
            }
            value_start = false;
            continue;
        }
        match ch {
            '(' => {
                out.push('(');
                parens += 1;
                value_start = true;
                i += 1;
                while i < chars.len() && chars[i] == ' ' {
                    i += 1;
                }
                continue;
            }
            ')' => {
                trim_protected(&mut out, protect);
                out.push(')');
                parens = parens.saturating_sub(1);
                value_start = false;
                i += 1;
                continue;
            }
            ',' => {
                trim_protected(&mut out, protect);
                out.push(',');
                out.push(' ');
                value_start = true;
                i += 1;
                while i < chars.len() && chars[i] == ' ' {
                    i += 1;
                }
                continue;
            }
            ':' if parens > 0 => {
                let prev = out.chars().last();
                let next = chars.get(i + 1).copied();
                let is_prop = prev
                    .is_some_and(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
                    && next.is_some_and(|c| c != ':' && c != ' ');
                out.push(':');
                if is_prop {
                    out.push(' ');
                    value_start = true;
                    i += 1;
                    while i < chars.len() && chars[i] == ' ' {
                        i += 1;
                    }
                    continue;
                }
                i += 1;
                continue;
            }
            _ => {
                out.push(ch);
                value_start = false;
                i += 1;
            }
        }
    }
    trim_protected(&mut out, protect);
    out.trim_start().to_string()
}

/// Sentinel for the terminator whitespace of an escape (U+0001, which is not collapsed
/// as whitespace).
const ESCAPE_SPACE: char = '\u{1}';

/// Replace "the terminator whitespace that a CSS escape carries" with the sentinel and
/// keep the rest of the text as is (nothing inside quotes is touched).
fn guard_escape_spaces(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut quote: Option<char> = None;
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        if let Some(q) = quote {
            out.push(ch);
            if ch == '\\' {
                if let Some(next) = chars.get(i + 1) {
                    out.push(*next);
                    i += 2;
                    continue;
                }
            } else if ch == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        if ch == '"' || ch == '\'' {
            quote = Some(ch);
            out.push(ch);
            i += 1;
            continue;
        }
        if ch != '\\' {
            out.push(ch);
            i += 1;
            continue;
        }
        out.push(ch);
        i += 1;
        if i >= chars.len() {
            break;
        }
        if !chars[i].is_ascii_hexdigit() {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        let mut digits = 0;
        while i < chars.len() && digits < 6 && chars[i].is_ascii_hexdigit() {
            out.push(chars[i]);
            i += 1;
            digits += 1;
        }
        if i < chars.len() && chars[i].is_whitespace() {
            out.push(ESCAPE_SPACE);
            i += 1;
        }
    }
    out
}

/// Remove trailing spaces, but keep the "escape terminator space" (the bytes before
/// `protect`).
fn trim_protected(out: &mut String, protect: usize) {
    while out.len() > protect && out.ends_with(' ') {
        out.pop();
    }
}

/// Write back a single character per dart's identifier escaping rules: control
/// characters and digits at the start of an identifier use `\hex ` (with a terminator
/// space), other printable characters use `\c`, and alphanumerics stay as is.
fn push_escaped_char(out: &mut String, ch: Option<char>, value_start: bool, protect: &mut usize) {
    let Some(ch) = ch else {
        out.push('\u{FFFD}');
        return;
    };
    let cp = ch as u32;
    if cp < 0x20 || cp == 0x7f || (value_start && ch.is_ascii_digit()) {
        out.push_str(&format!("\\{cp:x} "));
        *protect = out.len();
    } else if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' || cp >= 0x80 {
        out.push(ch);
    } else if ch == '\\' {
        out.push_str("\\\\");
    } else {
        out.push('\\');
        out.push(ch);
    }
}

/// `@supports` conditions are **structured** (dart's `SupportsCondition`), not plain
/// text: redundant parentheses disappear at parse time, and serialization decides per
/// node type whether parentheses are needed.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SuppOp {
    And,
    Or,
}

impl SuppOp {
    fn from_word(word: &str) -> Option<Self> {
        match word.to_ascii_lowercase().as_str() {
            "and" => Some(Self::And),
            "or" => Some(Self::Or),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::And => "and",
            Self::Or => "or",
        }
    }
}

enum Supports {
    /// `(name: value)`
    Declaration {
        name: String,
        value: String,
    },
    /// `selector(:has(a))` / `foo(bar)`: the function form keeps its arguments verbatim.
    Function {
        name: String,
        args: String,
    },
    Negation(Box<Supports>),
    Operation {
        op: SuppOp,
        left: Box<Supports>,
        right: Box<Supports>,
    },
    /// A bare condition produced by `#{}` interpolation.
    Raw(String),
}

enum SuppFail {
    /// Syntax error: position (byte offset relative to the prelude) + message.
    At(usize, String),
    /// A shape this implementation does not recognize yet: fall back to plain text
    /// normalization (never silently rewrite).
    Unsupported,
}

type SuppResult<T> = Result<T, SuppFail>;

/// Parse an `@supports` prelude into a condition structure, then print it following
/// dart's rules.
pub(super) fn supports_prelude(
    prelude: &str,
    eval: &mut Evaluator<'_>,
    base: usize,
) -> Result<String, (usize, String)> {
    let mut parser = SupportsParser {
        text: prelude,
        pos: 0,
        eval,
        base,
    };
    match parser.condition_top() {
        Ok(condition) => Ok(write_supports(&condition)),
        Err(SuppFail::At(pos, message)) => Err((pos, message)),
        Err(SuppFail::Unsupported) => Ok(normalize_prelude(prelude)),
    }
}

struct SupportsParser<'a, 'e, 'v> {
    text: &'a str,
    pos: usize,
    eval: &'e mut Evaluator<'v>,
    base: usize,
}

impl SupportsParser<'_, '_, '_> {
    fn peek(&self) -> Option<char> {
        self.text[self.pos..].chars().next()
    }

    fn bump(&mut self) -> Option<char> {
        let ch = self.peek()?;
        self.pos += ch.len_utf8();
        Some(ch)
    }

    fn eat(&mut self, ch: char) -> bool {
        if self.peek() == Some(ch) {
            self.pos += ch.len_utf8();
            true
        } else {
            false
        }
    }

    fn skip_ws(&mut self) {
        while self.peek().is_some_and(char::is_whitespace) {
            self.bump();
        }
    }

    fn at_ident_start(&self) -> bool {
        self.peek().is_some_and(|c| {
            c.is_ascii_alphabetic() || c == '_' || c == '-' || c == '\\' || !c.is_ascii()
        })
    }

    /// An identifier (used for keywords such as `and`/`or`/`not`); escape sequences
    /// are swallowed verbatim.
    fn ident(&mut self) -> Option<String> {
        if !self.at_ident_start() {
            return None;
        }
        let start = self.pos;
        while let Some(ch) = self.peek() {
            if ch == '\\' {
                self.bump();
                self.bump();
            } else if ch.is_alphanumeric() || ch == '_' || ch == '-' {
                self.bump();
            } else {
                break;
            }
        }
        Some(self.text[start..self.pos].to_string())
    }

    fn condition_top(&mut self) -> SuppResult<Supports> {
        self.skip_ws();
        let condition = self.condition()?;
        self.skip_ws();
        if self.pos != self.text.len() {
            return Err(SuppFail::At(
                self.pos,
                "Expected @supports condition.".to_string(),
            ));
        }
        Ok(condition)
    }

    /// `[not] cond (and|or cond)*`: dart requires operators at the same level to be of
    /// the **same kind**; mixing them reports `Expected "and".`
    fn condition(&mut self) -> SuppResult<Supports> {
        // A top-level `not` is a prefix (`@supports not (a: b)`), not an identifier.
        let save = self.pos;
        if self.at_ident_start() {
            if let Some(word) = self.ident() {
                if word.eq_ignore_ascii_case("not") {
                    self.skip_ws();
                    let inner = self.in_parens()?;
                    return Ok(Supports::Negation(Box::new(inner)));
                }
            }
            self.pos = save;
        }
        let mut node = self.in_parens()?;
        let mut current: Option<SuppOp> = None;
        loop {
            self.skip_ws();
            let start = self.pos;
            let Some(word) = self.ident() else { break };
            let Some(op) = SuppOp::from_word(&word) else {
                self.pos = start;
                break;
            };
            if let Some(expected) = current {
                if expected != op {
                    return Err(SuppFail::At(
                        start,
                        format!("Expected \"{}\".", expected.as_str()),
                    ));
                }
            }
            current = Some(op);
            self.skip_ws();
            let right = self.in_parens()?;
            node = Supports::Operation {
                op,
                left: Box::new(node),
                right: Box::new(right),
            };
        }
        Ok(node)
    }

    fn in_parens(&mut self) -> SuppResult<Supports> {
        self.skip_ws();
        let start = self.pos;
        // `selector(...)` / `foo(bar)`: identifier + parentheses. A bare identifier can
        // only be a condition produced by interpolation.
        if self.at_ident_start() {
            let Some(name) = self.ident() else {
                return Err(SuppFail::Unsupported);
            };
            if name.eq_ignore_ascii_case("not") {
                return Err(SuppFail::At(
                    start,
                    "\"not\" is not a valid identifier here.".to_string(),
                ));
            }
            if self.eat('(') {
                let args_start = self.pos;
                let Some(end) = self.group_end()? else {
                    return Err(SuppFail::Unsupported);
                };
                let args = self.text[args_start..end].to_string();
                self.pos = end + 1;
                return Ok(Supports::Function { name, args });
            }
            // A bare identifier may only be followed by `and`/`or` or the end; otherwise
            // this implementation does not accept it (fall back to the raw text).
            let save = self.pos;
            self.skip_ws();
            let rest_ok = self.pos == self.text.len()
                || self.ident().is_some_and(|w| {
                    SuppOp::from_word(&w).is_some() && {
                        self.pos = save;
                        true
                    }
                });
            self.pos = save;
            if !rest_ok {
                return Err(SuppFail::Unsupported);
            }
            return Ok(Supports::Raw(self.text[start..self.pos].to_string()));
        }

        if !self.eat('(') {
            return Err(SuppFail::Unsupported);
        }
        self.skip_ws();
        // `(not <cond>)`
        let save = self.pos;
        if self.at_ident_start() {
            if let Some(word) = self.ident() {
                if word.eq_ignore_ascii_case("not") {
                    self.skip_ws();
                    let inner = self.in_parens()?;
                    if !self.eat(')') {
                        return Err(SuppFail::At(self.pos, "expected \")\".".to_string()));
                    }
                    return Ok(Supports::Negation(Box::new(inner)));
                }
            }
            self.pos = save;
        }
        // `((cond))`: parentheses are only grouping and simply disappear (dart drops them
        // at parse time).
        if self.peek() == Some('(') {
            let inner = self.condition()?;
            if !self.eat(')') {
                return Err(SuppFail::At(self.pos, "expected \")\".".to_string()));
            }
            return Ok(inner);
        }
        // `(name: value)`
        let Some(close) = self.group_end()? else {
            return Err(SuppFail::Unsupported);
        };
        let inner = &self.text[self.pos..close];
        let Some(colon) = top_level_colon(inner) else {
            return Err(SuppFail::Unsupported);
        };
        let name_start = self.pos;
        let name = self.evaluate(
            inner[..colon].trim(),
            name_start + leading_ws(&inner[..colon]),
        )?;
        let raw_value = &inner[colon + 1..];
        let value_start = name_start + colon + 1;
        let value = self.evaluate(raw_value.trim(), value_start + leading_ws(raw_value))?;
        self.pos = close + 1;
        Ok(Supports::Declaration { name, value })
    }

    /// Position of the matching `)` of the current `(` group (balanced inside the group
    /// by `(`/`)`, quotes and escapes).
    fn group_end(&self) -> SuppResult<Option<usize>> {
        let bytes = self.text.as_bytes();
        let mut depth = 0usize;
        let mut quote: Option<u8> = None;
        let mut i = self.pos;
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
                b'\\' => i += 1,
                b'(' => depth += 1,
                b')' => {
                    if depth == 0 {
                        return Ok(Some(i));
                    }
                    depth -= 1;
                }
                _ => {}
            }
            i += 1;
        }
        Ok(None)
    }

    /// Both the name and the value of a declaration are evaluated as SassScript
    /// (dart's `evaluateToCss`); when that fails the raw text is kept.
    fn evaluate(&mut self, text: &str, offset: usize) -> SuppResult<String> {
        if text.is_empty() {
            return Ok(String::new());
        }
        match self.eval.eval_text(text, self.base + offset) {
            Ok(value) => Ok(value.to_css().unwrap_or_else(|_| text.to_string())),
            // Real evaluation errors (undefined variable / invalid operation) are
            // reported; CSS values that cannot be parsed are kept as raw text
            // (equivalent to dart's `SupportsAnything` fallback).
            Err(error) if error.message.starts_with("Undefined") => {
                Err(SuppFail::At(offset, error.message))
            }
            Err(_) => Ok(normalize_prelude(text)),
        }
    }
}

fn leading_ws(text: &str) -> usize {
    text.len() - text.trim_start().len()
}

/// When `open` points at `(`, return the position of the matching `)` (skipping quotes
/// and escapes).
fn paren_end(text: &str, open: usize) -> Option<usize> {
    crate::text::matching(text, open).map(|end| end - 1)
}

/// The first **top-level** `:` of the group (skipping quotes, parentheses and escapes).
fn top_level_colon(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut depth = 0usize;
    let mut quote: Option<u8> = None;
    let mut i = 0;
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
            b'\\' => i += 1,
            b'(' | b'[' => depth += 1,
            b')' | b']' => depth = depth.saturating_sub(1),
            b':' if depth == 0 => {
                // `::` and the like are not declaration separators
                if bytes.get(i + 1) != Some(&b':') {
                    return Some(i);
                }
                i += 1;
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// dart's serialization rules: a whole condition is printed directly; as an operand of
/// `and`/`or`/`not`, **a Negation always gets parentheses** and **an Operation only
/// gets parentheses when the operator differs (or under `not`)**, while
/// declarations/functions/interpolations get none.
fn write_supports(condition: &Supports) -> String {
    match condition {
        Supports::Declaration { name, value } => format!("({name}: {value})"),
        Supports::Function { name, args } => format!("{name}({args})"),
        Supports::Raw(text) => text.clone(),
        Supports::Negation(inner) => format!("not {}", write_supports_operand(inner, None)),
        Supports::Operation { op, left, right } => format!(
            "{} {} {}",
            write_supports_operand(left, Some(*op)),
            op.as_str(),
            write_supports_operand(right, Some(*op))
        ),
    }
}

fn write_supports_operand(condition: &Supports, parent: Option<SuppOp>) -> String {
    let wrap = match condition {
        Supports::Negation(_) => true,
        Supports::Operation { op, .. } => parent.is_none_or(|outer| outer != *op),
        _ => false,
    };
    let text = write_supports(condition);
    if wrap { format!("({text})") } else { text }
}
