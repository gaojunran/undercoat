//! less guard evaluation: `when (...)` condition trees, value comparison, and `default()`.
//!
//! Comparison rules follow less@4.1.3's `Node.compare` one by one:
//! - Nodes with `compare` (Dimension / Color / Quoted / Anonymous) are compared first,
//!   but when the right side is Quoted / Anonymous, the right side's `compare` is used instead (the "symmetric result" uses toCSS).
//! - When both Quoted sides have quotes, only the content is compared (quote differences are ignored); when `~"..."` is involved, comparison degrades to toCSS.
//! - A Dimension with one side unitless is compared by value directly; with units on both sides both are first normalized to px/s/rad, and different units are treated as incomparable.

use std::collections::HashMap;

use oxc_css_parser::ast::*;

use super::value::{self, Color, Scope, VarRef};

/// A value taking part in comparison inside a guard.
#[derive(Clone, Debug)]
pub(crate) enum Val {
    /// Dimension / Percentage: `4px`, `50%`, `3`
    Num(f64, Option<String>),
    /// Color: the color value and its original spelling (a named color like `white` keeps its text for toCSS comparison)
    Color(Color, Option<String>),
    Quoted {
        text: String,
        /// An unquoted string such as `~"..."`
        escaped: bool,
        quote: char,
    },
    Keyword(String),
    Anonymous(String),
    /// A space- or comma-separated list (less's Expression / Value)
    List(Vec<Val>, bool),
    Url(String),
    Other(String),
}

impl Val {
    fn keyword(v: bool) -> Self {
        Val::Keyword(if v { "true" } else { "false" }.to_string())
    }
}

fn type_name(v: &Val) -> &'static str {
    match v {
        Val::Num(..) => "Dimension",
        Val::Color(..) => "Color",
        Val::Quoted { .. } => "Quoted",
        Val::Keyword(_) => "Keyword",
        Val::Anonymous(_) => "Anonymous",
        Val::List(_, true) => "Value",
        Val::List(_, false) => "Expression",
        Val::Url(_) => "URL",
        Val::Other(_) => "Other",
    }
}

fn has_compare(v: &Val) -> bool {
    matches!(
        v,
        Val::Num(..) | Val::Color(..) | Val::Quoted { .. } | Val::Anonymous(_)
    )
}

fn is_quoted_like(v: &Val) -> bool {
    matches!(v, Val::Quoted { .. } | Val::Anonymous(_))
}

/// CSS text of a value (Quoted follows escaped to decide whether to add quotes).
pub(crate) fn to_css(v: &Val) -> String {
    match v {
        Val::Num(n, u) => match u {
            Some(u) => format!("{}{u}", value::fmt_number(*n)),
            None => value::fmt_number(*n),
        },
        Val::Color(c, original) => original.clone().unwrap_or_else(|| c.to_css()),
        Val::Quoted {
            text,
            escaped,
            quote,
        } => {
            if *escaped {
                text.clone()
            } else {
                format!("{quote}{text}{quote}")
            }
        }
        Val::Keyword(t) | Val::Anonymous(t) | Val::Other(t) | Val::Url(t) => t.clone(),
        Val::List(items, comma) => items
            .iter()
            .map(to_css)
            .collect::<Vec<_>>()
            .join(if *comma { ", " } else { " " }),
    }
}

fn num_cmp(a: f64, b: f64) -> Option<i32> {
    if a < b {
        Some(-1)
    } else if a == b {
        Some(0)
    } else if a > b {
        Some(1)
    } else {
        None
    }
}

/// Unit normalization (less `Dimension.unify` → px / s / rad).
fn unified(v: f64, unit: &str) -> (String, f64) {
    const CANONICAL: [&str; 3] = ["px", "s", "rad"];
    match (value::unit_factor(unit), value::unit_factor(CANONICAL[0])) {
        (Some((g, f)), _) => {
            let target = CANONICAL[g];
            let tf = value::unit_factor(target).map(|(_, f)| f).unwrap_or(1.0);
            (target.to_string(), v * f / tf)
        }
        _ => (unit.to_string(), v),
    }
}

/// less `Node.compare`: `-1/0/1` are comparable results, `None` means "incomparable" (every comparison then yields false).
pub(crate) fn cmp(a: &Val, b: &Val) -> Option<i32> {
    if has_compare(a) && !is_quoted_like(b) {
        return compare_node(a, b);
    }
    if has_compare(b) {
        return compare_node(b, a).map(|v| -v);
    }
    if type_name(a) != type_name(b) {
        return None;
    }
    deep_cmp(a, b)
}

fn compare_node(a: &Val, b: &Val) -> Option<i32> {
    match a {
        Val::Num(av, au) => match b {
            Val::Num(bv, bu) => match (au, bu) {
                (None, _) | (_, None) => num_cmp(*av, *bv),
                (Some(a), Some(b)) => {
                    let (ua, va) = unified(*av, a);
                    let (ub, vb) = unified(*bv, b);
                    (ua == ub).then(|| num_cmp(va, vb)).flatten()
                }
            },
            _ => None,
        },
        Val::Color(c, _) => match b {
            Val::Color(o, _) => (c.r == o.r && c.g == o.g && c.b == o.b && c.a == o.a).then_some(0),
            _ => None,
        },
        Val::Quoted {
            text,
            escaped,
            quote: _,
        } => match b {
            Val::Quoted {
                text: btext,
                escaped: bescaped,
                ..
            } if !escaped && !bescaped => {
                // Both quoted: ignore quote differences and compare content only
                Some(if text == btext {
                    0
                } else if text.as_str() < btext.as_str() {
                    -1
                } else {
                    1
                })
            }
            _ => (to_css(a) == to_css(b)).then_some(0),
        },
        Val::Anonymous(text) => (to_css(b) == *text).then_some(0),
        _ => None,
    }
}

/// Value comparison of same-type nodes (the array/scalar branches of less `Node.compare`).
fn deep_cmp(a: &Val, b: &Val) -> Option<i32> {
    match (a, b) {
        (Val::List(x, _), Val::List(y, _)) => {
            if x.len() != y.len() {
                return None;
            }
            x.iter()
                .zip(y)
                .all(|(a, b)| cmp(a, b) == Some(0))
                .then_some(0)
        }
        _ => (to_css(a) == to_css(b)).then_some(0),
    }
}

/// Shape inference for plain-text values (globalVars / variables leaked from a mixin only have text).
pub(crate) fn from_text(text: &str) -> Val {
    let t = text.trim();
    for q in ['"', '\''] {
        if t.len() >= 2 && t.starts_with(q) && t.ends_with(q) {
            return Val::Quoted {
                text: t[1..t.len() - 1].to_string(),
                escaped: false,
                quote: q,
            };
        }
    }
    if let Some((n, u)) = value::parse_number(t) {
        return Val::Num(n, u);
    }
    if !t.starts_with('#')
        && let Some(c) = value::parse_color(t)
    {
        return Val::Color(c, Some(t.to_string()));
    }
    Val::Keyword(t.to_string())
}

fn text_of<'a>(cv: &ComponentValue, src: &'a str) -> &'a str {
    let s = cv.span();
    if s.start <= s.end && s.end <= src.len() {
        &src[s.start..s.end]
    } else {
        ""
    }
}

/// Context of one guard evaluation.
pub(crate) struct Ctx<'a, 'b> {
    pub(crate) src: &'a str,
    pub(crate) scope: &'b dyn Scope<'a>,
    pub(crate) params: &'b HashMap<String, Val>,
    /// The `default()` value in this round (less's two-pass evaluation)
    pub(crate) default: bool,
}

pub(crate) fn eval_conditions<'a>(conds: &LessConditions<'a>, ctx: &Ctx<'a, '_>) -> bool {
    conds.conditions.iter().any(|c| eval_condition(c, ctx, 0))
}

/// Evaluates a single condition (the argument of `boolean()` / `if()`).
pub(crate) fn eval_single<'a>(cond: &LessCondition<'a>, ctx: &Ctx<'a, '_>) -> bool {
    eval_condition(cond, ctx, 0)
}

fn eval_condition<'a>(cv: &LessCondition<'a>, ctx: &Ctx<'a, '_>, depth: usize) -> bool {
    if depth > 32 {
        return false;
    }
    match cv {
        LessCondition::Binary(b) => match b.op.kind {
            LessBinaryConditionOperatorKind::And => {
                eval_condition(&b.left, ctx, depth + 1) && eval_condition(&b.right, ctx, depth + 1)
            }
            LessBinaryConditionOperatorKind::Or => {
                eval_condition(&b.left, ctx, depth + 1) || eval_condition(&b.right, ctx, depth + 1)
            }
            ref op => compare_op(
                op,
                &cond_val(&b.left, ctx, depth + 1),
                &cond_val(&b.right, ctx, depth + 1),
            ),
        },
        LessCondition::Negated(n) => !eval_condition(&n.condition, ctx, depth + 1),
        LessCondition::Parenthesized(p) => eval_condition(&p.condition, ctx, depth + 1),
        // Bare value condition: less treats `when (@a)` as `@a = true`
        LessCondition::Value(v) => compare_op(
            &LessBinaryConditionOperatorKind::Equal,
            &val_of(v, ctx, depth + 1),
            &Val::Keyword("true".to_string()),
        ),
    }
}

fn cond_val<'a>(c: &LessCondition<'a>, ctx: &Ctx<'a, '_>, depth: usize) -> Val {
    match c {
        LessCondition::Value(v) => val_of(v, ctx, depth),
        LessCondition::Parenthesized(p) => cond_val(&p.condition, ctx, depth + 1),
        LessCondition::Negated(n) => Val::keyword(!eval_condition(&n.condition, ctx, depth + 1)),
        LessCondition::Binary(_) => Val::keyword(eval_condition(c, ctx, depth + 1)),
    }
}

fn compare_op(op: &LessBinaryConditionOperatorKind, a: &Val, b: &Val) -> bool {
    use LessBinaryConditionOperatorKind::*;
    // less's `Condition.eval` only recognizes `> >= < <= = =<`; `=>` parses but never matches.
    match cmp(a, b) {
        Some(-1) => matches!(op, LessThan | LessThanOrEqual | EqualOrLessThan),
        Some(0) => matches!(
            op,
            Equal | GreaterThanOrEqual | LessThanOrEqual | EqualOrLessThan
        ),
        Some(1) => matches!(op, GreaterThan | GreaterThanOrEqual),
        _ => false,
    }
}

/// `ComponentValue` → comparable value.
pub(crate) fn val_of<'a>(cv: &ComponentValue<'a>, ctx: &Ctx<'a, '_>, depth: usize) -> Val {
    if depth > 16 {
        return Val::Other(text_of(cv, ctx.src).to_string());
    }
    let text = text_of(cv, ctx.src);
    match cv {
        ComponentValue::Number(_) => match value::parse_number(text) {
            Some((v, _)) => Val::Num(v, None),
            None => Val::Other(text.to_string()),
        },
        ComponentValue::Dimension(d) => match value::parse_number(text) {
            Some((v, u)) => Val::Num(v, u.or_else(|| Some(d.unit.name.to_string()))),
            None => Val::Other(text.to_string()),
        },
        ComponentValue::Percentage(_) => match value::parse_number(text) {
            Some((v, _)) => Val::Num(v, Some("%".to_string())),
            None => Val::Other(text.to_string()),
        },
        ComponentValue::HexColor(_) => match value::parse_color(text) {
            Some(c) => Val::Color(c, None),
            None => Val::Other(text.to_string()),
        },
        ComponentValue::LessVariable(v) => var_val(v.name.name, ctx, depth, Some(*cv.span())),
        ComponentValue::LessVariableCall(c) => {
            var_val(c.variable.name.name, ctx, depth, Some(*cv.span()))
        }
        ComponentValue::LessVariableVariable(vv) => {
            let inner = value::interpolate(
                &format!("@{}", vv.variable.name.name),
                ctx.src,
                ctx.scope,
                depth + 1,
                Some(*cv.span()),
            );
            var_val(inner.trim_start_matches('@'), ctx, depth, Some(*cv.span()))
        }
        ComponentValue::InterpolableIdent(InterpolableIdent::Literal(i)) => {
            let raw = i.raw.trim_end();
            // less parses color keywords into Color nodes (so `white = #fff` holds)
            match value::parse_color(raw) {
                Some(c) => Val::Color(c, Some(raw.to_string())),
                None => Val::Keyword(raw.to_string()),
            }
        }
        ComponentValue::InterpolableIdent(_) => {
            Val::Keyword(value::eval(cv, ctx.src, ctx.scope, depth + 1))
        }
        ComponentValue::InterpolableStr(InterpolableStr::Literal(s)) => Val::Quoted {
            text: s.value.to_string(),
            escaped: false,
            quote: s.raw.chars().next().unwrap_or('"'),
        },
        ComponentValue::InterpolableStr(_) => {
            let raw = value::eval(cv, ctx.src, ctx.scope, depth + 1);
            let quote = raw
                .chars()
                .next()
                .filter(|c| *c == '"' || *c == '\'')
                .unwrap_or('"');
            Val::Quoted {
                text: raw.trim_matches(['"', '\'']).to_string(),
                escaped: false,
                quote,
            }
        }
        ComponentValue::LessEscapedStr(e) => Val::Quoted {
            text: value::interpolate(e.str.value, ctx.src, ctx.scope, depth + 1, Some(*cv.span())),
            escaped: true,
            quote: '"',
        },
        // Operation: parenthesized operations in a guard must be evaluated (`when (8 + (5 - 1) < 13)`)
        ComponentValue::LessBinaryOperation(_) | ComponentValue::LessParenthesizedOperation(_) => {
            from_text(&value::eval(cv, ctx.src, ctx.scope, depth + 1))
        }
        ComponentValue::LessList(list) => Val::List(
            list.elements
                .iter()
                .map(|v| val_of(v, ctx, depth + 1))
                .collect(),
            list.comma_spans.is_some(),
        ),
        ComponentValue::Url(_) => Val::Url(value::eval(cv, ctx.src, ctx.scope, depth + 1)),
        ComponentValue::Function(f) => function_val(f, ctx, depth),
        _ => from_text(&value::eval(cv, ctx.src, ctx.scope, depth + 1)),
    }
}

fn var_val<'a>(
    name: &str,
    ctx: &Ctx<'a, '_>,
    depth: usize,
    span: Option<oxc_css_parser::pos::Span>,
) -> Val {
    if let Some(v) = ctx.params.get(name) {
        return v.clone();
    }
    match ctx.scope.var(name) {
        Some(VarRef::Ast(node)) => val_of(node, ctx, depth + 1),
        Some(VarRef::List(v)) => from_text(&v.join(" ")),
        Some(VarRef::Text(t)) => from_text(&t),
        Some(VarRef::Color(c)) => from_text(&c.to_css()),
        Some(VarRef::Ruleset(_)) => from_text(""),
        None => {
            if let Some(span) = span {
                ctx.scope.var_miss(name, span);
            }
            Val::Other(format!("@{name}"))
        }
    }
}

fn unit_is(v: &Val, unit: &str) -> bool {
    matches!(v, Val::Num(_, Some(u)) if u == unit)
}

fn function_val<'a>(f: &Function<'a>, ctx: &Ctx<'a, '_>, depth: usize) -> Val {
    let name = match &f.name {
        FunctionName::Ident(InterpolableIdent::Literal(i)) => i.name.to_string(),
        _ => String::new(),
    };
    // Probe functions do not evaluate their arguments (less's evalArgs=false)
    match name.as_str() {
        "isdefined" => {
            return Val::keyword(
                f.args
                    .first()
                    .is_some_and(|a| value::probe_defined(a, ctx.scope)),
            );
        }
        "isruleset" => {
            return Val::keyword(
                f.args
                    .first()
                    .is_some_and(|a| value::probe_ruleset(a, ctx.scope)),
            );
        }
        _ => {}
    }
    let args: Vec<Val> = f
        .args
        .iter()
        .filter(|a| !matches!(a, ComponentValue::Delimiter(_)))
        .map(|a| val_of(a, ctx, depth + 1))
        .collect();
    let first = || args.first().cloned().unwrap_or(Val::Other(String::new()));
    match name.as_str() {
        "default" => Val::keyword(ctx.default),
        // `e("...")` is Anonymous in less: when compared it only compares toCSS
        "e" => Val::Anonymous(
            args.first()
                .map(|a| to_css(a).trim_matches(['"', '\'']).to_string())
                .unwrap_or_default(),
        ),
        "isnumber" => Val::keyword(matches!(first(), Val::Num(..))),
        "iscolor" => Val::keyword(matches!(first(), Val::Color(..))),
        "isstring" => Val::keyword(matches!(first(), Val::Quoted { .. })),
        "iskeyword" => Val::keyword(matches!(first(), Val::Keyword(_))),
        "isurl" => Val::keyword(matches!(first(), Val::Url(_))),
        "ispixel" => Val::keyword(unit_is(&first(), "px")),
        "ispercentage" => Val::keyword(unit_is(&first(), "%")),
        "isem" => Val::keyword(unit_is(&first(), "em")),
        "isunit" => {
            let unit = match args.get(1).map(to_css) {
                Some(u) => u.trim_matches(['"', '\'']).to_string(),
                None => return Val::keyword(false),
            };
            Val::keyword(unit_is(&first(), &unit))
        }
        "lightness" => match first() {
            Val::Color(c, _) => Val::Num(c.to_hsl().2 * 100.0, Some("%".to_string())),
            _ => Val::Other(value::eval_function_text(f, ctx.src, ctx.scope, depth)),
        },
        _ => Val::Other(value::eval_function_text(f, ctx.src, ctx.scope, depth)),
    }
}
