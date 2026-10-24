//! Selector parsing and normalization: parent selector expansion, `&`
//! substitution, pseudo-class arguments, `nth`/`an+b` normalization.
//!
//! Split out of `scss/mod.rs` (pure code move).

use super::*;

impl<'a> Converter<'a> {
    /// Position of `SassNestingDeclaration` (`font: { ... }`) inside a declaration
    /// value.
    pub(super) fn nesting_cut(&self, decl: &Declaration<'a>) -> Option<usize> {
        decl.value
            .iter()
            .position(|c| matches!(c, ComponentValue::SassNestingDeclaration(_)))
    }

    /// Selector list: interpolation -> normalization -> split on commas -> resolve
    /// against the parent selector.
    /// Trailing combinator of `X > { ... }`: oxc's selector span does not include it,
    /// so it has to be read from the source.
    fn trailing_combinator(&self, from: usize) -> Option<&'static str> {
        let rest = self.src().get(from..)?;
        let trimmed = rest.trim_start();
        let ch = trimmed.chars().next()?;
        let combinator = match ch {
            '>' => ">",
            '+' => "+",
            '~' => "~",
            _ => return None,
        };
        trimmed[ch.len_utf8()..]
            .trim_start()
            .starts_with('{')
            .then_some(combinator)
    }

    pub(super) fn selectors(
        &mut self,
        list: &SelectorList<'a>,
        parent: Option<&SelectorSet>,
    ) -> Result<(SelectorSet, Span), Box<CompileError>> {
        let mut parts: Vec<(String, bool)> = Vec::new();
        let mut prev_end: Option<usize> = None;
        for selector in &list.selectors {
            let raw = snippet(self.src(), selector.span.start, selector.span.end);
            let interpolated = self
                .eval
                .interpolate(&raw, selector.span.start)
                .map_err(|e| self.eval_err(e))?;
            let lead_newline = prev_end.is_some_and(|end| {
                self.src()
                    .get(end..selector.span.start)
                    .is_some_and(|gap| gap.contains('\n'))
            });
            let mut prev_piece_end = 0usize;
            for (i, (start, end)) in split_top_level_comma_ranges(&interpolated)
                .into_iter()
                .enumerate()
            {
                let piece = normalize_selector(interpolated[start..end].trim());
                let newline = if i == 0 {
                    lead_newline
                } else {
                    interpolated[prev_piece_end..start].contains('\n')
                };
                prev_piece_end = end;
                if piece.is_empty() {
                    continue;
                }
                parts.push((piece, newline));
            }
            // `X > { ... }`: the trailing combinator is not inside the selector span in
            // oxc, so append it from the source.
            if let Some(combinator) = self.trailing_combinator(selector.span.end) {
                if let Some((text, _)) = parts.last_mut() {
                    text.push(' ');
                    text.push_str(combinator);
                }
            }
            prev_end = Some(selector.span.end);
        }

        match parent {
            None => {
                // Inside a bare `@at-root`: there is no outer nesting, but `&` still
                // refers to the original selector.
                let outer = self.eval.parent_selector_text();
                let mut resolved = Vec::with_capacity(parts.len());
                for (part, line_break) in parts {
                    let text = if has_parent_selector(&part) {
                        match &outer {
                            Some(text) => part.replace('&', text),
                            None => {
                                return Err(self.err(
                                    list.span.start,
                                    "Top-level selectors may not contain the parent selector \"&\".",
                                ));
                            }
                        }
                    } else {
                        part
                    };
                    resolved.push(Sel { text, line_break });
                }
                Ok((SelectorSet { parts: resolved }, list.span))
            }
            Some(ps) => {
                let parents = &ps.parts;
                // dart-sass `resolveParentSelectors`: `flattenVertically` merges the
                // expansion results of the child selectors (interleaved by index), and
                // each result carries its own `lineBreak` -- ones containing `&` take the
                // parent's, pure descendant concatenations take the child's own.
                let lists: Vec<Vec<Sel>> = parts
                    .into_iter()
                    .map(|(part, line_break)| resolve_selector(&part, parents, line_break))
                    .collect();
                let resolved = flatten_vertically(lists);
                // Safety net: a leftover unreplaced `&` in the result is never allowed
                // (it would become invalid CSS outright).
                if let Some(bad) = resolved.iter().find(|sel| has_parent_selector(&sel.text)) {
                    return Err(self.err(
                        list.span.start,
                        format!(
                            "internal SCSS error: unreplaced `&` left in selector ({})",
                            bad.text
                        ),
                    ));
                }
                Ok((SelectorSet { parts: resolved }, list.span))
            }
        }
    }

    pub(super) fn keyframe_selectors(
        &mut self,
        block: &KeyframeBlock<'a>,
    ) -> Result<String, Box<CompileError>> {
        let mut out = String::new();
        let mut prev_end: Option<usize> = None;
        for selector in &block.selectors {
            let span = self.keyframe_span(selector);
            // Keyframe selector lists are always joined with `, ` (dart's
            // `_commaSeparator`).
            if prev_end.is_some() {
                out.push_str(", ");
            }
            out.push_str(&normalize_selector(&snippet(
                self.src(),
                span.start,
                span.end,
            )));
            prev_end = Some(span.end);
        }
        Ok(out)
    }

    pub(super) fn keyframe_span(&self, selector: &KeyframeSelector<'a>) -> Span {
        match selector {
            KeyframeSelector::Ident(i) => *i.span(),
            KeyframeSelector::Percentage(p) => p.span,
            KeyframeSelector::TimelineRange(r) => r.span,
        }
    }
}

pub(super) fn is_space_or_newline(ch: char) -> bool {
    matches!(ch, ' ' | '\t' | '\n' | '\r' | '\x0c')
}

/// Whether the selector contains the parent selector `&` (ignoring `&` inside
/// strings/attribute values).
/// Whether the selector has a `&` (including inside functional pseudo-class
/// arguments: `:not(&)`).
fn has_parent_selector(text: &str) -> bool {
    find_parent_in(text, true).is_some()
}

/// Whether there is a `&` at the top level (not inside parentheses): only used for
/// the "cartesian product" expansion.
fn find_parent(text: &str) -> Option<(usize, usize)> {
    find_parent_in(text, false)
}

/// Position of the first `&` and the length of the suffix right after it (the `-s`
/// of `&-s`).
/// With `deep == false` parentheses are skipped (those `&` belong to pseudo-class
/// arguments and are substituted separately per list).
fn find_parent_in(text: &str, deep: bool) -> Option<(usize, usize)> {
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
            b'[' | b'(' => depth += 1,
            b']' | b')' => depth = depth.saturating_sub(1),
            b'&' if deep || depth == 0 => {
                let mut len = 0usize;
                while bytes.get(i + 1 + len).is_some_and(|c| {
                    c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_') || *c >= 0x80
                }) {
                    len += 1;
                }
                return Some((i, len));
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Resolve a child selector against the parent selectors.
///
/// Observed rules for line breaks (dart `ComplexSelector.lineBreak`):
/// - pure descendant concatenation: a break as soon as `either parent or child
///   breaks`;
/// - containing `&`: only inherit the parent's break (`&.b,\n&.c` is output on one
///   line).
pub(super) fn resolve_selector(child: &str, parents: &[Sel], line_break: bool) -> Vec<Sel> {
    if !has_parent_selector(child) {
        return parents
            .iter()
            .map(|parent| Sel {
                text: format!("{} {child}", parent.text),
                line_break: parent.line_break || line_break,
            })
            .collect();
    }
    // A `&` inside functional pseudo-class arguments is substituted with the **whole
    // parent list** (`:not(.a, .b)`); only the remaining positions use the cartesian
    // product.
    let expanded = expand_pseudo_args(child, parents);
    if !has_parent_selector(&expanded) {
        return vec![Sel {
            text: expanded,
            line_break: parents[0].line_break,
        }];
    }
    let texts = parents.iter().map(|p| p.text.as_str()).collect::<Vec<_>>();
    expand_parent(&expanded, &texts)
        .into_iter()
        .map(|(first, text)| Sel {
            text,
            line_break: parents[first].line_break,
        })
        .collect()
}

/// Substitute each `&` (including `&-suffix`) with a parent selector (`& + &`
/// expands into a cartesian product).
/// The result carries the index of "which parent the first `&` used" (the line break
/// is taken from it).
fn expand_parent(child: &str, parents: &[&str]) -> Vec<(usize, String)> {
    let Some((at, suffix_len)) = find_parent(child) else {
        return vec![(0, child.to_string())];
    };
    let head = &child[..at];
    let suffix = &child[at + 1..at + 1 + suffix_len];
    let tail = &child[at + 1 + suffix_len..];
    let mut out = Vec::new();
    for (i, parent) in parents.iter().enumerate() {
        for (_, text) in expand_parent(&format!("{head}{parent}{suffix}{tail}"), parents) {
            out.push((i, text));
        }
    }
    out
}

/// `&` inside pseudo-class arguments: the semantics of dart's
/// `SelectorList.resolveParentSelectors` -- the argument is split as a selector
/// list, multiple `&` in each complex selector form a cartesian product, and
/// finally `flattenVertically` (interleaved by index) merges them; each `&` is
/// substituted with a single parent selector.
fn expand_pseudo_args(text: &str, parents: &[Sel]) -> String {
    let mut out = text.to_string();
    // Substitute from the innermost outwards (`:not(:is(&))` resolves `:is` first).
    loop {
        let Some((open, name)) = innermost_pseudo_arg_with_parent(&out) else {
            return out;
        };
        let Some(close) = paren_close(&out, open) else {
            return out;
        };
        let arg = out[open + 1..close].to_string();
        let resolved = resolve_pseudo_arg(&name, &arg, parents);
        out = format!("{}{}{}", &out[..open + 1], resolved, &out[close..]);
    }
}

/// Innermost functional pseudo-class whose argument contains `&`: returns the index
/// of `(` and the pseudo-class name.
fn innermost_pseudo_arg_with_parent(text: &str) -> Option<(usize, String)> {
    let bytes = text.as_bytes();
    let mut quote: Option<u8> = None;
    let mut found = None;
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
            b'(' => {
                if let Some(name) = pseudo_name_before(text, i) {
                    if let Some(close) = paren_close(text, i) {
                        if find_parent_in(&text[i + 1..close], true).is_some() {
                            found = Some((i, name));
                        }
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }
    found
}

/// Whether `(` is preceded by `:name` (a functional pseudo-class); returns the
/// pseudo-class name.
fn pseudo_name_before(text: &str, open: usize) -> Option<String> {
    let bytes = text.as_bytes();
    let mut i = open;
    while i > 0 {
        let b = bytes[i - 1];
        if b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b >= 0x80 {
            i -= 1;
        } else {
            break;
        }
    }
    if i == open || i == 0 || bytes[i - 1] != b':' {
        return None;
    }
    Some(text[i..open].to_string())
}

fn paren_close(text: &str, open: usize) -> Option<usize> {
    crate::text::matching(text, open).map(|end| end - 1)
}

/// `:nth-child(2n of &)`: the selector list only starts after `of`, and the prefix
/// is printed once.
fn resolve_pseudo_arg(name: &str, arg: &str, parents: &[Sel]) -> String {
    let lower = name.to_ascii_lowercase();
    if lower == "nth-child" || lower == "nth-last-child" {
        if let Some(at) = top_level_of(arg) {
            let prefix = &arg[..at];
            let list = join_selector_list(&resolve_selector_text(&arg[at + 4..], parents));
            return format!("{} of {list}", prefix.trim_end());
        }
    }
    join_selector_list(&resolve_selector_text(arg, parents))
}

/// Position of a top-level ` of ` (whitespace on both sides).
fn top_level_of(text: &str) -> Option<usize> {
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
            b'(' | b'[' => depth += 1,
            b')' | b']' => depth = depth.saturating_sub(1),
            b' ' if depth == 0 => {
                let rest = &text[i..];
                if rest.len() > 4
                    && rest[..4].eq_ignore_ascii_case(" of ")
                    && bytes.get(i + 4) != Some(&b' ')
                {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Parse a piece of selector text (a pseudo-class argument) into a selector list,
/// expanding `&` for each parent.
fn resolve_selector_text(text: &str, parents: &[Sel]) -> Vec<Sel> {
    let texts = parents.iter().map(|p| p.text.as_str()).collect::<Vec<_>>();
    let lists: Vec<Vec<Sel>> = split_top_level_commas(text)
        .into_iter()
        .filter(|part| !part.trim().is_empty())
        .map(|part| {
            expand_cartesian(part.trim(), &texts)
                .into_iter()
                .map(|(index, text)| Sel {
                    text,
                    line_break: index.is_some_and(|i| parents[i].line_break),
                })
                .collect()
        })
        .collect();
    flatten_vertically(lists)
}

/// Cartesian product of multiple `&`; the result carries "which parent the first `&`
/// used".
fn expand_cartesian(child: &str, parents: &[&str]) -> Vec<(Option<usize>, String)> {
    let Some((at, suffix_len)) = find_parent_in(child, true) else {
        return vec![(None, child.to_string())];
    };
    let head = &child[..at];
    let suffix = &child[at + 1..at + 1 + suffix_len];
    let tail = &child[at + 1 + suffix_len..];
    let mut out = Vec::new();
    for (i, parent) in parents.iter().enumerate() {
        for (_, text) in expand_cartesian(&format!("{head}{parent}{suffix}{tail}"), parents) {
            out.push((Some(i), text));
        }
    }
    out
}

/// dart's `flattenVertically`: merge several lists interleaved by index.
fn flatten_vertically(lists: Vec<Vec<Sel>>) -> Vec<Sel> {
    let longest = lists.iter().map(Vec::len).max().unwrap_or(0);
    let mut out = Vec::new();
    for i in 0..longest {
        for list in &lists {
            if let Some(sel) = list.get(i) {
                out.push(Sel {
                    text: sel.text.clone(),
                    line_break: sel.line_break,
                });
            }
        }
    }
    out
}

/// Print a selector list with dart's line-break positions (used inside pseudo-class
/// arguments).
fn join_selector_list(list: &[Sel]) -> String {
    let mut out = String::new();
    for (i, sel) in list.iter().enumerate() {
        if i > 0 {
            out.push(',');
            out.push(if sel.line_break { '\n' } else { ' ' });
        }
        out.push_str(&sel.text);
    }
    out
}

fn split_top_level_comma_ranges(text: &str) -> Vec<(usize, usize)> {
    crate::text::split_top_level(text, b',')
}

pub(super) fn split_top_level_commas(text: &str) -> Vec<&str> {
    crate::text::split_top_level(text, b',')
        .into_iter()
        .map(|(start, end)| &text[start..end])
        .collect()
}

/// Whether the block contains rule-like statements (decides whether an at-rule body
/// is processed as rules or as a declaration list).
pub(super) fn has_rule_statements(stmts: &[Statement<'_>]) -> bool {
    stmts.iter().any(|st| {
        !matches!(
            st,
            Statement::Declaration(_) | Statement::SassVariableDeclaration(_)
        )
    })
}

pub(super) fn snippet(src: &str, start: usize, end: usize) -> String {
    src.get(start..end).unwrap_or("").to_string()
}

/// Static container of a declaration-like at-rule. dart rejects some declarations by
/// container already at **parse time** (observed: a `@mixin` inside an `@if false`
/// branch errors too), so this is a static walk independent of evaluation.
#[derive(Clone, Copy, Default)]
pub(super) struct Nesting {
    /// mixin body (dart `_inMixin`).
    pub(super) mixin: bool,
    /// content block of `@include ... { }` (dart `_inContentBlock`).
    pub(super) content_block: bool,
    /// Inside a control directive body: `@if` / `@else` / `@each` / `@for` / `@while`.
    pub(super) control: bool,
    /// function body (dart `_inFunction`): only the at-rules listed below are allowed.
    pub(super) function: bool,
}

impl Nesting {
    /// Whitelist of a function body (dart's function sub-parser).
    pub(super) fn function_allows(name: &str) -> bool {
        matches!(
            name,
            "debug" | "warn" | "error" | "each" | "for" | "while" | "if" | "return"
        )
    }
}

/// Selector normalization: collapse whitespace, one space on both sides of a
/// combinator, one space after a comma, no whitespace inside parentheses.
pub(super) fn normalize_selector(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pending_space = false;
    let mut parens = 0usize;
    let mut brackets = 0usize;
    let mut quote: Option<char> = None;
    let mut after_eq = false;
    let mut chars = text.chars().peekable();

    let flush_space = |out: &mut String, pending: &mut bool| {
        if *pending && !out.is_empty() && !out.ends_with(' ') && !out.ends_with('(') {
            out.push(' ');
        }
        *pending = false;
    };

    while let Some(ch) = chars.next() {
        if let Some(q) = quote {
            out.push(ch);
            if ch == '\\' {
                if let Some(next) = chars.next() {
                    out.push(next);
                }
            } else if ch == q {
                quote = None;
            }
            continue;
        }
        match ch {
            '\'' | '"' if brackets > 0 => {
                // A string inside an attribute selector: a legal identifier loses its
                // quotes, otherwise it is rewritten per the quoting rules.
                let mut raw = String::new();
                raw.push(ch);
                while let Some(c) = chars.next() {
                    raw.push(c);
                    if c == '\\' {
                        if let Some(next) = chars.next() {
                            raw.push(next);
                        }
                    } else if c == ch {
                        break;
                    }
                }
                let text = value::decode_css_string(&raw);
                let is_value = out.trim_end().ends_with('=');
                pending_space = false;
                while out.ends_with(' ') {
                    out.pop();
                }
                if is_value && value::is_css_identifier(&text) {
                    out.push_str(&text);
                } else {
                    out.push_str(&value::quote_string(&text));
                }
                // The quoted string is an attribute value: the whitespace after it
                // separates descendant selectors and must not be swallowed.
                after_eq = false;
            }
            '\'' | '"' => {
                flush_space(&mut out, &mut pending_space);
                quote = Some(ch);
                out.push(ch);
            }
            '[' => {
                flush_space(&mut out, &mut pending_space);
                out.push(ch);
                brackets += 1;
            }
            ']' => {
                while out.ends_with(' ') {
                    out.pop();
                }
                out.push(ch);
                brackets = brackets.saturating_sub(1);
                after_eq = false;
            }
            '(' => {
                while out.ends_with(' ') {
                    out.pop();
                }
                out.push(ch);
                parens += 1;
                pending_space = false;
            }
            ')' => {
                while out.ends_with(' ') {
                    out.pop();
                }
                out.push(ch);
                parens = parens.saturating_sub(1);
                pending_space = false;
            }
            ',' => {
                while out.ends_with(' ') {
                    out.pop();
                }
                out.push(',');
                out.push(' ');
                pending_space = false;
            }
            '>' | '+' | '~' if parens == 0 && brackets == 0 => {
                while out.ends_with(' ') {
                    out.pop();
                }
                if !out.is_empty() {
                    out.push(' ');
                }
                out.push(ch);
                out.push(' ');
                pending_space = false;
            }
            '=' if brackets > 0 => {
                while out.ends_with(' ') {
                    out.pop();
                }
                out.push('=');
                pending_space = false;
                after_eq = true;
            }
            c if c.is_whitespace() => {
                if !after_eq {
                    pending_space = true;
                }
            }
            c => {
                after_eq = false;
                flush_space(&mut out, &mut pending_space);
                out.push(c);
            }
        }
    }
    normalize_nth(out.trim_end())
}

/// An+B argument reformatting for `:nth-child()` / `:nth-last-child()`.
///
/// dart-sass 1.35.2 only serializes An+B structurally for these two pseudo-classes
/// (whitespace removed, `n`/`odd`/`even` lowercased, the original digit spelling and
/// sign of A/B kept); `:nth-of-type()` / `:nth-last-of-type()` are observed to be
/// **not** normalized (whitespace is collapsed like in any other selector), so they
/// are not included here.
fn normalize_nth(text: &str) -> String {
    const NAMES: [&str; 2] = ["nth-last-child", "nth-child"];
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        let rest: String = chars[i..].iter().take(20).collect();
        let lower = rest.to_ascii_lowercase();
        let matched = (chars[i] == ':')
            .then(|| {
                NAMES
                    .iter()
                    .find(|name| lower.starts_with(&format!(":{name}(")))
                    .map(|name| name.len())
            })
            .flatten();
        let Some(name_len) = matched else {
            out.push(chars[i]);
            i += 1;
            continue;
        };
        let open = i + 1 + name_len;
        if let Some(close) = matching_paren(&chars, open) {
            let inner: String = chars[open + 1..close].iter().collect();
            out.push_str(&chars[i..=open].iter().collect::<String>());
            out.push_str(&normalize_anb(&inner));
            out.push(')');
            i = close + 1;
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

fn matching_paren(chars: &[char], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut quote: Option<char> = None;
    for (i, &ch) in chars.iter().enumerate().skip(open) {
        if let Some(q) = quote {
            if ch == q {
                quote = None;
            }
            continue;
        }
        match ch {
            '\'' | '"' => quote = Some(ch),
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// An+B serialization: remove all whitespace, lowercase `n`/`odd`/`even`, and use a
/// single space in `of <selector>`.
fn normalize_anb(inner: &str) -> String {
    let (anb, rest) = split_of(inner);
    let anb: String = anb.split_whitespace().collect::<String>().replace('N', "n");
    let anb = if anb.eq_ignore_ascii_case("odd") || anb.eq_ignore_ascii_case("even") {
        anb.to_ascii_lowercase()
    } else {
        anb
    };
    match rest {
        Some(selector) => {
            let selector = normalize_selector(selector);
            format!("{anb} of {selector}")
        }
        None => anb,
    }
}

/// Split out the `of` clause at depth 0 (`2n+1 of .a`).
fn split_of(inner: &str) -> (&str, Option<&str>) {
    let bytes = inner.as_bytes();
    let mut depth = 0usize;
    let mut quote: Option<u8> = None;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if let Some(q) = quote {
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
            b'o' | b'O' if depth == 0 => {
                let prev_space = inner[..i].ends_with(char::is_whitespace);
                let word = inner.get(i..i + 2).unwrap_or("");
                let next_space = inner[i + 2..].starts_with(char::is_whitespace);
                if prev_space && word.eq_ignore_ascii_case("of") && next_space {
                    return (inner[..i].trim(), Some(inner[i + 2..].trim()));
                }
            }
            _ => {}
        }
        i += 1;
    }
    (inner.trim(), None)
}
