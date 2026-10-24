//! Value evaluation: variable substitution, arithmetic, colors, and built-in functions.
//!
//! The conformance baseline is less@4.1.3 (default `math: parens-division`): `/` is computed only when wrapped in parentheses,
//! otherwise it is emitted as-is; `*` `+` `-` are always computed.

use std::path::{Path, PathBuf};

use oxc_css_parser::ast::*;

/// Variable binding: an AST node (inside the source file), text (globalVars), or a color.
///
/// Colors are a separate kind: in less a variable's value is a Color object, and `darken(@c, 10%)` uses the floating-point channel,
/// so writing it as `#rrggbb` and parsing it again would lose the fractional part (`#286090` would become `#285f8f`).
#[derive(Clone)]
pub(crate) enum VarRef<'a> {
    Ast(&'a ComponentValue<'a>),
    Text(String),
    /// A list value (`@arguments`: the grouping between arguments must be preserved, `length`/`extract` rely on it).
    List(Vec<String>),
    Color(Color),
    /// A detached ruleset, or the result of a mixin call used as a ruleset (`@dr()` / `@p[key]`).
    Ruleset(std::rc::Rc<crate::less::RuleSet<'a>>),
}

/// Property lookup result: a single declaration, or the grouped values after `+:` / `+_:` merging.
#[derive(Clone)]
pub(crate) enum PropSource<'a> {
    Decl(&'a Declaration<'a>),
    /// Comma groups on the outside, space separation within (the product of less `_mergeRules`).
    Merged(Vec<Vec<&'a Declaration<'a>>>),
}

/// Variable/property lookup (the scope chain is provided by the caller).
pub(crate) trait Scope<'a> {
    fn var(&self, name: &str) -> Option<VarRef<'a>>;
    fn prop(&self, name: &str) -> Option<Vec<PropSource<'a>>>;
    /// Block comment trailing a variable declaration's value: less treats comments as value nodes, so it must be brought out at references.
    fn var_suffix(&self, _name: &str) -> Option<String> {
        None
    }
    /// `$prop` did not match: less reports `Property '$prop' is undefined`, and the caller records the error accordingly.
    fn prop_miss(&self, _name: &str, _span: oxc_css_parser::pos::Span) {}
    /// `@var` did not match: less reports `variable @var is undefined`.
    ///
    /// Only called at references that are genuinely evaluated: probe cases such as `isdefined(@x)` and unexpanded detached rulesets
    /// pass `None` as the span and produce no error.
    fn var_miss(&self, _name: &str, _span: oxc_css_parser::pos::Span) {}
    /// Lookup root for file functions (`data-uri` / `image-size`): the entry less file's directory plus `paths`.
    fn file_roots(&self) -> Option<(&Path, &[PathBuf])> {
        None
    }
}

fn text<'a>(src: &'a str, cv: &ComponentValue) -> &'a str {
    text_of(*cv.span(), src)
}

fn text_of(span: oxc_css_parser::pos::Span, src: &str) -> &str {
    let (lo, hi) = (span.start, span.end);
    if lo <= hi && hi <= src.len() {
        &src[lo..hi]
    } else {
        ""
    }
}

/// Placeholder text for an unmatched variable: records `variable @name is undefined` on demand.
fn undefined_var<'a>(
    name: &str,
    scope: &dyn Scope<'a>,
    span: Option<oxc_css_parser::pos::Span>,
) -> String {
    if let Some(span) = span {
        scope.var_miss(name, span);
    }
    format!("@{name}")
}

/// `isdefined(expr)`: less's `isdefined` disables argument evaluation, and a failed evaluation means false.
pub(crate) fn probe_defined<'a>(cv: &ComponentValue<'a>, scope: &dyn Scope<'a>) -> bool {
    match cv {
        ComponentValue::LessVariable(v) => scope.var(v.name.name).is_some(),
        ComponentValue::LessVariableCall(c) => scope.var(c.variable.name.name).is_some(),
        // `@x[...]`: confirm the variable exists first, then worry about the lookup result
        ComponentValue::LessNamespaceValue(ns) => match &ns.callee {
            LessNamespaceValueCallee::LessVariable(v) => scope.var(v.name.name).is_some(),
            LessNamespaceValueCallee::LessMixinCall(_) => true,
        },
        _ => true,
    }
}

/// `isruleset(expr)`: whether `expr` is a detached ruleset (a variable pointing at one counts too).
pub(crate) fn probe_ruleset<'a>(cv: &ComponentValue<'a>, scope: &dyn Scope<'a>) -> bool {
    match cv {
        ComponentValue::LessDetachedRuleset(_) => true,
        ComponentValue::LessVariable(v) => matches!(
            scope.var(v.name.name),
            Some(VarRef::Ast(ComponentValue::LessDetachedRuleset(_)))
        ),
        ComponentValue::LessVariableCall(c) => matches!(
            scope.var(c.variable.name.name),
            Some(VarRef::Ast(ComponentValue::LessDetachedRuleset(_)))
        ),
        _ => false,
    }
}

pub(crate) fn eval<'a>(
    cv: &ComponentValue<'a>,
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
) -> String {
    if depth > 32 {
        return text(src, cv).to_string();
    }
    match cv {
        // Numbers are always re-parsed from the source text into f64: f32 in the AST loses precision (0.7 -> 0.69999999)
        ComponentValue::Number(_) => {
            fmt_number(parse_number(text(src, cv)).map(|(v, _)| v).unwrap_or(0.0))
        }
        ComponentValue::Dimension(d) => match parse_number(text(src, cv)) {
            Some((v, u)) => format!(
                "{}{}",
                fmt_number(v),
                u.unwrap_or_else(|| d.unit.name.to_string())
            ),
            None => text(src, cv).to_string(),
        },
        ComponentValue::Percentage(_) => format!(
            "{}%",
            fmt_number(parse_number(text(src, cv)).map(|(v, _)| v).unwrap_or(0.0))
        ),
        ComponentValue::HexColor(_) => text(src, cv).to_string(),
        ComponentValue::LessVariable(v) => {
            var_value(v.name.name, src, scope, depth, Some(*cv.span()))
        }
        ComponentValue::LessVariableCall(call) => {
            var_value(call.variable.name.name, src, scope, depth, Some(*cv.span()))
        }
        // `@@b`: first evaluate @b as the variable name, then look up @<value>
        ComponentValue::LessVariableVariable(vv) => {
            let inner = var_name(
                vv.variable.name.name,
                src,
                scope,
                depth + 1,
                Some(*cv.span()),
            );
            match scope.var(&inner) {
                Some(VarRef::Ast(node)) => eval(node, src, scope, depth + 1),
                Some(VarRef::List(v)) => v.join(" "),
                Some(VarRef::Text(t)) => t,
                Some(VarRef::Color(c)) => c.to_css(),
                Some(VarRef::Ruleset(_)) => String::new(),
                None => undefined_var(&inner, scope, Some(*cv.span())),
            }
        }
        // `$prop`: the value of a same-named property in the same scope (including outer rules)
        ComponentValue::LessPropertyVariable(p) => {
            match prop_text(p.name.name, src, scope, depth) {
                Some(text) => text,
                None => {
                    scope.prop_miss(p.name.name, p.span);
                    format!("${}", p.name.name)
                }
            }
        }
        ComponentValue::LessBinaryOperation(op) => eval_binary(op, src, scope, depth),
        // math=parens-division: division inside parentheses is computed, outside it is kept as-is
        ComponentValue::LessParenthesizedOperation(p) => {
            eval_paren(&p.operation, src, scope, depth + 1)
        }
        // `~"..."` -> drop the ~ and the quotes; `@{}` / `${}` interpolation still happens inside the string
        ComponentValue::LessEscapedStr(_) => {
            // Take the source text to preserve escapes (`\9` in `~"\9"` must not be decoded into a tab)
            let raw = text(src, cv);
            let inner = raw.strip_prefix('~').unwrap_or(raw);
            interpolate(unquote_ref(inner), src, scope, depth, Some(*cv.span()))
        }
        ComponentValue::InterpolableStr(InterpolableStr::LessInterpolated(s)) => {
            interpolated_str(&s.elements, src, scope, depth, *cv.span())
        }
        ComponentValue::InterpolableIdent(InterpolableIdent::LessInterpolated(i)) => i
            .elements
            .iter()
            .map(|e| match e {
                LessInterpolatedIdentElement::Static(p) => p.value.to_string(),
                LessInterpolatedIdentElement::Variable(v) => {
                    interp_var(v.name.name, src, scope, depth, Some(*cv.span()))
                }
                LessInterpolatedIdentElement::Property(p) => {
                    prop_text(p.name.name, src, scope, depth)
                        .unwrap_or_else(|| format!("${{{}}}", p.name.name))
                }
            })
            .collect(),
        ComponentValue::Function(f) => eval_function(f, src, scope, depth),
        ComponentValue::LessList(list) => {
            // A pure token sequence (a less anonymous value such as `(min-width: 768px) and (...)`) is taken from the source text
            // and emitted as-is: joining token by token would insert extra spaces and could break forms like `1 ,2`
            if list
                .elements
                .iter()
                .all(|e| matches!(e, ComponentValue::TokenWithSpan(_)))
            {
                return substitute(text(src, cv), src, scope, depth, Some(*cv.span()));
            }
            // Whether a comma separator exists decides the joining (space lists use spaces); delimiters such as `/` get no spaces on either side
            // (less parses it as an Operation, and genCSS only adds spaces when isSpaced)
            let sep = if list.comma_spans.is_some() {
                ", "
            } else {
                " "
            };
            let mut out = String::new();
            for (i, e) in list.elements.iter().enumerate() {
                let delim = matches!(e, ComponentValue::Delimiter(_));
                if i > 0 && !delim && !matches!(list.elements[i - 1], ComponentValue::Delimiter(_))
                {
                    out.push_str(sep);
                }
                out.push_str(&eval(e, src, scope, depth + 1));
            }
            out
        }
        ComponentValue::LessNegativeValue(n) => {
            let inner = eval(&n.value, src, scope, depth + 1);
            format!("-{inner}")
        }
        ComponentValue::Url(u) => eval_url(u, src, scope, depth, Some(*cv.span())),
        ComponentValue::Ratio(_) => text(src, cv).to_string(),
        ComponentValue::Delimiter(_) => text(src, cv).to_string(),
        _ => text(src, cv).to_string(),
    }
}

/// Variable value (keeps `@name` and records an error when undefined).
fn var_value<'a>(
    name: &str,
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
    span: Option<oxc_css_parser::pos::Span>,
) -> String {
    let text = match scope.var(name) {
        Some(VarRef::Ast(node)) => eval(node, src, scope, depth + 1),
        Some(VarRef::List(v)) => v.join(" "),
        Some(VarRef::Text(t)) => t,
        Some(VarRef::Color(c)) => c.to_css(),
        Some(VarRef::Ruleset(_)) => String::new(),
        None => undefined_var(name, scope, span),
    };
    match scope.var_suffix(name) {
        Some(suffix) => format!("{text} {suffix}"),
        None => text,
    }
}

/// Text of a variable used as a name: strings yield their content (unquoted), everything else is evaluated as a value.
pub(crate) fn var_name<'a>(
    name: &str,
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
    span: Option<oxc_css_parser::pos::Span>,
) -> String {
    match scope.var(name) {
        Some(VarRef::Ast(node)) => text_value(node, src, scope, depth + 1),
        Some(VarRef::List(v)) => v.join(" "),
        Some(VarRef::Text(t)) => unquote(&t),
        Some(VarRef::Color(c)) => c.to_css(),
        Some(VarRef::Ruleset(_)) => String::new(),
        None => {
            if let Some(span) = span {
                scope.var_miss(name, span);
            }
            name.to_string()
        }
    }
}

/// Text for interpolation: quoted variables yield the content inside the quotes, others the CSS text of the evaluated result.
fn text_value<'a>(
    node: &'a ComponentValue<'a>,
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
) -> String {
    match node {
        ComponentValue::InterpolableStr(InterpolableStr::Literal(s)) => s.value.to_string(),
        _ => unquote(&eval(node, src, scope, depth)),
    }
}

/// Variable substitution inside interpolation (kept as-is when undefined).
fn interp_var<'a>(
    name: &str,
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
    span: Option<oxc_css_parser::pos::Span>,
) -> String {
    // The variable name itself may contain interpolation (`@{box-@{suffix}}`)
    let resolved = if name.contains("@{") {
        interpolate(name, src, scope, depth, span)
    } else {
        name.to_string()
    };
    if !plain_var_name(&resolved) {
        return format!("@{{{name}}}");
    }
    match scope.var(&resolved) {
        Some(VarRef::Ast(node)) => text_value(node, src, scope, depth + 1),
        Some(VarRef::List(v)) => v.join(" "),
        Some(VarRef::Text(t)) => unquote(&t),
        Some(VarRef::Color(c)) => c.to_css(),
        Some(VarRef::Ruleset(_)) => String::new(),
        None => {
            if let Some(span) = span {
                scope.var_miss(&resolved, span);
            }
            format!("@{{{name}}}")
        }
    }
}

/// Inside `@{}` is an ordinary variable name (less's name interpolation regex only accepts `[\w-]`).
/// Forms like `@{paths[dir]}` are emitted as raw text in less 4.1.3 (permissive contexts are not evaluated) and do not report undefined.
fn plain_var_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Interpolation in selectors/property names: the value is embedded as-is (quotes of a quoted value are kept, as in less).
fn interp_var_raw<'a>(
    name: &str,
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
    span: Option<oxc_css_parser::pos::Span>,
) -> String {
    let resolved = if name.contains("@{") {
        interpolate(name, src, scope, depth, span)
    } else {
        name.to_string()
    };
    if !plain_var_name(&resolved) {
        return format!("@{{{name}}}");
    }
    let text = match scope.var(&resolved) {
        // Name/selector interpolation happens in less's eval phase: numbers are not truncated by numPrecision here
        Some(VarRef::Ast(node)) => precise(|| eval(node, src, scope, depth + 1)),
        Some(VarRef::List(v)) => v.join(" "),
        Some(VarRef::Text(t)) => t,
        Some(VarRef::Color(c)) => c.to_css(),
        Some(VarRef::Ruleset(_)) => String::new(),
        None => {
            if let Some(span) = span {
                scope.var_miss(&resolved, span);
            }
            format!("@{{{name}}}")
        }
    };
    match scope.var_suffix(&resolved) {
        Some(suffix) => format!("{text} {suffix}"),
        None => text,
    }
}

/// Interpolated strings such as `"a@{n}b"`: static parts keep their source text (including quotes), variables/properties are substituted.
fn interpolated_str<'a>(
    elements: &[LessInterpolatedStrElement<'a>],
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
    span: oxc_css_parser::pos::Span,
) -> String {
    // After element substitution the text is interpolated again: `"@{box-@{suffix}}"` first becomes `@{box-large}`
    let joined: String = elements
        .iter()
        .map(|e| match e {
            LessInterpolatedStrElement::Static(p) => p.raw.to_string(),
            LessInterpolatedStrElement::Variable(v) => {
                interp_var(v.name.name, src, scope, depth, Some(span))
            }
            LessInterpolatedStrElement::Property(p) => prop_text(p.name.name, src, scope, depth)
                .unwrap_or_else(|| format!("${{{}}}", p.name.name)),
        })
        .collect();
    interpolate(&joined, src, scope, depth, Some(span))
}

/// `@{name}` / `${prop}` text interpolation (also used for selectors and property names).
///
/// less's `Quoted.eval` substitutes repeatedly until nothing changes; this loops the same way, with a limit against infinite loops.
pub(crate) fn interpolate<'a>(
    text: &str,
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
    span: Option<oxc_css_parser::pos::Span>,
) -> String {
    if !text.contains("@{") && !text.contains("${") {
        return text.to_string();
    }
    let mut out = text.to_string();
    for _ in 0..8 {
        let next = interpolate_once(&out, src, scope, depth, span);
        if next == out {
            break;
        }
        out = next;
    }
    out
}

fn interpolate_once<'a>(
    text: &str,
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
    span: Option<oxc_css_parser::pos::Span>,
) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some((pos, is_prop)) = next_marker(rest) {
        out.push_str(&rest[..pos]);
        let after = &rest[pos + 2..];
        // Names may nest `@{}` (`@{box-@{suffix}}`): pair brackets from the inside out
        let Some(end) = matching_brace(after) else {
            out.push_str(&rest[pos..]);
            return out;
        };
        let name = &after[..end];
        // Quote state must consider the whole prefix: inserted values shift the start of rest (`"a@{x}@{y}"`)
        let abs = text.len() - rest.len() + pos;
        let quoted = inside_quotes(&text[..abs]);
        out.push_str(&if is_prop {
            prop_text(name, src, scope, depth).unwrap_or_else(|| format!("${{{name}}}"))
        } else if quoted {
            interp_var(name, src, scope, depth, span)
        } else {
            interp_var_raw(name, src, scope, depth, span)
        });
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

/// Index after `@{` up to the matching `}` (accounting for nested `@{`).
fn matching_brace(text: &str) -> Option<usize> {
    let mut depth = 0usize;
    for (i, ch) in text.char_indices() {
        match ch {
            '{' => depth += 1,
            '}' if depth == 0 => return Some(i),
            '}' => depth -= 1,
            _ => {}
        }
    }
    None
}

/// Whether position `pos` lies inside a quoted string (skipping escapes).
fn inside_quotes(text: &str) -> bool {
    let mut quote = None;
    let mut escaped = false;
    for ch in text.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' => escaped = true,
            '"' | '\'' => match quote {
                Some(q) if q == ch => quote = None,
                Some(_) => {}
                None => quote = Some(ch),
            },
            _ => {}
        }
    }
    quote.is_some()
}

/// Returns the earliest interpolation marker position and whether it is `${prop}`.
fn next_marker(text: &str) -> Option<(usize, bool)> {
    match (text.find("@{"), text.find("${")) {
        (Some(a), Some(b)) if a <= b => Some((a, false)),
        (Some(_), Some(b)) => Some((b, true)),
        (Some(a), None) => Some((a, false)),
        (None, Some(b)) => Some((b, true)),
        (None, None) => None,
    }
}

/// When the preceding character is part of an identifier, `@` is not a variable reference (`url(user@example.com)`).
fn after_ident(prefix: &str) -> bool {
    prefix
        .chars()
        .next_back()
        .is_some_and(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.'))
}

/// Whether `@x` sits inside a `[...]` lookup name: less treats it as a namespace lookup name, not a variable reference.
fn inside_lookup(prefix: &str) -> bool {
    let (mut depth, mut quote) = (0i32, None);
    for c in prefix.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None => match c {
                '\'' | '"' => quote = Some(c),
                '[' => depth += 1,
                ']' => depth -= 1,
                _ => {}
            },
        }
    }
    depth > 0
}

/// `@name` / `@{name}` text substitution: used for at-rule preludes and inside `url()`.
///
/// Unlike selector interpolation, `@name` keeps the value's quotes (`url(@p)` -> `url("a.png")`).
/// An `@` inside quotes or in the middle of an identifier is not a variable reference (less treats it as plain text) and reports no error.
pub(crate) fn substitute<'a>(
    text: &str,
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
    span: Option<oxc_css_parser::pos::Span>,
) -> String {
    if !text.contains('@') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(pos) = rest.find('@') {
        out.push_str(&rest[..pos]);
        let abs = text.len() - rest.len() + pos;
        let prefix = &text[..abs];
        let after = &rest[pos + 1..];
        if let Some(name) = after.strip_prefix('{') {
            match name.find('}') {
                Some(end) => {
                    out.push_str(&interp_var(&name[..end], src, scope, depth, span));
                    rest = &name[end + 1..];
                }
                None => {
                    out.push_str(&rest[pos..]);
                    return out;
                }
            }
            continue;
        }
        let len = after
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
            .unwrap_or(after.len());
        if len == 0 {
            out.push('@');
            rest = after;
            continue;
        }
        let report = if inside_quotes(prefix) || after_ident(prefix) || inside_lookup(prefix) {
            None
        } else {
            span
        };
        out.push_str(&var_value(&after[..len], src, scope, depth, report));
        rest = &after[len..];
    }
    out.push_str(rest);
    out
}

/// `url(...)`: the inner text is evaluated as less does (interpolation/variables); bare values such as `data:` are kept as-is.
fn eval_url<'a>(
    u: &Url<'a>,
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
    span: Option<oxc_css_parser::pos::Span>,
) -> String {
    let raw = text_of(u.span, src);
    let (Some(open), Some(close)) = (raw.find('('), raw.rfind(')')) else {
        return raw.to_string();
    };
    let inner = &raw[open + 1..close];
    if !inner.contains('@') || inner.trim_start().starts_with("data:") {
        return raw.to_string();
    }
    format!(
        "{}({})",
        &raw[..open],
        substitute(inner, src, scope, depth, span)
    )
}

/// Property value text: returns the (merged) value on a hit, None when undefined.
fn prop_text<'a>(name: &str, src: &'a str, scope: &dyn Scope<'a>, depth: usize) -> Option<String> {
    let last = scope.prop(name)?.pop()?;
    Some(match last {
        PropSource::Decl(d) => with_important(eval_values(&d.value, src, scope, depth + 1), d),
        PropSource::Merged(groups) => groups
            .iter()
            .map(|g| {
                g.iter()
                    .map(|d| with_important(eval_values(&d.value, src, scope, depth + 1), d))
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect::<Vec<_>>()
            .join(", "),
    })
}

/// less's `$prop` carries the source declaration's `!important` to the reference site.
fn with_important(text: String, d: &Declaration) -> String {
    if d.important.is_some() {
        format!("{text} !important")
    } else {
        text
    }
}

/// A declaration's whole value: whitespace between segments normalized to a single space (less `Expression`'s joining).
pub(crate) fn eval_values<'a>(
    values: &[ComponentValue<'a>],
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
) -> String {
    join_seq(values, src, scope, depth)
}

/// Joining of a value/argument sequence: whitespace between segments normalized to a single space and commas to `, `.
fn join_seq<'a>(
    items: &[ComponentValue<'a>],
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
) -> String {
    let mut out = String::new();
    let mut prev_end: Option<usize> = None;
    let mut skip_gap = false;
    for v in items {
        let span = *v.span();
        let comma =
            matches!(v, ComponentValue::Delimiter(d) if matches!(d.kind, DelimiterKind::Comma));
        if let Some(end) = prev_end
            && end <= span.start
            && span.start <= src.len()
        {
            let gap = &src[end..span.start];
            if gap.contains("/*") {
                // A block comment between arguments is a value node (less `arguments()` treats a comment as an entity)
                if !skip_gap {
                    out.push(' ');
                }
                out.push_str(gap.trim());
                if !comma {
                    out.push(' ');
                }
            } else if !skip_gap && !comma {
                out.push_str(if gap.trim().is_empty() && !gap.is_empty() {
                    " "
                } else {
                    gap
                });
            }
        }
        if comma {
            out.push_str(", ");
        } else {
            out.push_str(&eval(v, src, scope, depth));
        }
        skip_gap = comma;
        prev_end = Some(span.end);
    }
    out
}

/// Evaluation inside parentheses: division is computed only here (less's `math: parens-division`).
/// less turns math fully on inside parentheses, so divisions in nested operations are computed too.
fn eval_paren<'a>(
    cv: &ComponentValue<'a>,
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
) -> String {
    if depth > 32 {
        return eval(cv, src, scope, depth);
    }
    match cv {
        ComponentValue::LessBinaryOperation(op) => {
            // Compute f64 directly in the numeric domain: going through text (8 decimals) loses precision
            if let Some((v, u)) = number_in(cv, src, scope, depth + 1, true) {
                return fmt_with_unit(v, u);
            }
            let left = eval_paren(&op.left, src, scope, depth + 1);
            let right = eval_paren(&op.right, src, scope, depth + 1);
            operate(&op.op.kind, &left, &right).unwrap_or_else(|| format!("{left} {right}"))
        }
        ComponentValue::LessParenthesizedOperation(p) => {
            eval_paren(&p.operation, src, scope, depth + 1)
        }
        // Variable references inside parentheses are evaluated with "math on": `@b: 18 / 2` in `(@b)` must compute to 9
        ComponentValue::LessVariable(v) => {
            paren_var(v.name.name, src, scope, depth, Some(*cv.span()))
        }
        ComponentValue::LessVariableCall(c) => {
            paren_var(c.variable.name.name, src, scope, depth, Some(*cv.span()))
        }
        _ => eval(cv, src, scope, depth + 1),
    }
}

fn paren_var<'a>(
    name: &str,
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
    span: Option<oxc_css_parser::pos::Span>,
) -> String {
    match scope.var(name) {
        Some(VarRef::Ast(node)) => eval_paren(node, src, scope, depth + 1),
        Some(VarRef::List(v)) => v.join(" "),
        Some(VarRef::Text(t)) => t,
        Some(VarRef::Color(c)) => c.to_css(),
        Some(VarRef::Ruleset(_)) => String::new(),
        None => undefined_var(name, scope, span),
    }
}

/// Binary operations.
fn eval_binary<'a>(
    op: &LessBinaryOperation<'a>,
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
) -> String {
    let left_text = eval(&op.left, src, scope, depth + 1);
    let right_text = eval(&op.right, src, scope, depth + 1);

    // Division is not computed by default (math=parens-division) and is emitted as-is, keeping the whitespace around the operator
    if matches!(op.op.kind, LessOperationOperatorKind::Division) {
        let lo = op.left.span().end;
        let hi = op.right.span().start;
        let sep = if lo <= hi && hi <= src.len() {
            &src[lo..hi]
        } else {
            "/"
        };
        return format!("{left_text}{sep}{right_text}");
    }

    let (lo, hi) = (op.left.span().end, op.right.span().start);
    let gap = if lo <= hi && hi <= src.len() {
        &src[lo..hi]
    } else {
        ""
    };
    let op_text = match op.op.kind {
        LessOperationOperatorKind::Plus => "+",
        LessOperationOperatorKind::Minus => "-",
        LessOperationOperatorKind::Multiply => "*",
        LessOperationOperatorKind::Division | LessOperationOperatorKind::DotDivision => "/",
    };
    // less's `Operation.eval`: when an operand cannot be operated on (e.g. an unevaluated division), the whole operation is kept as-is
    let spaced = !gap.is_empty() && gap.starts_with(|c: char| c.is_whitespace());
    let fallback = if spaced {
        format!("{left_text} {op_text} {right_text}")
    } else {
        format!("{left_text}{op_text}{right_text}")
    };
    operate(&op.op.kind, &left_text, &right_text).unwrap_or(fallback)
}

/// Operand: a number with a unit, or a color (in less, a Dimension participating in color arithmetic becomes a grayscale color).
#[derive(Clone)]
enum Operand {
    Num(f64, Option<String>),
    Color(Color),
}

fn parse_operand(text: &str) -> Option<Operand> {
    if let Some(c) = parse_color(text) {
        return Some(Operand::Color(c));
    }
    // Only accept "number + unit" as a whole: the unit part of `10px / 2` is not a unit and must not be computed as a number
    parse_number(text)
        .filter(|(_, u)| u.as_deref().is_none_or(is_unit_name))
        .map(|(v, u)| Operand::Num(v, u))
}

fn is_unit_name(unit: &str) -> bool {
    unit.chars()
        .all(|c| c.is_alphabetic() || c == '%' || c == '_')
}

/// Same-group unit conversion (less `Dimension.convertTo`); returns the input as-is when not convertible.
fn convert_unit(value: f64, from: &str, to: &str) -> f64 {
    for (_, table) in UNIT_GROUPS {
        let f = table.iter().find(|(u, _)| *u == from).map(|(_, f)| *f);
        let t = table.iter().find(|(u, _)| *u == to).map(|(_, f)| *f);
        if let (Some(f), Some(t)) = (f, t) {
            return value * f / t;
        }
    }
    value
}

/// Arithmetic when both sides are already evaluated text (for namespace lookup results taking part in operations).
pub(crate) fn arithmetic(
    kind: &LessOperationOperatorKind,
    left: &str,
    right: &str,
) -> Option<String> {
    operate(kind, left, right)
}

fn operate(kind: &LessOperationOperatorKind, left: &str, right: &str) -> Option<String> {
    match (parse_operand(left)?, parse_operand(right)?) {
        (Operand::Num(lv, lu), Operand::Num(rv, ru)) => {
            let (v, unit) = match *kind {
                // less's +/-: the result takes the left unit; the right side is converted first when convertible
                LessOperationOperatorKind::Plus | LessOperationOperatorKind::Minus => {
                    let rv = match (lu.as_deref(), ru.as_deref()) {
                        (Some(l), Some(r)) => convert_unit(rv, r, l),
                        _ => rv,
                    };
                    (arith(kind, lv, rv), lu.or(ru))
                }
                _ => (arith(kind, lv, rv), lu.or(ru)),
            };
            Some(fmt_with_unit(v, unit))
        }
        (l, r) => {
            let (l, r) = (to_color(l), to_color(r));
            // less Color.operate: alpha follows the composition formula regardless of the operator
            let alpha = l.a * (1.0 - r.a) + r.a;
            Some(
                Color {
                    r: arith(kind, l.r, r.r),
                    g: arith(kind, l.g, r.g),
                    b: arith(kind, l.b, r.b),
                    a: alpha,
                }
                .to_css(),
            )
        }
    }
}

fn arith(kind: &LessOperationOperatorKind, a: f64, b: f64) -> f64 {
    match kind {
        LessOperationOperatorKind::Plus => a + b,
        LessOperationOperatorKind::Minus => a - b,
        LessOperationOperatorKind::Multiply => a * b,
        LessOperationOperatorKind::Division | LessOperationOperatorKind::DotDivision => a / b,
    }
}

fn to_color(op: Operand) -> Color {
    match op {
        Operand::Color(c) => c,
        // less Dimension.toColor: a number becomes a grayscale color
        Operand::Num(v, _) => Color {
            r: v,
            g: v,
            b: v,
            a: 1.0,
        },
    }
}

/// less `unit-conversions.js`: units within the same group are convertible.
const UNIT_GROUPS: &[(&str, &[(&str, f64)])] = &[
    (
        "length",
        &[
            ("m", 1.0),
            ("cm", 0.01),
            ("mm", 0.001),
            ("in", 0.0254),
            ("px", 0.0254 / 96.0),
            ("pt", 0.0254 / 72.0),
            ("pc", 0.0254 / 72.0 * 12.0),
        ],
    ),
    ("duration", &[("s", 1.0), ("ms", 0.001)]),
    (
        "angle",
        &[
            ("rad", 1.0 / (2.0 * std::f64::consts::PI)),
            ("deg", 1.0 / 360.0),
            ("grad", 1.0 / 400.0),
            ("turn", 1.0),
        ],
    ),
];

pub(super) fn eval_function_text<'a>(
    f: &Function<'a>,
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
) -> String {
    eval_function(f, src, scope, depth)
}

/// Unit conversion group: returns (group index, factor to the group's base unit).
pub(super) fn unit_factor(unit: &str) -> Option<(usize, f64)> {
    (0..UNIT_GROUPS.len()).find_map(|g| {
        UNIT_GROUPS[g]
            .1
            .iter()
            .find(|(u, _)| *u == unit)
            .map(|(_, f)| (g, *f))
    })
}

/// Function name and arguments; `if`/`boolean` arguments contain condition nodes and must be evaluated separately (only the selected branch).
/// The IE filter function's `(path, inside parentheses)`: `progid:DXImageTransform.Microsoft.f(x=1)` ->
/// `("progid:DXImageTransform.Microsoft.f", "x=1")`.
fn progid_parts<'a>(f: &Function<'a>, src: &'a str) -> Option<(&'a str, &'a str)> {
    let raw = src.get(f.span.start..f.span.end)?;
    let head = raw.get(..raw.find('(')?)?;
    let tail = raw.get(head.len() + 1..)?;
    if !head.to_ascii_lowercase().starts_with("progid:") {
        return None;
    }
    let inner = tail.strip_suffix(')')?;
    Some((head, inner))
}

/// `key = value` -> `key=value` (untouched inside quotes): less's `Assignment` output form.
fn tighten_assignments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut quote = None;
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match quote {
            Some(q) => {
                out.push(ch);
                if ch == q {
                    quote = None;
                }
            }
            None => match ch {
                '"' | '\'' => {
                    quote = Some(ch);
                    out.push(ch);
                }
                '=' => {
                    while out.ends_with(' ') {
                        out.pop();
                    }
                    out.push('=');
                    while chars.peek() == Some(&' ') {
                        chars.next();
                    }
                }
                _ => out.push(ch),
            },
        }
    }
    out
}

/// Evaluation entry for a statement-level function call (root-level bare call `e('…');`).
pub(crate) fn eval_function_stmt<'a>(
    f: &'a Function<'a>,
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
) -> String {
    eval_function(f, src, scope, depth)
}

fn eval_function<'a>(
    f: &Function<'a>,
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
) -> String {
    let name = function_name(f);
    // IE filter `progid:DXImageTransform.Microsoft.f(opacity = 20)`: less treats the whole path as the function name,
    // and the arguments are `key=value` assignments (`Assignment` outputs `key=value` without spaces).
    if let Some((prefix, inner)) = progid_parts(f, src) {
        let inner = substitute(inner, src, scope, depth, Some(*f.span()));
        return format!("{prefix}({})", tighten_assignments(&inner));
    }
    // Probe functions inspect the node directly without evaluating arguments: `isdefined(@nope)` / `isruleset(@nope)` must not report errors.
    match name.as_str() {
        "isdefined" => {
            return is_css(f.args.first().is_some_and(|a| probe_defined(a, scope)));
        }
        "isruleset" => {
            return is_css(f.args.first().is_some_and(|a| probe_ruleset(a, scope)));
        }
        _ => {}
    }
    if matches!(name.as_str(), "if" | "boolean") {
        if let Some(out) = cond_function(&name, f, src, scope, depth) {
            return out;
        }
    }
    // A comma is a separate Delimiter node in the AST, not an argument: it must be filtered when taking arguments by position,
    // otherwise args[1] of `lighten(#333, 10%)` would be the comma.
    let args: Vec<String> = f
        .args
        .iter()
        .filter(|a| !matches!(a, ComponentValue::Delimiter(_)))
        .map(|a| eval(a, src, scope, depth + 1))
        .collect();
    let joined = join_args(&f.args, src, scope, depth);

    match name.as_str() {
        "lighten" | "darken" | "saturate" | "desaturate" | "spin" | "greyscale" | "fade"
        | "fadein" | "fadeout" | "mix" | "tint" | "shade" => color_function(f, src, scope, depth)
            .map(|c| c.to_css())
            .unwrap_or_else(|| format!("{name}({joined})")),
        // `rgba(color)` / `rgba(color, alpha)`: when the argument is a color only alpha is overridden (less `functions/color.js`)
        "rgba" | "rgb" | "hsl" | "hsla" => {
            let single = f
                .args
                .first()
                .and_then(|a| color_arg(a, src, scope, depth))
                .filter(|_| args.len() <= 2);
            match (single, name.as_str()) {
                (Some(c), "rgba" | "rgb") => {
                    let a = args.get(1).and_then(|a| color_number(a)).unwrap_or(c.a);
                    Color { a, ..c }.to_css()
                }
                (Some(c), _) => {
                    let (h, s, l) = c.to_hsl();
                    let a = args.get(1).and_then(|a| color_number(a)).unwrap_or(c.a);
                    hsl_css(h, s, l, a)
                }
                _ => {
                    let text = match name.as_str() {
                        "hsl" | "hsla" => hsl_text(&args),
                        _ => parse_color(&format!("{name}({joined})")).map(|c| c.to_css()),
                    };
                    text.unwrap_or_else(|| format!("{name}({joined})"))
                }
            }
        }
        // Color channels/properties (less `functions/color.js`)
        "red" | "green" | "blue" => {
            let c = parse_color(arg_at(&args, 0));
            let v = c.map(|c| match name.as_str() {
                "red" => c.r,
                "green" => c.g,
                _ => c.b,
            });
            v.map(fmt_number)
                .unwrap_or_else(|| format!("{name}({joined})"))
        }
        "alpha" => parse_color(arg_at(&args, 0))
            .map(|c| fmt_number(c.a))
            .unwrap_or_else(|| format!("{name}({joined})")),
        "hue" | "saturation" | "lightness" => parse_color(arg_at(&args, 0))
            .map(|c| {
                let (h, s, l) = c.to_hsl();
                match name.as_str() {
                    "hue" => fmt_number(h),
                    "saturation" => format!("{}%", fmt_number(s * 100.0)),
                    _ => format!("{}%", fmt_number(l * 100.0)),
                }
            })
            .unwrap_or_else(|| format!("{name}({joined})")),
        "argb" => parse_color(arg_at(&args, 0))
            .map(|c| c.to_argb())
            .unwrap_or_else(|| format!("{name}({joined})")),
        "percentage" => number_arg(&f.args, src, scope, depth)
            .map(|(v, u)| {
                let v = if u.as_deref() == Some("%") {
                    v / 100.0
                } else {
                    v
                };
                format!("{}%", fmt_number(v * 100.0))
            })
            .unwrap_or_else(|| format!("{name}({joined})")),
        "unit" => {
            let v = args.first().cloned().unwrap_or_default();
            let unit = args.get(1).cloned().unwrap_or_default();
            let unit = unit.trim_matches('"').trim_matches('\'');
            match parse_number(&v) {
                Some((n, _)) => format!("{}{unit}", fmt_number(n)),
                None => v,
            }
        }
        "round" | "ceil" | "floor" | "abs" => {
            let v = number_arg(&f.args, src, scope, depth);
            // `round(n, precision)`: less goes through `toFixed`, converting back to a number after the decimal places
            let digits =
                number_arg_at(&f.args, 1, src, scope, depth).map(|(d, _)| d.max(0.0) as usize);
            match (v, name.as_str()) {
                (Some((n, u)), "round") => fmt_with_unit(fixed(n, digits.unwrap_or(0)), u),
                (Some((n, u)), "ceil") => fmt_with_unit(n.ceil(), u),
                (Some((n, u)), "floor") => fmt_with_unit(n.floor(), u),
                (Some((n, u)), "abs") => fmt_with_unit(n.abs(), u),
                _ => format!("{name}({joined})"),
            }
        }
        // ---- Lists (less `functions/list.js`) ----
        "length" => fmt_number(list_items(f, src, scope, depth).len() as f64),
        "extract" => {
            let items = list_items(f, src, scope, depth);
            match extract_index(f, src, scope, depth) {
                Some(i) if i < items.len() => items[i].clone(),
                _ => format!("{name}({joined})"),
            }
        }
        "range" => range_list(f, src, scope, depth).unwrap_or_else(|| format!("{name}({joined})")),
        // ---- Math (less `functions/math.js`, `functions/number.js`) ----
        "pi" => fmt_number(std::f64::consts::PI),
        "sin" | "cos" | "tan" | "asin" | "acos" | "atan" => {
            math_fn(&name, f, src, scope, depth).unwrap_or_else(|| format!("{name}({joined})"))
        }
        "pow" | "sqrt" | "mod" => {
            number_fn(&name, f, src, scope, depth).unwrap_or_else(|| format!("{name}({joined})"))
        }
        "min" | "max" => min_max(f, name == "min", src, scope, depth)
            .unwrap_or_else(|| format!("{name}({joined})")),
        "convert" => {
            convert_fn(f, src, scope, depth).unwrap_or_else(|| format!("{name}({joined})"))
        }
        "get-unit" => match number_arg(&f.args, src, scope, depth) {
            Some((_, u)) => u.unwrap_or_default(),
            None => format!("{name}({joined})"),
        },
        // ---- Color channels and derivatives (less `functions/color.js`) ----
        "luma" | "luminance" => parse_color(arg_at(&args, 0))
            .map(|c| {
                let v = match name.as_str() {
                    "luma" => c.luma(),
                    _ => c.luminance(),
                };
                format!("{}%", fmt_number(v * c.a * 100.0))
            })
            .unwrap_or_else(|| format!("{name}({joined})")),
        "hsvhue" | "hsvsaturation" | "hsvvalue" => parse_color(arg_at(&args, 0))
            .map(|c| {
                let (h, s, v) = c.to_hsv();
                match name.as_str() {
                    "hsvhue" => fmt_number(h),
                    "hsvsaturation" => format!("{}%", fmt_number(s * 100.0)),
                    _ => format!("{}%", fmt_number(v * 100.0)),
                }
            })
            .unwrap_or_else(|| format!("{name}({joined})")),
        "hsv" | "hsva" => {
            let number = |i: usize| args.get(i).and_then(|a| color_number(a));
            match (number(0), number(1), number(2)) {
                (Some(h), Some(s), Some(v)) => {
                    let a = number(3).unwrap_or(1.0);
                    hsv_color(h, s, v, a).to_css()
                }
                _ => format!("{name}({joined})"),
            }
        }
        "contrast" => parse_color(arg_at(&args, 0))
            .map(|c| contrast_color(c, &args))
            .unwrap_or_else(|| format!("{name}({joined})")),
        // `color('#55FF5599')`: a hex value inside quotes is handed back as-is (case preserved);
        // color keywords fall back to `#rrggbb` (less clears the Color's `value`)
        "color" => {
            let a = arg_at(&args, 0);
            let inner = unquote(a);
            let hex = inner.strip_prefix('#').filter(|h| {
                matches!(h.len(), 3 | 4 | 6 | 8) && h.chars().all(|c| c.is_ascii_hexdigit())
            });
            match hex {
                Some(_) => inner,
                None => parse_color(a)
                    .map(|c| c.to_css())
                    .unwrap_or_else(|| format!("{name}({joined})")),
            }
        }
        // Color blending (less `functions/color-blending.js`)
        "multiply" | "screen" | "overlay" | "softlight" | "hardlight" | "difference"
        | "exclusion" | "average" | "negation" => {
            let (Some(a), Some(b)) = (parse_color(arg_at(&args, 0)), parse_color(arg_at(&args, 1)))
            else {
                return format!("{name}({joined})");
            };
            blend(&name, a, b).to_css()
        }
        "replace" => {
            replace_fn(&f.args, src, scope, depth).unwrap_or_else(|| format!("{name}({joined})"))
        }
        // `calc(...)`: no arithmetic inside, only variables and function results are written back
        "calc" => calc_text(f, src, scope, depth),
        // Type checks (less `functions/types.js`)
        "isnumber" => is_css(parse_number(arg_at(&args, 0)).is_some()),
        "iscolor" => is_css(is_color_text(arg_at(&args, 0))),
        "isstring" => is_css({
            let a = arg_at(&args, 0);
            a.len() >= 2 && (a.starts_with('"') || a.starts_with('\''))
        }),
        "iskeyword" => is_css({
            let a = arg_at(&args, 0);
            !a.is_empty()
                && parse_number(a).is_none()
                && !is_color_text(a)
                && !a.starts_with(['"', '\''])
        }),
        "isurl" => is_css(arg_at(&args, 0).starts_with("url(")),
        "ispixel" => is_css(unit_of(arg_at(&args, 0)) == Some("px".to_string())),
        "ispercentage" => is_css(unit_of(arg_at(&args, 0)).as_deref() == Some("%")),
        "isem" => is_css(unit_of(arg_at(&args, 0)).as_deref() == Some("em")),
        "isunit" => is_css({
            let unit = arg_at(&args, 1);
            let unit = unit.trim_matches(['"', '\'']);
            unit_of(arg_at(&args, 0)).as_deref() == Some(unit) && !unit.is_empty()
        }),
        // `e("...")`: an anonymous value with the quotes removed (less `Anonymous`)
        "e" => args
            .first()
            .map(|a| a.trim_matches(['"', '\'']).to_string())
            .unwrap_or_default(),
        // `%("format", args...)`: less `format` (`%s %d %a`)
        "%" => format_string(&args).unwrap_or_else(|| format!("{name}({joined})")),
        // `escape(str)`: less `encodeURIComponent` plus extra escaping of `= : # ; ( )`
        "escape" => args
            .first()
            .map(|a| escape_url(&unquote(a)))
            .unwrap_or_else(|| format!("{name}({joined})")),
        // File functions: `data-uri` / `image-size` / `image-width` / `image-height`
        "data-uri" => {
            let args = arg_list(f, src, scope, depth);
            scope
                .file_roots()
                .and_then(|(dir, paths)| super::file_fn::data_uri(dir, paths, &args))
                .unwrap_or_else(|| format!("{name}({joined})"))
        }
        "image-size" => image_size(f, src, scope, depth)
            .map(|s| format!("{}px {}px", s.width, s.height))
            .unwrap_or_else(|| format!("{name}({joined})")),
        "image-width" => image_size(f, src, scope, depth)
            .map(|s| format!("{}px", s.width))
            .unwrap_or_else(|| format!("{name}({joined})")),
        "image-height" => image_size(f, src, scope, depth)
            .map(|s| format!("{}px", s.height))
            .unwrap_or_else(|| format!("{name}({joined})")),
        "svg-gradient" => super::file_fn::svg_gradient(&arg_list(f, src, scope, depth))
            .unwrap_or_else(|| format!("{name}({joined})")),
        // Unknown function: keep the original name and the evaluated arguments
        _ => format!("{name}({joined})"),
    }
}

fn arg_at(args: &[String], i: usize) -> &str {
    args.get(i).map(String::as_str).unwrap_or("")
}

/// The `image-size` family: less throws when the file is missing or the format is unknown; here the original call text is kept.
fn image_size<'a>(
    f: &Function<'a>,
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
) -> Option<super::file_fn::Size> {
    let (dir, paths) = scope.file_roots()?;
    super::file_fn::image_size(dir, paths, arg_list(f, src, scope, depth).first()?)
}

/// Arguments are grouped as less's `parser.arguments` does (comma-separated, space-separated within a group), but each group is joined
/// back into one text after evaluation: the direction in `svg-gradient(to bottom, black, white)` is the single group `to bottom`.
fn arg_list<'a>(
    f: &Function<'a>,
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
) -> Vec<String> {
    fn_arg_groups(f)
        .iter()
        .map(|group| {
            group
                .iter()
                .map(|cv| eval(cv, src, scope, depth + 1))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect()
}

fn unit_of(text: &str) -> Option<String> {
    parse_number(text).and_then(|(_, u)| u)
}

// ---------------------------------------------------------------------------
// List functions (less `functions/list.js`)
// ---------------------------------------------------------------------------

/// Argument grouping: less's `parser.arguments`: when a semicolon is present only semicolons split (commas stay in the group),
/// otherwise commas split; within a group values are space-separated.
pub(super) fn arg_groups<'a, 'b>(
    args: &'b [ComponentValue<'a>],
) -> Vec<Vec<&'b ComponentValue<'a>>> {
    // less: the second argument of `each(10px 15px, 20px 25px; { ... })` is a ruleset,
    // so once a semicolon appears commas no longer separate arguments
    let semi = args.iter().any(|a| {
        matches!(
            a,
            ComponentValue::Delimiter(d) if matches!(d.kind, DelimiterKind::Semicolon)
        )
    });
    let mut groups: Vec<Vec<&ComponentValue>> = vec![Vec::new()];
    for a in args {
        let split = matches!(
            a,
            ComponentValue::Delimiter(d)
                if if semi {
                    matches!(d.kind, DelimiterKind::Semicolon)
                } else {
                    matches!(d.kind, DelimiterKind::Comma)
                }
        );
        if split {
            groups.push(Vec::new());
        } else if let Some(last) = groups.last_mut() {
            last.push(a);
        }
    }
    groups.retain(|g| !g.is_empty());
    groups
}

fn fn_arg_groups<'a, 'b>(f: &'b Function<'a>) -> Vec<Vec<&'b ComponentValue<'a>>> {
    arg_groups(&f.args)
}

/// Elements of `length`/`extract` (less `getItemsFromNode`): a list expands, anything else counts as one.
fn list_items<'a>(
    f: &Function<'a>,
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
) -> Vec<String> {
    let Some(group) = fn_arg_groups(f).into_iter().next() else {
        return Vec::new();
    };
    if group.len() > 1 {
        return group
            .iter()
            .map(|n| eval(n, src, scope, depth + 1))
            .collect();
    }
    match group.first() {
        Some(node) => node_items(node, src, scope, depth),
        None => Vec::new(),
    }
}

fn node_items<'a>(
    node: &ComponentValue<'a>,
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
) -> Vec<String> {
    if depth > 32 {
        return vec![eval(node, src, scope, depth)];
    }
    match node {
        ComponentValue::LessVariable(v) => var_items(v.name.name, src, scope, depth),
        ComponentValue::LessVariableCall(c) => var_items(c.variable.name.name, src, scope, depth),
        ComponentValue::LessList(l) => l
            .elements
            .iter()
            .map(|e| eval(e, src, scope, depth + 1))
            .collect(),
        _ => {
            let text = eval(node, src, scope, depth);
            if text.is_empty() {
                vec![text]
            } else {
                split_items(&text)
            }
        }
    }
}

fn var_items<'a>(name: &str, src: &'a str, scope: &dyn Scope<'a>, depth: usize) -> Vec<String> {
    if depth > 32 {
        return vec![format!("@{name}")];
    }
    match scope.var(name) {
        Some(VarRef::Ast(n)) => node_items(n, src, scope, depth + 1),
        Some(VarRef::List(v)) => v.clone(),
        Some(VarRef::Text(t)) => split_items(&t),
        Some(VarRef::Color(c)) => vec![c.to_css()],
        Some(VarRef::Ruleset(_)) => Vec::new(),
        None => vec![format!("@{name}")],
    }
}

/// A list in text form (globalVars, variadic bindings): split at top-level commas, then at top-level whitespace.
pub(super) fn split_items(text: &str) -> Vec<String> {
    let t = text.trim();
    if t.is_empty() {
        return Vec::new();
    }
    let commas = crate::text::split_top_level(t, b',');
    if commas.len() > 1 {
        return commas
            .into_iter()
            .map(|(s, e)| t[s..e].trim().to_string())
            .filter(|p| !p.is_empty())
            .collect();
    }
    split_ws(t)
}

/// Top-level whitespace splitting.
pub(super) fn split_ws(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut depth = 0usize;
    let mut quote = None;
    let mut escaped = false;
    for ch in text.chars() {
        if escaped {
            cur.push(ch);
            escaped = false;
            continue;
        }
        match ch {
            '\\' => {
                cur.push(ch);
                escaped = true;
            }
            '"' | '\'' if quote.is_none() => {
                quote = Some(ch);
                cur.push(ch);
            }
            c if Some(c) == quote => {
                quote = None;
                cur.push(ch);
            }
            '(' | '[' => {
                depth += 1;
                cur.push(ch);
            }
            ')' | ']' => {
                depth = depth.saturating_sub(1);
                cur.push(ch);
            }
            c if c.is_whitespace() && depth == 0 && quote.is_none() => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Index for `extract` (0-based); less returns undefined for a non-positive-integer index (the call is emitted verbatim).
fn extract_index<'a>(
    f: &Function<'a>,
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
) -> Option<usize> {
    let groups = fn_arg_groups(f);
    let node = groups.get(1)?.first()?;
    let (v, _) = number_in(node, src, scope, depth + 1, false)?;
    (v >= 1.0 && v.fract() == 0.0 && v < 1e9).then(|| v as usize - 1)
}

/// `range(start, end, step)`: less's `functions/list.js`, emitting a space-separated list.
fn range_list<'a>(
    f: &Function<'a>,
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
) -> Option<String> {
    let groups = fn_arg_groups(f);
    let num = |i: usize| -> Option<(f64, Option<String>)> {
        number_in(groups.get(i)?.first()?, src, scope, depth + 1, false)
    };
    let (from, to, step) = match groups.len() {
        1 => (1.0, num(0)?.0, 1.0),
        2 => (num(0)?.0, num(1)?.0, 1.0),
        _ => (num(0)?.0, num(1)?.0, num(2)?.0),
    };
    let unit = num(groups.len().saturating_sub(1))?.1;
    if step == 0.0 {
        return None;
    }
    let mut out: Vec<String> = Vec::new();
    let mut v = from;
    while (step > 0.0 && v <= to) || (step < 0.0 && v >= to) {
        out.push(fmt_with_unit(v, unit.clone()));
        if out.len() > 10_000 {
            return None;
        }
        v += step;
    }
    if out.is_empty() {
        return None;
    }
    Some(out.join(" "))
}

// ---------------------------------------------------------------------------
// Math functions (less `functions/math.js`, `functions/number.js`)
// ---------------------------------------------------------------------------

/// `Dimension.unify()`: length -> px, duration -> s, angle -> rad (non-convertible units are kept).
fn unify(v: f64, unit: Option<String>) -> (f64, Option<String>) {
    let Some(u) = unit else {
        return (v, None);
    };
    let Some((group, from)) = unit_factor(&u) else {
        return (v, Some(u));
    };
    let target = ["px", "s", "rad"].get(group).copied().unwrap_or("px");
    match unit_factor(target) {
        Some((_, to)) => (v * from / to, Some(target.to_string())),
        None => (v, Some(u)),
    }
}

/// less's `round(n, d)` goes through `toFixed`: round half away from zero.
fn fixed(v: f64, digits: usize) -> f64 {
    let scale = 10f64.powi(digits as i32);
    (v * scale).round() / scale
}

/// `sin`/`cos`/`tan` (unitless results) and `asin`/`acos`/`atan` (results in rad).
fn math_fn<'a>(
    name: &str,
    f: &Function<'a>,
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
) -> Option<String> {
    let (v, u) = number_arg(&f.args, src, scope, depth)?;
    let (v, _) = unify(v, u);
    let out = match name {
        "sin" => v.sin(),
        "cos" => v.cos(),
        "tan" => v.tan(),
        "asin" => v.asin(),
        "acos" => v.acos(),
        _ => v.atan(),
    };
    let unit = if name.starts_with('a') { "rad" } else { "" };
    Some(fmt_with_unit(out, Some(unit.to_string())))
}

/// `pow`/`sqrt`/`mod` (less `functions/number.js`).
fn number_fn<'a>(
    name: &str,
    f: &Function<'a>,
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
) -> Option<String> {
    let (a, au) = number_arg(&f.args, src, scope, depth)?;
    let out = match name {
        "sqrt" => (a.sqrt(), au),
        _ => {
            let (b, _) = number_arg_at(&f.args, 1, src, scope, depth)?;
            match name {
                "pow" => (a.powf(b), au),
                _ => (a % b, au),
            }
        }
    };
    Some(fmt_with_unit(out.0, out.1))
}

/// Numeric value of the `i`-th argument (skipping delimiters).
fn number_arg_at<'a>(
    args: &[ComponentValue<'a>],
    i: usize,
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
) -> Option<(f64, Option<String>)> {
    let node = args
        .iter()
        .filter(|a| !matches!(a, ComponentValue::Delimiter(_)))
        .nth(i)?;
    number_in(node, src, scope, depth, false)
}

/// `min`/`max`: pick the extreme among same (convertible) units; with multiple non-convertible units less emits them as-is.
fn min_max<'a>(
    f: &Function<'a>,
    is_min: bool,
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
) -> Option<String> {
    let mut cands: Vec<(f64, Option<String>)> = Vec::new();
    for group in fn_arg_groups(f) {
        for node in group {
            cands.push(number_in(node, src, scope, depth + 1, false)?);
        }
    }
    if cands.is_empty() {
        return None;
    }
    // less treats a unitless argument as having the unit of the "first argument with a unit"
    let unit_static = cands.iter().find_map(|c| c.1.clone());
    let unified: Vec<(f64, Option<String>)> = cands
        .iter()
        .map(|c| unify(c.0, c.1.clone().or_else(|| unit_static.clone())))
        .collect();
    let units: Vec<&String> = unified.iter().filter_map(|u| u.1.as_ref()).collect();
    if units.windows(2).any(|w| w[0] != w[1]) {
        return None;
    }
    let mut best = 0;
    for i in 1..unified.len() {
        let better = if is_min {
            unified[i].0 < unified[best].0
        } else {
            unified[i].0 > unified[best].0
        };
        if better {
            best = i;
        }
    }
    // Emit the original argument (less takes `order[j].toCSS()`, keeping its own unit)
    let (v, u) = &cands[best];
    Some(fmt_with_unit(*v, u.clone()))
}

/// `convert(value, unit)`: same-group unit conversion (less `Dimension.convertTo`).
fn convert_fn<'a>(
    f: &Function<'a>,
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
) -> Option<String> {
    let (v, u) = number_arg(&f.args, src, scope, depth)?;
    let target = unquote(&eval(f.args.last()?, src, scope, depth + 1));
    let (group, from) = unit_factor(u.as_deref()?)?;
    let (target_group, to) = unit_factor(&target)?;
    // No conversion across groups (less's `convertTo` only handles the matching group)
    let v = if group == target_group {
        v * from / to
    } else {
        v
    };
    Some(fmt_with_unit(v, Some(target)))
}

/// Color blend modes (less `functions/color-blending.js`, W3C compositing formulas).
fn blend(mode: &str, backdrop: Color, source: Color) -> Color {
    let f = |cb: f64, cs: f64| match mode {
        "multiply" => cb * cs,
        "screen" => cb + cs - cb * cs,
        "overlay" => {
            let cb = cb * 2.0;
            if cb <= 1.0 {
                cb * cs
            } else {
                let (cb, cs) = (cb - 1.0, cs);
                cb + cs - cb * cs
            }
        }
        "softlight" => {
            let (d, e) = if cs > 0.5 {
                let d = if cb > 0.25 {
                    cb.sqrt()
                } else {
                    ((16.0 * cb - 12.0) * cb + 4.0) * cb
                };
                (d, 1.0)
            } else {
                (1.0, cb)
            };
            cb - (1.0 - 2.0 * cs) * e * (d - cb)
        }
        // hardlight is just overlay with the arguments swapped: overlay(cs, cb)
        "hardlight" => {
            let cs2 = cs * 2.0;
            if cs2 <= 1.0 {
                cs2 * cb
            } else {
                let (cs2, cb) = (cs2 - 1.0, cb);
                cs2 + cb - cs2 * cb
            }
        }
        "difference" => (cb - cs).abs(),
        "exclusion" => cb + cs - 2.0 * cb * cs,
        "average" => (cb + cs) / 2.0,
        _ => 1.0 - (cb + cs - 1.0).abs(),
    };
    let (ab, as_) = (backdrop.a, source.a);
    let ar = as_ + ab * (1.0 - as_);
    let mut ch = [0.0f64; 3];
    for (i, slot) in ch.iter_mut().enumerate() {
        let cb = [backdrop.r, backdrop.g, backdrop.b][i] / 255.0;
        let cs = [source.r, source.g, source.b][i] / 255.0;
        let mut cr = f(cb, cs);
        if ar != 0.0 {
            cr = (as_ * cs + ab * (cb - as_ * (cb + cs - cr))) / ar;
        }
        *slot = cr * 255.0;
    }
    Color {
        r: ch[0],
        g: ch[1],
        b: ch[2],
        a: ar,
    }
}

/// `hsv(h, s, v, a)`: less's perm table implementation.
fn hsv_color(h: f64, s: f64, v: f64, a: f64) -> Color {
    let h = ((h % 360.0) / 360.0) * 360.0;
    let i = ((h / 60.0) % 6.0).floor();
    let f = h / 60.0 - i;
    let vs = [
        v,
        v * (1.0 - s),
        v * (1.0 - f * s),
        v * (1.0 - (1.0 - f) * s),
    ];
    let perm = [
        [0, 3, 1],
        [2, 0, 1],
        [1, 0, 3],
        [1, 2, 0],
        [3, 1, 0],
        [0, 1, 2],
    ];
    let p = perm[(i as usize).min(5)];
    Color {
        r: vs[p[0]] * 255.0,
        g: vs[p[1]] * 255.0,
        b: vs[p[2]] * 255.0,
        a,
    }
}

/// `contrast(color[, dark, light, threshold])`: picks dark or light by luma.
///
/// Returns the **raw text** of the selected argument: less hands back that Color node, so `#eee` is not lengthened to `#eeeeee`.
fn contrast_color(c: Color, args: &[String]) -> String {
    let pick = |i: usize, fallback: &str| -> (String, Color) {
        match args.get(i).and_then(|a| parse_color(a)) {
            Some(color) => (args[i].clone(), color),
            None => (
                fallback.to_string(),
                parse_color(fallback).expect("valid fallback"),
            ),
        }
    };
    let (mut dark_text, mut dark) = pick(1, "#000000");
    let (mut light_text, mut light) = pick(2, "#ffffff");
    if dark.luma() > light.luma() {
        std::mem::swap(&mut dark, &mut light);
        std::mem::swap(&mut dark_text, &mut light_text);
    }
    let threshold = args.get(3).and_then(|a| color_number(a)).unwrap_or(0.43);
    if c.luma() < threshold {
        light_text
    } else {
        dark_text
    }
}

/// `replace(string, pattern, replacement, flags)`: literal patterns only (no regex metacharacters).
fn replace_fn<'a>(
    args: &[ComponentValue<'a>],
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
) -> Option<String> {
    let groups = arg_groups(args);
    let subject = groups.first()?.first()?;
    let raw = eval(subject, src, scope, depth + 1);
    let quote = match subject {
        ComponentValue::LessEscapedStr(_) => None,
        _ => raw.chars().next().filter(|c| *c == '"' || *c == '\''),
    };
    let body = unquote(&raw);
    let pattern = unquote(&eval(groups.get(1)?.first()?, src, scope, depth + 1));
    let replacement = unquote(&eval(groups.get(2)?.first()?, src, scope, depth + 1));
    let flags = groups
        .get(3)
        .and_then(|g| g.first())
        .map(|n| unquote(&eval(n, src, scope, depth + 1)))
        .unwrap_or_default();
    // JavaScript regex syntax is out of scope: keep the call as-is when metacharacters are present
    if pattern.is_empty() || pattern.chars().any(|c| "\\^$.|?*+()[]{}".contains(c)) {
        return None;
    }
    let global = flags.contains('g');
    let folded = body.to_lowercase();
    let needle = if flags.contains('i') {
        pattern.to_lowercase()
    } else {
        pattern.clone()
    };
    let haystack = if flags.contains('i') { &folded } else { &body };
    let mut out = String::new();
    let mut rest = 0usize;
    while let Some(pos) = haystack[rest..].find(&needle) {
        let at = rest + pos;
        out.push_str(&body[rest..at]);
        out.push_str(&replacement);
        rest = at + needle.len();
        if !global {
            break;
        }
    }
    out.push_str(&body[rest..]);
    Some(match quote {
        Some(q) => format!("{q}{out}{q}"),
        None => out,
    })
}

/// `boolean(cond)` / `if(cond, a, b)`: evaluates only the selected branch.
fn cond_function<'a>(
    name: &str,
    f: &Function<'a>,
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
) -> Option<String> {
    let groups = fn_arg_groups(f);
    let node = groups.first()?.first()?;
    let ComponentValue::LessCondition(cond) = node else {
        return None;
    };
    let params = std::collections::HashMap::new();
    let ctx = super::guard::Ctx {
        src,
        scope,
        params: &params,
        default: true,
    };
    let ok = super::guard::eval_single(cond, &ctx);
    if name == "boolean" {
        return Some(is_css(ok));
    }
    let branch = groups.get(if ok { 1 } else { 2 })?;
    Some(
        branch
            .iter()
            .map(|n| eval(n, src, scope, depth + 1))
            .collect::<Vec<_>>()
            .join(" "),
    )
}

// ---------------------------------------------------------------------------
// `calc()` (less `tree/call.js`): math is turned off inside calc, only variables and function results are written back
// ---------------------------------------------------------------------------

/// Evaluation result inside calc: a number (parentheses can be dropped) or something else (parentheses kept).
enum CalcOut {
    Dim(String),
    Other(String),
}

impl CalcOut {
    fn text(&self) -> &str {
        match self {
            CalcOut::Dim(t) | CalcOut::Other(t) => t,
        }
    }
}

fn calc_text<'a>(f: &Function<'a>, src: &'a str, scope: &dyn Scope<'a>, depth: usize) -> String {
    let mut out: Vec<String> = Vec::new();
    for group in fn_arg_groups(f) {
        let mut parts: Vec<String> = Vec::new();
        for node in group {
            parts.push(calc_top(node, src, scope, depth + 1).text().to_string());
        }
        out.push(parts.join(" "));
    }
    format!("calc({})", out.join(", "))
}

/// Top-level arguments of calc: all parentheses are stripped (less's Expression flattens single values).
fn calc_top<'a>(
    cv: &ComponentValue<'a>,
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
) -> CalcOut {
    match cv {
        ComponentValue::CalcParenthesized(p) => calc_top(&p.expr, src, scope, depth + 1),
        _ => calc_value(cv, src, scope, depth + 1),
    }
}

/// Operand of an operation: keep one layer of parentheses when it has them and the result is not a number.
fn calc_operand<'a>(
    cv: &ComponentValue<'a>,
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
) -> CalcOut {
    if !matches!(cv, ComponentValue::CalcParenthesized(_)) {
        return calc_value(cv, src, scope, depth + 1);
    }
    match calc_top(cv, src, scope, depth + 1) {
        CalcOut::Dim(t) => CalcOut::Dim(t),
        CalcOut::Other(t) => CalcOut::Other(format!("({t})")),
    }
}

fn calc_value<'a>(
    cv: &ComponentValue<'a>,
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
) -> CalcOut {
    if depth > 32 {
        return CalcOut::Other(eval(cv, src, scope, depth));
    }
    match cv {
        ComponentValue::Calc(c) => {
            let left = calc_operand(&c.left, src, scope, depth + 1);
            let right = calc_operand(&c.right, src, scope, depth + 1);
            CalcOut::Other(format!(
                "{}{}{}",
                left.text(),
                calc_sep(&c.left, &c.right, &c.op.kind, src),
                right.text()
            ))
        }
        ComponentValue::Number(_)
        | ComponentValue::Dimension(_)
        | ComponentValue::Percentage(_) => CalcOut::Dim(eval(cv, src, scope, depth + 1)),
        ComponentValue::LessVariable(v) => calc_var(v.name.name, src, scope, depth, *cv.span()),
        ComponentValue::LessVariableCall(c) => {
            calc_var(c.variable.name.name, src, scope, depth, *cv.span())
        }
        ComponentValue::Function(f) => {
            let text = eval_function(f, src, scope, depth + 1);
            if function_name(f) == "calc" || parse_number(&text).is_none() {
                CalcOut::Other(text)
            } else {
                CalcOut::Dim(text)
            }
        }
        _ => CalcOut::Other(eval(cv, src, scope, depth + 1)),
    }
}

/// A variable inside calc: evaluated first with `math: always` (less wraps it in `_SELF` for cascading evaluation).
fn calc_var<'a>(
    name: &str,
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
    span: oxc_css_parser::pos::Span,
) -> CalcOut {
    let numeric = match scope.var(name) {
        Some(VarRef::Ast(n)) => number_in(n, src, scope, depth + 1, true),
        Some(VarRef::List(v)) => parse_number(&v.join(" ")),
        Some(VarRef::Text(t)) => parse_number(&t),
        _ => None,
    };
    match numeric {
        Some((v, u)) => CalcOut::Dim(fmt_with_unit(v, u)),
        None => CalcOut::Other(var_value(name, src, scope, depth, Some(span))),
    }
}

/// Whitespace around the operator: less's `Operation.isSpaced` only looks at the whitespace before the operator.
fn calc_sep(
    left: &ComponentValue<'_>,
    right: &ComponentValue<'_>,
    kind: &CalcOperatorKind,
    src: &str,
) -> String {
    let op = match kind {
        CalcOperatorKind::Plus => "+",
        CalcOperatorKind::Minus => "-",
        CalcOperatorKind::Multiply => "*",
        CalcOperatorKind::Division => "/",
        CalcOperatorKind::Modulo => "%",
    };
    let (lo, hi) = (left.span().end, right.span().start);
    let gap = if lo <= hi && hi <= src.len() {
        &src[lo..hi]
    } else {
        ""
    };
    if gap.starts_with(|c: char| c.is_whitespace()) {
        format!(" {op} ")
    } else {
        op.to_string()
    }
}

/// Exact evaluation of numeric arguments: compute f64 directly, avoiding the precision loss of "format to 8 decimals and re-parse".
fn number_arg<'a>(
    args: &[ComponentValue<'a>],
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
) -> Option<(f64, Option<String>)> {
    number_in(args.first()?, src, scope, depth, false)
}

fn number_in<'a>(
    cv: &ComponentValue<'a>,
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
    paren: bool,
) -> Option<(f64, Option<String>)> {
    if depth > 32 {
        return None;
    }
    match cv {
        ComponentValue::Number(_)
        | ComponentValue::Dimension(_)
        | ComponentValue::Percentage(_) => parse_number(text(src, cv)),
        ComponentValue::LessVariable(v) => number_in_var(v.name.name, src, scope, depth, paren),
        ComponentValue::LessVariableCall(c) => {
            number_in_var(c.variable.name.name, src, scope, depth, paren)
        }
        ComponentValue::LessNegativeValue(n) => {
            number_in(&n.value, src, scope, depth + 1, paren).map(|(v, u)| (-v, u))
        }
        ComponentValue::LessPropertyVariable(p) => {
            prop_text(p.name.name, src, scope, depth + 1).and_then(|t| parse_number(&t))
        }
        ComponentValue::LessParenthesizedOperation(p) => {
            number_in(&p.operation, src, scope, depth + 1, true)
        }
        // Arguments of functions like min/max are calc trees: the "math off" semantics of calc() do not apply there
        ComponentValue::Calc(c) => {
            let (lv, lu) = number_in(&c.left, src, scope, depth + 1, paren)?;
            let (rv, ru) = number_in(&c.right, src, scope, depth + 1, paren)?;
            let rv = if matches!(c.op.kind, CalcOperatorKind::Plus | CalcOperatorKind::Minus) {
                match (lu.as_deref(), ru.as_deref()) {
                    (Some(l), Some(r)) => convert_unit(rv, r, l),
                    _ => rv,
                }
            } else {
                rv
            };
            let v = match c.op.kind {
                CalcOperatorKind::Plus => lv + rv,
                CalcOperatorKind::Minus => lv - rv,
                CalcOperatorKind::Multiply => lv * rv,
                CalcOperatorKind::Division => lv / rv,
                CalcOperatorKind::Modulo => lv % rv,
            };
            Some((v, lu.or(ru)))
        }
        ComponentValue::CalcParenthesized(p) => number_in(&p.expr, src, scope, depth + 1, paren),
        // Numeric functions nested in a variable value (`floor((@a * 1.15))`)
        ComponentValue::Function(f) => {
            let (v, u) = number_in(f.args.first()?, src, scope, depth + 1, true)?;
            match function_name(f).as_str() {
                "ceil" => Some((v.ceil(), u)),
                "floor" => Some((v.floor(), u)),
                "round" => Some((v.round(), u)),
                "abs" => Some((v.abs(), u)),
                "percentage" => {
                    let v = if u.as_deref() == Some("%") {
                        v
                    } else {
                        v * 100.0
                    };
                    Some((v, Some("%".to_string())))
                }
                "unit" => {
                    let unit = unquote(text(src, f.args.get(1)?));
                    Some((v, Some(unit)))
                }
                _ => None,
            }
        }
        // math=parens-division: division outside parentheses is not computed
        ComponentValue::LessBinaryOperation(op) => {
            if matches!(op.op.kind, LessOperationOperatorKind::Division) && !paren {
                return None;
            }
            let (lv, lu) = number_in(&op.left, src, scope, depth + 1, paren)?;
            let (rv, ru) = number_in(&op.right, src, scope, depth + 1, paren)?;
            // Same as `operate`: only +/- does same-group unit conversion (multiplication/division keep the left unit)
            let rv = match (lu.as_deref(), ru.as_deref()) {
                (Some(l), Some(r))
                    if matches!(
                        op.op.kind,
                        LessOperationOperatorKind::Plus | LessOperationOperatorKind::Minus
                    ) =>
                {
                    convert_unit(rv, r, l)
                }
                _ => rv,
            };
            Some((arith(&op.op.kind, lv, rv), lu.or(ru)))
        }
        _ => None,
    }
}

fn number_in_var<'a>(
    name: &str,
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
    paren: bool,
) -> Option<(f64, Option<String>)> {
    match scope.var(name) {
        Some(VarRef::Ast(node)) => number_in(node, src, scope, depth + 1, paren),
        Some(VarRef::List(v)) => parse_number(&v.join(" ")),
        Some(VarRef::Text(t)) => parse_number(&t),
        Some(VarRef::Color(_)) => None,
        Some(VarRef::Ruleset(_)) => None,
        None => None,
    }
}

fn is_color_text(text: &str) -> bool {
    let t = text.trim();
    parse_color(t).is_some()
}

fn is_css(v: bool) -> String {
    if v {
        "true".to_string()
    } else {
        "false".to_string()
    }
}

/// less `escape`: JS `encodeURIComponent` (keeping `A-Za-z0-9-_.!~*'()`),
/// then escaping `= : # ; ( )`.
fn escape_url(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for b in text.bytes() {
        let keep = b.is_ascii_alphanumeric()
            || matches!(b, b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'');
        if keep {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// less `%("format", args...)`: substitutes the first `%s`/`%d`/`%a` with each argument in turn, then turns `%%` back into `%`.
fn format_string(args: &[String]) -> Option<String> {
    let first = args.first()?;
    let quote = first.chars().next().filter(|c| *c == '"' || *c == '\'');
    let mut out = match quote {
        Some(_) => unquote(first),
        None => first.clone(),
    };
    for arg in &args[1..] {
        let Some(pos) = out.find('%').filter(|p| {
            out[*p..]
                .chars()
                .nth(1)
                .is_some_and(|c| matches!(c, 's' | 'd' | 'a' | 'S' | 'D' | 'A'))
        }) else {
            break;
        };
        let token = out[pos + 1..].chars().next()?;
        let value = if matches!(token, 's' | 'S') && arg.starts_with(['"', '\'']) {
            unquote(arg)
        } else {
            arg.clone()
        };
        out.replace_range(pos..pos + 2, &value);
    }
    Some(match quote {
        Some(q) => format!("{q}{}{q}", out.replace("%%", "%")),
        None => out.replace("%%", "%"),
    })
}

/// Function arguments joined by source span: commas and whitespace come from the source text (args may contain Delimiter nodes).
fn join_args<'a>(
    args: &[ComponentValue<'a>],
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
) -> String {
    join_seq(args, src, scope, depth)
}

pub(super) fn function_name(f: &Function) -> String {
    match &f.name {
        FunctionName::Ident(InterpolableIdent::Literal(ident)) => ident.name.to_string(),
        // `%("...", args)`: less's format function
        FunctionName::LessFormatFunction(_) => "%".to_string(),
        other => format!("{other:?}"),
    }
}

fn fmt_with_unit(v: f64, unit: Option<String>) -> String {
    match unit {
        Some(u) => format!("{}{u}", fmt_number(v)),
        None => fmt_number(v),
    }
}

// ---------------------------------------------------------------------------
// Numbers
// ---------------------------------------------------------------------------

/// `"10px"` -> `(10.0, Some("px"))`; `"0.5"` -> `(0.5, None)`; `"50%"` -> `(50.0, Some("%"))`.
pub(crate) fn parse_number(s: &str) -> Option<(f64, Option<String>)> {
    let s = s.trim();
    let b = s.as_bytes();
    let mut i = 0;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        i += 1;
    }
    let digits = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    if i < b.len() && b[i] == b'.' {
        i += 1;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
    }
    if i == digits {
        return None;
    }
    // Exponent part: `e`/`E` must be followed directly by a (possibly signed) number, otherwise it starts a unit (e.g. `em`)
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        let mut j = i + 1;
        if j < b.len() && (b[j] == b'+' || b[j] == b'-') {
            j += 1;
        }
        let exp = j;
        while j < b.len() && b[j].is_ascii_digit() {
            j += 1;
        }
        if j > exp {
            i = j;
        }
    }
    let (num, unit) = s.split_at(i);
    let v: f64 = num.parse().ok()?;
    let unit = unit.trim();
    Some((v, (!unit.is_empty()).then(|| unit.to_string())))
}

/// less `Node.fround`: `Number((value + 2e-16).toFixed(8))`; the bias keeps binary underflow from
/// pinning 8 decimals at a rounding boundary (`0.66666666` vs `0.66666667`).
fn round8(v: f64) -> f64 {
    ((v + 2e-16) * 1e8).round() / 1e8
}

thread_local! {
    /// Number formatting during the eval phase (selector/property-name interpolation): less uses `String(value)` in `eval`,
    /// and only `genCSS` is subject to `numPrecision: 8`.
    static PRECISE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Runs in the mode where numbers are not truncated to 8 decimals during the eval phase.
pub(crate) fn precise<R>(f: impl FnOnce() -> R) -> R {
    PRECISE.with(|p| {
        let old = p.replace(true);
        let out = f();
        p.set(old);
        out
    })
}

/// Shortest round-trip form of JS `Number.prototype.toString` (used during the eval phase).
fn fmt_number_exact(v: f64) -> String {
    if !v.is_finite() || v == 0.0 {
        return "0".to_string();
    }
    format!("{v}")
}

/// Approximation of JS `Number.prototype.toString`: integers have no `.0`, decimals have at most 8 places with trailing zeros removed.
pub(crate) fn fmt_number(v: f64) -> String {
    if PRECISE.with(|p| p.get()) {
        return fmt_number_exact(v);
    }
    if !v.is_finite() {
        return "0".to_string();
    }
    if v == 0.0 {
        return "0".to_string();
    }
    if v.fract() == 0.0 && v.abs() < 1e15 {
        return format!("{}", v as i64);
    }
    let mut s = format!("{:.8}", v + 2e-16);
    while s.ends_with('0') {
        s.pop();
    }
    if s.ends_with('.') {
        s.pop();
    }
    if s == "-0" {
        return "0".to_string();
    }
    if s.starts_with('.') {
        s.insert(0, '0');
    } else if s.starts_with("-.") {
        s.insert(1, '0');
    }
    s
}

/// Removes a matching pair of quotes around the value (used when a string variable takes part in interpolation/variable variables).
fn unquote(s: &str) -> String {
    unquote_ref(s).to_string()
}

pub(super) fn unquote_ref(s: &str) -> &str {
    for q in ['"', '\''] {
        if s.len() >= 2 && s.starts_with(q) && s.ends_with(q) {
            return &s[1..s.len() - 1];
        }
    }
    s
}

// ---------------------------------------------------------------------------
// Colors
// ---------------------------------------------------------------------------

/// A less color: channels are floating point. HSL->RGB does not round, rounding happens only at output (`Math.round`),
/// so chained calls such as `darken(darken(#428bca, 6.5%), 15%)` must keep intermediate precision.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Color {
    pub(crate) r: f64,
    pub(crate) g: f64,
    pub(crate) b: f64,
    pub(crate) a: f64,
}

impl Color {
    pub(crate) fn to_css(self) -> String {
        let ch = |v: f64| v.round().clamp(0.0, 255.0) as u8;
        let a = round8(self.a).clamp(0.0, 1.0);
        if a >= 1.0 {
            format!("#{:02x}{:02x}{:02x}", ch(self.r), ch(self.g), ch(self.b))
        } else {
            format!(
                "rgba({}, {}, {}, {})",
                ch(self.r),
                ch(self.g),
                ch(self.b),
                fmt_number(a)
            )
        }
    }

    /// less `Color.toHSL`: h in degrees, s/l in 0..1.
    pub(crate) fn to_hsl(self) -> (f64, f64, f64) {
        let (r, g, b) = (self.r / 255.0, self.g / 255.0, self.b / 255.0);
        let (max, min) = (r.max(g).max(b), r.min(g).min(b));
        let l = (max + min) / 2.0;
        let d = max - min;
        if max == min {
            return (0.0, 0.0, l);
        }
        let s = if l > 0.5 {
            d / (2.0 - max - min)
        } else {
            d / (max + min)
        };
        let h = if max == r {
            (g - b) / d + if g < b { 6.0 } else { 0.0 }
        } else if max == g {
            (b - r) / d + 2.0
        } else {
            (r - g) / d + 4.0
        };
        (h / 6.0 * 360.0, s, l)
    }

    /// less `functions.hsla`: h in degrees, s/l/a in 0..1.
    pub(crate) fn from_hsl(h: f64, s: f64, l: f64, a: f64) -> Self {
        let h = (h % 360.0) / 360.0;
        let (s, l, a) = (s.clamp(0.0, 1.0), l.clamp(0.0, 1.0), a.clamp(0.0, 1.0));
        let m2 = if l <= 0.5 {
            l * (s + 1.0)
        } else {
            l + s - l * s
        };
        let m1 = l * 2.0 - m2;
        let hue = |h: f64| {
            let h = if h < 0.0 {
                h + 1.0
            } else if h > 1.0 {
                h - 1.0
            } else {
                h
            };
            if h * 6.0 < 1.0 {
                m1 + (m2 - m1) * h * 6.0
            } else if h * 2.0 < 1.0 {
                m2
            } else if h * 3.0 < 2.0 {
                m1 + (m2 - m1) * (2.0 / 3.0 - h) * 6.0
            } else {
                m1
            }
        };
        Self {
            r: hue(h + 1.0 / 3.0) * 255.0,
            g: hue(h) * 255.0,
            b: hue(h - 1.0 / 3.0) * 255.0,
            a,
        }
    }

    /// less `toARGB`: `#aarrggbb`.
    fn to_argb(self) -> String {
        let ch = |v: f64| v.round().clamp(0.0, 255.0) as u8;
        format!(
            "#{:02x}{:02x}{:02x}{:02x}",
            ch(self.a * 255.0),
            ch(self.r),
            ch(self.g),
            ch(self.b)
        )
    }

    /// less `Color.luma()` (WCAG relative luminance, gamma-corrected), value in 0..1.
    fn luma(self) -> f64 {
        let f = |c: f64| {
            let c = c / 255.0;
            if c <= 0.03928 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * f(self.r) + 0.7152 * f(self.g) + 0.0722 * f(self.b)
    }

    /// less `luminance()`: weighted luminance without gamma correction, value in 0..1.
    fn luminance(self) -> f64 {
        0.2126 * self.r / 255.0 + 0.7152 * self.g / 255.0 + 0.0722 * self.b / 255.0
    }

    /// less `toHSV`: h in degrees, s/v in 0..1.
    fn to_hsv(self) -> (f64, f64, f64) {
        let (r, g, b) = (self.r / 255.0, self.g / 255.0, self.b / 255.0);
        let (max, min) = (r.max(g).max(b), r.min(g).min(b));
        let d = max - min;
        let h = if d == 0.0 {
            0.0
        } else if max == r {
            ((g - b) / d).rem_euclid(6.0)
        } else if max == g {
            (b - r) / d + 2.0
        } else {
            (r - g) / d + 4.0
        };
        (h * 60.0, if max == 0.0 { 0.0 } else { d / max }, max)
    }
}

const NAMED: &[(&str, u32)] = &[
    ("black", 0x000000),
    ("white", 0xffffff),
    ("red", 0xff0000),
    ("green", 0x008000),
    ("blue", 0x0000ff),
    ("yellow", 0xffff00),
    ("cyan", 0x00ffff),
    ("magenta", 0xff00ff),
    ("gray", 0x808080),
    ("grey", 0x808080),
    ("orange", 0xffa500),
    ("purple", 0x800080),
    ("pink", 0xffc0cb),
    ("brown", 0xa52a2a),
    ("silver", 0xc0c0c0),
    ("transparent", 0x000000),
];

/// `number(n)`: percentages divide by `/100`, everything else keeps its value (less `functions/color.js`).
fn color_number(text: &str) -> Option<f64> {
    parse_number(text).map(|(v, u)| {
        if u.as_deref() == Some("%") {
            v / 100.0
        } else {
            v
        }
    })
}

pub(crate) fn parse_color(s: &str) -> Option<Color> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix('#') {
        if !hex.is_ascii() {
            return None;
        }
        let pair = |i: usize| -> Option<f64> {
            u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16)
                .ok()
                .map(f64::from)
        };
        let single = |i: usize| -> Option<f64> {
            u8::from_str_radix(&hex[i..i + 1].repeat(2), 16)
                .ok()
                .map(f64::from)
        };
        return match hex.len() {
            3 => Some(Color {
                r: single(0)?,
                g: single(1)?,
                b: single(2)?,
                a: 1.0,
            }),
            4 => Some(Color {
                r: single(0)?,
                g: single(1)?,
                b: single(2)?,
                a: single(3)? / 255.0,
            }),
            6 => Some(Color {
                r: pair(0)?,
                g: pair(1)?,
                b: pair(2)?,
                a: 1.0,
            }),
            8 => Some(Color {
                r: pair(0)?,
                g: pair(1)?,
                b: pair(2)?,
                a: pair(3)? / 255.0,
            }),
            _ => None,
        };
    }
    let lower = s.to_ascii_lowercase();
    let funcs = [
        ("rgba(", true),
        ("rgb(", false),
        ("hsla(", true),
        ("hsl(", false),
    ];
    for (name, has_alpha) in funcs {
        if lower.starts_with(name) && s.ends_with(')') {
            let inner = &s[name.len()..s.len() - 1];
            let slash_alpha = inner.contains('/') && !inner.contains("//");
            let parts = color_parts(inner);
            let alpha = parts.3.as_deref().and_then(color_number).unwrap_or(1.0);
            return if name.contains('l') {
                let h = parse_number(&parts.0)?.0;
                let (s, l) = (color_number(&parts.1)?, color_number(&parts.2)?);
                Some(Color::from_hsl(h, s, l, alpha))
            } else {
                let (r, g, b) = (
                    color_channel(&parts.0)?,
                    color_channel(&parts.1)?,
                    color_channel(&parts.2)?,
                );
                Some(Color {
                    r,
                    g,
                    b,
                    a: if has_alpha || slash_alpha { alpha } else { 1.0 },
                })
            };
        }
    }
    NAMED.iter().find(|(n, _)| *n == lower).map(|(_, v)| Color {
        r: f64::from((v >> 16) as u8),
        g: f64::from((v >> 8) as u8),
        b: f64::from(*v as u8),
        a: 1.0,
    })
}

/// Arguments of a color function: comma-separated (old syntax) or whitespace plus `/ alpha` (new syntax).
fn color_parts(inner: &str) -> (String, String, String, Option<String>) {
    let inner = inner.trim();
    let (main, alpha) = match inner.split_once('/') {
        Some((m, a)) if !m.contains("//") => (m.trim(), Some(a.trim().to_string())),
        _ => (inner, None),
    };
    let parts: Vec<&str> = main
        .split([',', ' '])
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();
    let at = |i: usize| parts.get(i).map_or(String::new(), |p| p.to_string());
    (
        at(0),
        at(1),
        at(2),
        alpha.or_else(|| parts.get(3).map(|p| p.to_string())),
    )
}

/// An rgb channel: percentages scale by 255, everything else keeps its value (less `scaled`).
fn color_channel(text: &str) -> Option<f64> {
    match parse_number(text) {
        Some((v, Some(u))) if u == "%" => Some(v * 255.0 / 100.0),
        Some((v, _)) => Some(v),
        None => None,
    }
}

/// Structured color extraction: evaluate the color along the AST, keeping the HSL->RGB floating-point channel.
///
/// Text evaluation writes colors as `#rrggbb`, so chained calls (`darken(spin(@c, -10), 5%)`) would lose precision;
/// less passes a Color object, so this recurses along the AST.
fn color_arg<'a>(
    cv: &ComponentValue<'a>,
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
) -> Option<Color> {
    if depth > 32 {
        return None;
    }
    match cv {
        ComponentValue::HexColor(_) => parse_color(text(src, cv)),
        ComponentValue::LessVariable(v) => color_in_var(v.name.name, src, scope, depth),
        ComponentValue::LessVariableCall(c) => {
            color_in_var(c.variable.name.name, src, scope, depth)
        }
        ComponentValue::Function(f) => color_function(f, src, scope, depth),
        // `$prop`: take the property value text and use it as a color (`lighten($color, 10%)`)
        ComponentValue::LessPropertyVariable(p) => {
            prop_text(p.name.name, src, scope, depth + 1).and_then(|t| parse_color(&t))
        }
        // Plain-text colors such as named colors (`red`/`transparent`)
        _ => parse_color(text(src, cv).trim()),
    }
}

fn color_in_var<'a>(
    name: &str,
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
) -> Option<Color> {
    match scope.var(name) {
        Some(VarRef::Ast(node)) => color_arg(node, src, scope, depth + 1),
        Some(VarRef::List(v)) => parse_color(&v.join(" ")),
        Some(VarRef::Text(t)) => parse_color(&t),
        Some(VarRef::Color(c)) => Some(c),
        Some(VarRef::Ruleset(_)) => None,
        None => None,
    }
}

/// Used by the compiler to bind arguments: evaluates the AST into a color (keeping the floating-point channel).
pub(super) fn color_of<'a>(
    cv: &ComponentValue<'a>,
    src: &'a str,
    scope: &dyn Scope<'a>,
) -> Option<Color> {
    // Literals (including literals found along a variable chain) keep their source text: `#333` must not expand to `#333333`
    if literal_color_text(cv, src, scope, 0).is_some() {
        return None;
    }
    color_arg(cv, src, scope, 0)
}

/// Literal source text of a color value: `#333`, `#FFAA00`, `red`, looked up along variable chains.
fn literal_color_text<'a>(
    cv: &ComponentValue<'a>,
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
) -> Option<String> {
    if depth > 32 {
        return None;
    }
    match cv {
        ComponentValue::HexColor(_) => Some(text(src, cv).to_string()),
        ComponentValue::InterpolableIdent(InterpolableIdent::Literal(i))
            if parse_color(i.name).is_some() =>
        {
            Some(i.name.to_string())
        }
        ComponentValue::LessVariable(v) => literal_color_in_var(v.name.name, src, scope, depth),
        ComponentValue::LessVariableCall(c) => {
            literal_color_in_var(c.variable.name.name, src, scope, depth)
        }
        _ => None,
    }
}

fn literal_color_in_var<'a>(
    name: &str,
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
) -> Option<String> {
    match scope.var(name) {
        Some(VarRef::Ast(node)) => literal_color_text(node, src, scope, depth + 1),
        Some(VarRef::List(v)) => parse_color(&v.join(" ")).map(|_| v.join(" ")),
        Some(VarRef::Text(t)) => parse_color(&t).map(|_| t),
        _ => None,
    }
}

fn color_function<'a>(
    f: &Function<'a>,
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
) -> Option<Color> {
    let name = function_name(f);
    // A comma is a separate node in the AST: it must be filtered when taking nodes by argument index
    let vals: Vec<&ComponentValue> = f
        .args
        .iter()
        .filter(|a| !matches!(a, ComponentValue::Delimiter(_)))
        .collect();
    let at = |i: usize| vals.get(i).copied();
    let number = |i: usize| at(i).and_then(|a| number_in(a, src, scope, depth + 1, true));
    let color = |i: usize| at(i).and_then(|a| color_arg(a, src, scope, depth + 1));
    let relative = at(2).is_some_and(|a| text(src, a).trim() == "relative");
    Some(match name.as_str() {
        "greyscale" => adjust(color(0)?, "greyscale", 0.0, false)?,
        "lighten" | "darken" | "saturate" | "desaturate" | "spin" => {
            adjust(color(0)?, &name, number(1)?.0, relative)?
        }
        "fade" | "fadein" | "fadeout" => fade(color(0)?, &name, number(1)?.0),
        "mix" => mix_colors(
            color(0)?,
            color(1)?,
            number(2).map_or(0.5, |(v, _)| v / 100.0),
        ),
        "tint" => mix_colors(
            parse_color("#ffffff")?,
            color(0)?,
            number(1).map_or(0.5, |(v, _)| v / 100.0),
        ),
        "shade" => mix_colors(
            parse_color("#000000")?,
            color(0)?,
            number(1).map_or(0.5, |(v, _)| v / 100.0),
        ),
        "rgb" | "rgba" => parse_color(&format!(
            "{}({})",
            name,
            join_args(&f.args, src, scope, depth)
        ))?,
        "hsl" | "hsla" => {
            let args = eval_args(f, src, scope, depth);
            let (h, s, l, a) = hsl_args(&args)?;
            Color::from_hsl(h, s, l, a)
        }
        _ => return None,
    })
}

/// The HSL adjustment shared by `lighten`/`darken`/`saturate`/`desaturate`/`spin`/`greyscale`.
fn adjust(c: Color, name: &str, amount: f64, relative: bool) -> Option<Color> {
    let (h, s, l) = c.to_hsl();
    let d = amount / 100.0;
    let (h, s, l) = match name {
        // less: greyscale = desaturate(color, 100)
        "greyscale" => (h, 0.0, l),
        "lighten" => (h, s, clamp01(if relative { l + l * d } else { l + d })),
        "darken" => (h, s, clamp01(if relative { l - l * d } else { l - d })),
        "saturate" => (h, clamp01(if relative { s + s * d } else { s + d }), l),
        "desaturate" => (h, clamp01(if relative { s - s * d } else { s - d }), l),
        "spin" => (h + amount, s, l),
        _ => return None,
    };
    Some(Color::from_hsl(h, s, l, c.a))
}

fn fade(c: Color, name: &str, amount: f64) -> Color {
    let d = amount / 100.0;
    let a = match name {
        "fade" => d,
        "fadein" => c.a + d,
        _ => c.a - d,
    };
    Color { a: clamp01(a), ..c }
}

/// less `mix` (the Sass formula with alpha weighting).
fn mix_colors(a: Color, b: Color, weight: f64) -> Color {
    let w = weight * 2.0 - 1.0;
    let da = a.a - b.a;
    let w1 = (if w * da == -1.0 {
        w
    } else {
        (w + da) / (1.0 + w * da)
    } + 1.0)
        / 2.0;
    let w2 = 1.0 - w1;
    let mix = |x: f64, y: f64| x * w1 + y * w2;
    Color {
        r: mix(a.r, b.r),
        g: mix(a.g, b.g),
        b: mix(a.b, b.b),
        a: a.a * weight + b.a * (1.0 - weight),
    }
}

fn clamp01(v: f64) -> f64 {
    v.clamp(0.0, 1.0)
}

/// Evaluates function arguments one by one (dropping comma nodes).
fn eval_args<'a>(
    f: &Function<'a>,
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
) -> Vec<String> {
    f.args
        .iter()
        .filter(|a| !matches!(a, ComponentValue::Delimiter(_)))
        .map(|a| eval(a, src, scope, depth + 1))
        .collect()
}

/// `hsl()`/`hsla()` (h, s, l, a): `number()` semantics, percentages divide by `/100`.
fn hsl_args(args: &[String]) -> Option<(f64, f64, f64, f64)> {
    let h = parse_number(arg_at(args, 0))?.0;
    let s = color_number(arg_at(args, 1))?;
    let l = color_number(arg_at(args, 2))?;
    let a = args.get(3).and_then(|a| color_number(a)).unwrap_or(1.0);
    Some((h, s, l, a))
}

/// `hsl()`/`hsla()` output: less converts to RGB and back to HSL (rounding happens in `fround`).
fn hsl_text(args: &[String]) -> Option<String> {
    // `hsla(color, alpha)`: less allows a color as the first argument
    if let Some(c) = args.first().and_then(|a| parse_color(a)) {
        let a = args.get(1).and_then(|a| color_number(a)).unwrap_or(c.a);
        let (h, s, l) = c.to_hsl();
        return Some(hsl_css(h, s, l, a));
    }
    let (h, s, l, a) = hsl_args(args)?;
    let (h, s, l) = Color::from_hsl(h, s, l, a).to_hsl();
    Some(hsl_css(h, s, l, a))
}

fn hsl_css(h: f64, s: f64, l: f64, a: f64) -> String {
    let a = round8(a).clamp(0.0, 1.0);
    let (h, s, l) = (fmt_number(h), fmt_number(s * 100.0), fmt_number(l * 100.0));
    if a >= 1.0 {
        format!("hsl({h}, {s}%, {l}%)")
    } else {
        format!("hsla({h}, {s}%, {l}%, {})", fmt_number(a))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn number_formatting() {
        assert_eq!(fmt_number(10.0), "10");
        assert_eq!(fmt_number(0.5), "0.5");
        assert_eq!(fmt_number(20.0), "20");
        assert_eq!(fmt_number(0.30000000000000004), "0.3");
    }

    #[test]
    fn color_lighten_matches_less() {
        // less: lighten(#333, 10%) => #4d4d4d
        let c = parse_color("#333").unwrap();
        let (h, s, l) = c.to_hsl();
        assert_eq!(Color::from_hsl(h, s, l + 0.1, 1.0).to_css(), "#4d4d4d");
    }

    #[test]
    fn color_darken_matches_less() {
        // less: darken(#333, 10%) => #1a1a1a
        let c = parse_color("#333").unwrap();
        let (h, s, l) = c.to_hsl();
        assert_eq!(
            Color::from_hsl(h, s, (l - 0.1).max(0.0), 1.0).to_css(),
            "#1a1a1a"
        );
    }

    #[test]
    fn unit_conversion_matches_less() {
        // less: 1cm + 10mm => 2cm; 1px + 1em => 2px (the left unit is kept when not convertible)
        assert_eq!(
            operate(&LessOperationOperatorKind::Plus, "1cm", "10mm").as_deref(),
            Some("2cm")
        );
        assert_eq!(
            operate(&LessOperationOperatorKind::Plus, "1px", "1em").as_deref(),
            Some("2px")
        );
        assert_eq!(
            operate(&LessOperationOperatorKind::Plus, "1turn", "90deg").as_deref(),
            Some("1.25turn")
        );
    }

    #[test]
    fn color_arithmetic_matches_less() {
        // less: #333 + #111 => #444444; #888 * 0.5 => #444444
        assert_eq!(
            operate(&LessOperationOperatorKind::Plus, "#333", "#111").as_deref(),
            Some("#444444")
        );
        assert_eq!(
            operate(&LessOperationOperatorKind::Multiply, "#888", "0.5").as_deref(),
            Some("#444444")
        );
        assert_eq!(
            operate(&LessOperationOperatorKind::Minus, "#888", "#111").as_deref(),
            Some("#777777")
        );
    }
}
