use oxc_css_parser::ast::*;
use oxc_css_parser::pos::Span;

use super::Compiler;
use super::eval::interp_tokens;
use super::extend::{self, Need, Sel};
use super::value;

/// Splits selector text into less mixin element tokens (simple selectors; combinators and pseudo-class details are ignored).
pub(super) fn selector_tokens(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if matches!(c, '.' | '#' | '&' | '*' | ',') || c.is_alphanumeric() || c == '_' || c == '-' {
            let start = i;
            i += 1;
            while i < bytes.len() {
                let c = bytes[i] as char;
                if c == '\\' {
                    // Keep escapes like `.\31 23` whole
                    i += 2;
                } else if c.is_alphanumeric() || c == '_' || c == '-' {
                    i += 1;
                } else {
                    break;
                }
            }
            // Escapes/multibyte characters can leave `i` mid-character: back up to the nearest char boundary
            let end = (start..=i.min(text.len()))
                .rev()
                .find(|&k| text.is_char_boundary(k))
                .unwrap_or(start);
            let token = &text[start..end];
            match token {
                "," | "&" => {}
                _ => tokens.push(token.to_string()),
            }
            continue;
        }
        i += 1;
    }
    tokens
}

/// Scope token: an `&`-only selector has no tokens of its own but still occupies a scope level (inner definitions must not leak).
pub(super) fn scope_tokens(own: Vec<String>) -> Vec<String> {
    if own.is_empty() {
        vec!["&".to_string()]
    } else {
        own
    }
}

/// Raw text of all selectors of a rule (each selector is kept whole; tokens are split only after interpolation).
pub(super) fn rule_selector_raw_lists(sel: &SelectorList, src: &str) -> Vec<Vec<String>> {
    sel.selectors
        .iter()
        .filter_map(|s| src.get(s.span.start..s.span.end))
        .map(|t| vec![t.trim().to_string()])
        .filter(|t| !t[0].is_empty())
        .collect()
}

/// The rule's own selector tokens (after interpolation; the first selector is used when there are several).
pub(super) fn rule_own_tokens<'a>(
    sel: &SelectorList,
    src: &'a str,
    scope: &dyn value::Scope<'a>,
) -> Vec<String> {
    sel.selectors
        .first()
        .and_then(|s| src.get(s.span.start..s.span.end))
        .map(|t| interp_tokens(&[t.trim().to_string()], src, scope))
        .unwrap_or_default()
}

/// Whether the selector is exactly one `&` (less `isJustParentSelector`: folded into the parent rule).
pub(super) fn is_just_parent(sel: &SelectorList, src: &str) -> bool {
    sel.selectors.len() == 1
        && sel
            .selectors
            .first()
            .and_then(|s| src.get(s.span.start..s.span.end))
            .map(|t| t.trim() == "&")
            .unwrap_or(false)
}

/// Selector list expansion: comma strings produced by interpolation are split into multiple selectors (less `SelectorList` puts each on its own line).
pub(super) fn split_selectors(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut depth = 0usize;
    let mut quote: Option<char> = None;
    for (i, ch) in text.char_indices() {
        match quote {
            Some(q) if ch == q => quote = None,
            Some(_) => {}
            None => match ch {
                '"' | '\'' => quote = Some(ch),
                '(' | '[' => depth += 1,
                ')' | ']' => depth = depth.saturating_sub(1),
                ',' if depth == 0 => {
                    out.push(text[start..i].trim().to_string());
                    start = i + 1;
                }
                _ => {}
            },
        }
    }
    out.push(text[start..].trim().to_string());
    out.retain(|s| !s.is_empty());
    out
}

pub(super) fn stmt_start(st: &Statement) -> Option<usize> {
    match st {
        Statement::Declaration(d) => Some(d.span.start),
        Statement::QualifiedRule(r) => Some(r.span.start),
        Statement::AtRule(a) => Some(a.span.start),
        Statement::KeyframeBlock(k) => Some(k.span.start),
        Statement::LessVariableDeclaration(d) => Some(d.span.start),
        Statement::LessExtendRule(r) => Some(r.span.start),
        Statement::LessMixinCall(c) => Some(c.span.start),
        Statement::LessVariableCall(c) => Some(c.span.start),
        Statement::LessFunctionCall(f) => Some(f.span.start),
        Statement::LessConditionalQualifiedRule(r) => Some(r.span.start),
        _ => None,
    }
}

/// Parent × child combination: already done by `extend::combine` (following less `joinSelector`).
/// Commas introduced by interpolation must still be split into several selectors.
pub(super) fn split_interpolated(sel: Sel) -> Vec<Sel> {
    let text = extend::render(&sel);
    match split_selectors(&text) {
        parts if parts.len() > 1 => parts.iter().map(|p| extend::parse(p)).collect(),
        _ => vec![sel],
    }
}

/// Combinator text (matching less `Combinator.value`).
///
/// A less slashed combinator is an arbitrary `/name/` (`/shadow/`), so it can only be taken from the source text.
pub(super) fn combinator_text(c: &Combinator, src: &str) -> String {
    if matches!(c.kind, CombinatorKind::Slashed) {
        return src.get(c.span.start..c.span.end).unwrap_or("/").to_string();
    }
    match c.kind {
        CombinatorKind::Descendant => " ".to_string(),
        CombinatorKind::NextSibling => "+".to_string(),
        CombinatorKind::Child => ">".to_string(),
        CombinatorKind::LaterSibling => "~".to_string(),
        CombinatorKind::Column => "||".to_string(),
        CombinatorKind::Deep => "/deep/".to_string(),
        CombinatorKind::Slashed => "/".to_string(),
        CombinatorKind::ShadowChild => "^".to_string(),
        CombinatorKind::ShadowDescendant => "^^".to_string(),
    }
}

/// Combinator to attach to the next element (elements inside a compound selector have none).
pub(super) fn element_combinator(
    pending: &mut Option<String>,
    first: &mut bool,
    selector_empty: bool,
) -> String {
    if !*first {
        return String::new();
    }
    *first = false;
    if selector_empty {
        pending.take().unwrap_or_default()
    } else {
        pending.take().unwrap_or_else(|| " ".to_string())
    }
}

impl<'a> Compiler<'a> {
    /// Selector list → flattened element sequence (each path also yields its `:extend(...)`).
    pub(super) fn selector_sels(&mut self, sel: &SelectorList) -> Vec<(Sel, Vec<Need>, Span)> {
        let mut out = Vec::with_capacity(sel.selectors.len());
        for complex in &sel.selectors {
            let complex_span = complex.span;
            let mut elements: Sel = Vec::new();
            let mut needs: Vec<Need> = Vec::new();
            // The combinator waits for the next compound selector to land (isomorphic to less's Element.combinator)
            let mut pending: Option<String> = None;
            for child in &complex.children {
                match child {
                    ComplexSelectorChild::Combinator(c) => {
                        pending = Some(combinator_text(c, self.src))
                    }
                    ComplexSelectorChild::CompoundSelector(compound) => {
                        let mut first = true;
                        for simple in &compound.children {
                            if let SimpleSelector::PseudoClass(p) = simple
                                && let Some(arg) = &p.arg
                                && let PseudoClassSelectorArgKind::LessExtendList(list) = &arg.kind
                            {
                                needs.extend(self.needs_of(list));
                                continue;
                            }
                            if let SimpleSelector::Nesting(n) = simple {
                                // `&-suffix` is "a `&` element + a suffix element" and must not merge into one
                                let combinator = element_combinator(
                                    &mut pending,
                                    &mut first,
                                    elements.is_empty(),
                                );
                                elements.push(extend::Element {
                                    combinator,
                                    value: "&".to_string(),
                                });
                                if let Some(suffix) = &n.suffix {
                                    let suffix_span = *suffix.span();
                                    let text =
                                        self.interpolate_at(self.slice(suffix_span), suffix_span);
                                    if !text.is_empty() {
                                        let combinator = element_combinator(
                                            &mut pending,
                                            &mut first,
                                            elements.is_empty(),
                                        );
                                        elements.push(extend::Element {
                                            combinator,
                                            value: text,
                                        });
                                    }
                                }
                                continue;
                            }
                            let span = *simple.span();
                            let text = self.interpolate_at(self.slice(span), span);
                            // less's `Element` trims string values (trailing spaces in escape sequences are removed)
                            let text = text.trim();
                            if text.is_empty() {
                                continue;
                            }
                            let combinator =
                                element_combinator(&mut pending, &mut first, elements.is_empty());
                            elements.push(extend::Element {
                                combinator,
                                value: text.to_string(),
                            });
                        }
                    }
                }
            }
            out.push((elements, needs, complex_span));
        }
        out
    }

    /// `:extend(...)` argument list → match needs.
    pub(super) fn needs_of(&self, list: &LessExtendList<'_>) -> Vec<Need> {
        list.elements
            .iter()
            .map(|e| {
                let raw = self.slice(e.selector.span);
                let text = self.interpolate_at(raw, e.selector.span);
                Need {
                    target: extend::parse(&text),
                    all: e.all.is_some(),
                }
            })
            .collect()
    }
}
