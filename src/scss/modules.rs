//! Built-in modules (`sass:math` / `sass:map` / …).
//!
//! Built-in modules **do not take part in file resolution**: `@use "sass:math"`
//! directly yields a static member table, and the namespace never resolves to a user
//! file. The member table follows the vendored dart-sass 1.35.2 as observed
//! (`meta.module-functions()`), not the latest docs: `color.channel`,
//! `meta.calc-args` and `list.slice` do not exist in 1.35.2.
//!
//! 1.35.2 carries a historical wart: `meta.module-functions("sass:color")` lists 9
//! deprecated functions that are **not actually callable** (`lighten`/`darken`/…), and
//! calling them reports `The function X() isn't in the sass:color module.`. Here
//! "listed" and "callable" are two separate tables, so this bug is matched as well.

use super::builtins::{self, Args};
use super::color_space::{self, Space};
use super::extend::{Compound, Sel, parse_sel};
use super::value::{self, Color, List, Map, Number, SassStr, Sep, ValResult, Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Builtin {
    Math,
    Map,
    Meta,
    String,
    List,
    Color,
    Selector,
}

/// Members listed by `meta.module-functions()` (including the deprecated color
/// functions that are not callable in 1.35.2).
const MATH_FUNCTIONS: &[&str] = &[
    "abs",
    "acos",
    "asin",
    "atan",
    "atan2",
    "ceil",
    "clamp",
    "cos",
    "compatible",
    "floor",
    "hypot",
    "is-unitless",
    "log",
    "max",
    "min",
    "percentage",
    "pow",
    "random",
    "round",
    "sin",
    "sqrt",
    "tan",
    "unit",
    "div",
];

const MAP_FUNCTIONS: &[&str] = &[
    "get",
    "set",
    "merge",
    "remove",
    "keys",
    "values",
    "has-key",
    "deep-merge",
    "deep-remove",
];

const META_FUNCTIONS: &[&str] = &[
    "feature-exists",
    "inspect",
    "type-of",
    "keywords",
    "global-variable-exists",
    "variable-exists",
    "function-exists",
    "mixin-exists",
    "content-exists",
    "module-variables",
    "module-functions",
    "get-function",
    "call",
    "calc-name",
    "calc-args",
];

const STRING_FUNCTIONS: &[&str] = &[
    "unquote",
    "quote",
    "to-upper-case",
    "to-lower-case",
    "length",
    "insert",
    "index",
    "slice",
    "unique-id",
    "split",
];

const LIST_FUNCTIONS: &[&str] = &[
    "length",
    "nth",
    "set-nth",
    "join",
    "append",
    "zip",
    "index",
    "is-bracketed",
    "separator",
    "slash",
];

/// The 28 listed in the module table, the last 9 of which are actually not callable in
/// 1.35.2.
/// Members that only exist in 1.93.2 (their names do not even exist in 1.35.2).
const COLOR_MODERN: &[&str] = &[
    "space",
    "to-space",
    "is-legacy",
    "is-missing",
    "is-powerless",
    "is-in-gamut",
    "to-gamut",
    "channel",
    "same",
];

const COLOR_FUNCTIONS: &[&str] = &[
    "red",
    "green",
    "blue",
    "mix",
    "invert",
    "hue",
    "saturation",
    "lightness",
    "complement",
    "adjust-hue",
    "lighten",
    "darken",
    "saturate",
    "desaturate",
    "grayscale",
    "hwb",
    "whiteness",
    "blackness",
    "opacify",
    "fade-in",
    "transparentize",
    "fade-out",
    "alpha",
    "opacity",
    "adjust",
    "scale",
    "change",
    "ie-hex-str",
    "space",
    "to-space",
    "is-legacy",
    "is-missing",
    "is-powerless",
    "is-in-gamut",
    "to-gamut",
    "channel",
    "same",
];

const SELECTOR_FUNCTIONS: &[&str] = &[
    "is-superselector",
    "simple-selectors",
    "parse",
    "nest",
    "append",
    "extend",
    "replace",
    "unify",
];

/// The members callable in the module. 1.35.2 really exports only the first 19 (the 9
/// such as `lighten` are listed, but calling them errors); 1.93.2 keeps the same quirk
/// (`meta.module-functions("color")` lists them and calling them still reports
/// `The function lighten() isn't in the sass:color module.`), so this stays as is.
const COLOR_CALLABLE: &[&str] = &[
    "red",
    "green",
    "blue",
    "mix",
    "invert",
    "hue",
    "saturation",
    "lightness",
    "complement",
    "grayscale",
    "hwb",
    "whiteness",
    "blackness",
    "alpha",
    "opacity",
    "adjust",
    "scale",
    "change",
    "ie-hex-str",
    "space",
    "to-space",
    "is-legacy",
    "is-missing",
    "is-powerless",
    "is-in-gamut",
    "to-gamut",
    "channel",
    "same",
];

impl Builtin {
    /// URL of `@use "sass:xxx"` -> module. An unknown built-in URL returns `None` (the
    /// caller reports "can't find stylesheet").
    pub(crate) fn from_url(url: &str) -> Option<Self> {
        Some(match url {
            "sass:math" => Self::Math,
            "sass:map" => Self::Map,
            "sass:meta" => Self::Meta,
            "sass:string" => Self::String,
            "sass:list" => Self::List,
            "sass:color" => Self::Color,
            "sass:selector" => Self::Selector,
            _ => return None,
        })
    }

    pub(crate) fn default_namespace(self) -> &'static str {
        match self {
            Self::Math => "math",
            Self::Map => "map",
            Self::Meta => "meta",
            Self::String => "string",
            Self::List => "list",
            Self::Color => "color",
            Self::Selector => "selector",
        }
    }

    /// Key set of `meta.module-functions()` (including the non-callable legacy items).
    pub(crate) fn listed_functions(self) -> &'static [&'static str] {
        match self {
            Self::Math => MATH_FUNCTIONS,
            Self::Map => MAP_FUNCTIONS,
            Self::Meta => META_FUNCTIONS,
            Self::String => STRING_FUNCTIONS,
            Self::List => LIST_FUNCTIONS,
            Self::Color => COLOR_FUNCTIONS,
            Self::Selector => SELECTOR_FUNCTIONS,
        }
    }

    /// Whether the name appears in the module's **member table** (this table is used
    /// when `as *` shadows a global function).
    pub(crate) fn has_member(self, name: &str) -> bool {
        self.listed_functions()
            .iter()
            .any(|member| value::names_eq(member, name))
    }

    pub(crate) fn is_callable(self, name: &str) -> bool {
        let table = match self {
            Self::Color => COLOR_CALLABLE,
            other => other.listed_functions(),
        };
        table.iter().any(|member| value::names_eq(member, name))
    }

    /// Module variables. `$e` / `$pi` exist since 1.35.2; the other five constants were
    /// added in 1.93.2.
    pub(crate) fn variable(self, name: &str) -> Option<Value> {
        self.variables()
            .into_iter()
            .find(|(variable, _)| value::names_eq(variable, name))
            .map(|(_, value)| Value::Number(Number::unitless(value)))
    }

    pub(crate) fn variables(self) -> Vec<(&'static str, f64)> {
        match self {
            Self::Math => vec![
                ("e", std::f64::consts::E),
                ("pi", std::f64::consts::PI),
                ("epsilon", f64::EPSILON),
                ("max-safe-integer", 9007199254740991.0),
                ("min-safe-integer", -9007199254740991.0),
                ("max-number", f64::MAX),
                ("min-number", f64::MIN_POSITIVE),
            ],
            _ => Vec::new(),
        }
    }
}

/// The "function is not in the module" error (1.35.2 wording).
pub(crate) fn not_in_module(module: Builtin, name: &str) -> String {
    format!(
        "The function {}() isn't in the sass:{} module.",
        name,
        module.default_namespace()
    )
}

/// Module function dispatch. `Meta` needs the evaluator context and is handled directly
/// by `eval`.
pub(crate) fn call(
    module: Builtin,
    name: &str,
    positional: &[Value],
    keywords: &[(String, Value)],
) -> ValResult<Value> {
    if !module.is_callable(name) {
        return Err(not_in_module(module, name));
    }
    match module {
        Builtin::Math => math(name, positional, keywords),
        Builtin::Map => map(name, positional, keywords),
        Builtin::String => string(name, positional, keywords),
        Builtin::List => list(name, positional, keywords),
        Builtin::Color => color(name, positional, keywords),
        Builtin::Selector => selector(name, positional, keywords),
        // The three `meta` functions that don't need the evaluator (`inspect` /
        // `type-of` / `keywords`) reuse the global implementations directly.
        Builtin::Meta => {
            Ok(builtins::call(name, positional, keywords)?.expect("present in the global table"))
        }
    }
}

// ---------------------------------------------------------------------------
// sass:math
// ---------------------------------------------------------------------------

fn math(name: &str, positional: &[Value], keywords: &[(String, Value)]) -> ValResult<Value> {
    match name {
        "div" => div(positional, keywords),
        "percentage" => {
            let args = Args::new(&["number"], positional, keywords)?;
            let number = args.number(0)?;
            if !number.is_unitless() {
                return Err(format!(
                    "$number: Expected {} to have no units.",
                    number.format()
                ));
            }
            Ok(Value::Number(Number::with_unit(number.value * 100.0, "%")))
        }
        // Members with exactly the same semantics as a global function of the same name
        // reuse the global implementation directly.
        "round" | "ceil" | "floor" | "abs" | "max" | "min" => {
            Ok(builtins::call(name, positional, keywords)?.expect("present in the global table"))
        }
        "unit" => {
            Ok(builtins::call("unit", positional, keywords)?.expect("present in the global table"))
        }
        "is-unitless" => {
            Ok(builtins::call("unitless", positional, keywords)?
                .expect("present in the global table"))
        }
        "compatible" => Ok(builtins::call("comparable", positional, keywords)?
            .expect("present in the global table")),
        "pow" => pow(positional, keywords),
        "sqrt" => unary_unitless("sqrt", positional, keywords, f64::sqrt),
        "log" => log(positional, keywords),
        "hypot" => hypot(positional, keywords),
        "clamp" => clamp(positional, keywords),
        "random" => random(positional, keywords),
        "sin" | "cos" | "tan" => trig(name, positional, keywords),
        "acos" | "asin" => inverse_trig(name, positional, keywords),
        "atan" => {
            let args = Args::new(&["number"], positional, keywords)?;
            let number = args.number(0)?;
            let value = unitless_value(number, "number")?;
            Ok(deg(value.atan().to_degrees()))
        }
        "atan2" => atan2(positional, keywords),
        _ => Err(not_in_module(Builtin::Math, name)),
    }
}

fn deg(value: f64) -> Value {
    Value::Number(Number::with_unit(value, "deg"))
}

/// dart's `assertNoUnits` wording.
fn no_units(number: &Number, param: &str) -> ValResult<f64> {
    if !number.is_unitless() {
        return Err(format!(
            "${param}: Expected {} to have no units.",
            number.format()
        ));
    }
    Ok(number.value)
}

fn unitless_value(number: &Number, param: &str) -> ValResult<f64> {
    no_units(number, param)
}

/// The module version of `/`: a real division when both sides are numbers; when one
/// side is not a number it degrades to a slash list (the 1.35.2 behavior, which also
/// emits a deprecation warning; no warning is emitted here, the output is the same).
fn div(positional: &[Value], keywords: &[(String, Value)]) -> ValResult<Value> {
    let args = Args::new(&["number1", "number2"], positional, keywords)?;
    let left = args.req(0)?;
    let right = args.req(1)?;
    match (left, right) {
        (Value::Number(a), Value::Number(b)) => Ok(Value::Number(a.div(b)?)),
        _ => Ok(Value::List(List {
            items: vec![left.clone(), right.clone()],
            sep: Sep::Slash,
            bracketed: false,
        })),
    }
}

fn pow(positional: &[Value], keywords: &[(String, Value)]) -> ValResult<Value> {
    let args = Args::new(&["base", "exponent"], positional, keywords)?;
    let base = no_units(args.number(0)?, "base")?;
    let exponent = no_units(args.number(1)?, "exponent")?;
    Ok(Value::Number(Number::unitless(base.powf(exponent))))
}

fn unary_unitless(
    name: &str,
    positional: &[Value],
    keywords: &[(String, Value)],
    f: impl Fn(f64) -> f64,
) -> ValResult<Value> {
    let _ = name;
    let args = Args::new(&["number"], positional, keywords)?;
    let value = no_units(args.number(0)?, "number")?;
    Ok(Value::Number(Number::unitless(f(value))))
}

fn log(positional: &[Value], keywords: &[(String, Value)]) -> ValResult<Value> {
    let args = Args::new(&["number", "base"], positional, keywords)?;
    let value = no_units(args.number(0)?, "number")?;
    let base = match args.opt_number(1)? {
        Some(base) => Some(no_units(base, "base")?),
        None => None,
    };
    Ok(Value::Number(Number::unitless(match base {
        Some(base) => value.ln() / base.ln(),
        None => value.ln(),
    })))
}

/// `hypot($numbers...)`: units are converted to the unit of the first argument.
fn hypot(positional: &[Value], keywords: &[(String, Value)]) -> ValResult<Value> {
    if let Some((name, _)) = keywords.first() {
        return Err(format!("No argument named ${name}."));
    }
    let Some(first) = positional.first() else {
        return Err("At least one argument must be passed.".to_string());
    };
    let first = first.assert_number("numbers")?;
    let mut sum = first.value * first.value;
    for (i, value) in positional.iter().enumerate().skip(1) {
        let number = value.assert_number("numbers")?;
        let scaled = first.coerce(number).map_err(|_| {
            format!(
                "$numbers[{}]: {} and $numbers[1]: {} have incompatible units.",
                i + 1,
                number.format(),
                first.format()
            )
        })?;
        sum += scaled * scaled;
    }
    Ok(Value::Number(Number {
        value: sum.sqrt(),
        numer: first.numer.clone(),
        denom: first.denom.clone(),
        slash: None,
    }))
}

fn clamp(positional: &[Value], keywords: &[(String, Value)]) -> ValResult<Value> {
    let args = Args::new(&["min", "number", "max"], positional, keywords)?;
    let min = args.number(0)?;
    let number = args.number(1)?;
    let max = args.number(2)?;
    // dart: first check that $max and $min are comparable (the message is prefixed with
    // `$max: `).
    max.compare(min)
        .map_err(|message| format!("$max: {message}"))?;
    if number
        .compare(min)
        .is_ok_and(|o| o == std::cmp::Ordering::Less)
    {
        return Ok(Value::Number(min.clone()));
    }
    if number
        .compare(max)
        .is_ok_and(|o| o == std::cmp::Ordering::Greater)
    {
        return Ok(Value::Number(max.clone()));
    }
    Ok(Value::Number(number.clone()))
}

fn random(positional: &[Value], keywords: &[(String, Value)]) -> ValResult<Value> {
    let args = Args::new(&["limit"], positional, keywords)?;
    let Some(limit) = args.opt_number(0)? else {
        return Ok(Value::Number(Number::unitless(next_random())));
    };
    if limit.value <= 0.0 {
        return Err(format!(
            "$limit: Must be greater than 0, was {}.",
            limit.format()
        ));
    }
    let limit = limit.assert_int("limit")?;
    Ok(Value::Number(Number::unitless(
        (next_random() * limit as f64).floor() + 1.0,
    )))
}

/// No cryptographic strength needed: the output of `math.random()` is nondeterministic
/// anyway, and tests only check the range.
fn next_random() -> f64 {
    use std::cell::Cell;
    thread_local! {
        static STATE: Cell<u64> = const { Cell::new(0) };
    }
    STATE.with(|state| {
        let mut x = state.get();
        if x == 0 {
            x = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0x9e37_79b9_7f4a_7c15);
        }
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        state.set(x);
        ((x >> 11) as f64) / ((1u64 << 53) as f64)
    })
}

/// Angle argument of trigonometric functions: unitless counts as radians, angle units
/// are converted to radians.
fn radians(number: &Number, param: &str) -> ValResult<f64> {
    match number.unit() {
        "" => Ok(number.value),
        "deg" => Ok(number.value.to_radians()),
        "grad" => Ok(number.value * 0.9_f64.to_radians()),
        "rad" => Ok(number.value),
        "turn" => Ok(number.value * std::f64::consts::TAU),
        _ => Err(format!(
            "${param}: Expected {} to have no units.",
            number.format()
        )),
    }
}

fn trig(name: &str, positional: &[Value], keywords: &[(String, Value)]) -> ValResult<Value> {
    let args = Args::new(&["number"], positional, keywords)?;
    let angle = radians(args.number(0)?, "number")?;
    let value = match name {
        "sin" => angle.sin(),
        "cos" => angle.cos(),
        _ => angle.tan(),
    };
    Ok(Value::Number(Number::unitless(value)))
}

fn inverse_trig(
    name: &str,
    positional: &[Value],
    keywords: &[(String, Value)],
) -> ValResult<Value> {
    let args = Args::new(&["number"], positional, keywords)?;
    let value = no_units(args.number(0)?, "number")?;
    let radians = if name == "acos" {
        value.acos()
    } else {
        value.asin()
    };
    Ok(deg(radians.to_degrees()))
}

fn atan2(positional: &[Value], keywords: &[(String, Value)]) -> ValResult<Value> {
    let args = Args::new(&["y", "x"], positional, keywords)?;
    let y = args.number(0)?;
    let x = args.number(1)?;
    let (y, x) = if y.is_unitless() || x.is_unitless() {
        (y.value, x.value)
    } else {
        (x.coerce(y)?, x.value)
    };
    Ok(deg(y.atan2(x).to_degrees()))
}

// ---------------------------------------------------------------------------
// sass:map
// ---------------------------------------------------------------------------

/// `(entries of the map, remaining keys)`.
type MapAndKeys = (Vec<(Value, Value)>, Vec<Value>);

fn map(name: &str, positional: &[Value], keywords: &[(String, Value)]) -> ValResult<Value> {
    // Module signatures are all `($map, $keys...)` shaped: keyword arguments can only
    // come from a mistyped call, so report dart's "unknown argument" error directly.
    match name {
        "get" => {
            let (map, keys) = map_and_keys(positional, "$key")?;
            Ok(deep_get(&map, &keys))
        }
        "has-key" => {
            let (map, keys) = map_and_keys(positional, "$key")?;
            Ok(Value::Bool(!matches!(deep_get(&map, &keys), Value::Null)))
        }
        "set" => {
            let Some(map) = positional.first() else {
                return Err("Missing argument $map.".to_string());
            };
            let map = value::as_map(map, "map")?;
            if positional.len() < 3 {
                return Err("Expected $args to contain a value.".to_string());
            }
            let (value, keys) = positional[1..].split_last().expect("at least 2 elements");
            Ok(set_path(&map, keys, value.clone()))
        }
        "merge" => merge(positional, keywords),
        "deep-merge" => {
            let args = Args::new(&["map1", "map2"], positional, keywords)?;
            let first = value::as_map(args.req(0)?, "map1")?;
            let second = value::as_map(args.req(1)?, "map2")?;
            Ok(Value::Map(Map {
                entries: deep_merge(first, second),
            }))
        }
        "remove" => {
            let Some(map) = positional.first() else {
                return Err("Missing argument $map.".to_string());
            };
            let map = value::as_map(map, "map")?;
            let keys = &positional[1..];
            Ok(Value::Map(Map {
                entries: map
                    .into_iter()
                    .filter(|(k, _)| !keys.iter().any(|key| value::values_equal(k, key)))
                    .collect(),
            }))
        }
        "keys" | "values" => {
            let args = Args::new(&["map"], positional, keywords)?;
            let entries = value::as_map(args.req(0)?, "map")?;
            Ok(Value::List(List {
                items: entries
                    .into_iter()
                    .map(|(k, v)| if name == "keys" { k } else { v })
                    .collect(),
                sep: Sep::Comma,
                bracketed: false,
            }))
        }
        "deep-remove" => {
            let (map, keys) = map_and_keys(positional, "$key")?;
            Ok(deep_remove(&map, &keys))
        }
        _ => Err(not_in_module(Builtin::Map, name)),
    }
}

/// `map.merge($map1, $args...)`: the last argument is the map to merge in, the ones
/// before it are the nesting path (`merge($m, a, $other)` = merge `$other` under `a` of
/// `$m`).
fn merge(positional: &[Value], keywords: &[(String, Value)]) -> ValResult<Value> {
    let Some(first) = positional.first() else {
        return Err("Missing argument $map1.".to_string());
    };
    let entries = value::as_map(first, "map1")?;
    if positional.len() < 2 {
        return Err("Expected $args to contain a key.".to_string());
    }
    if positional.len() == 2 {
        let second = value::as_map(&positional[1], "map2")?;
        return Ok(Value::Map(Map {
            entries: merge_entries(entries, second),
        }));
    }
    let (second, keys) = positional[1..].split_last().expect("at least 2 elements");
    let second = value::as_map(second, "map2")?;
    let _ = keywords;
    Ok(set_path(
        &entries,
        keys,
        Value::Map(Map { entries: second }),
    ))
}

/// Take `$map` and the remaining positional arguments (`$keys...`).
fn map_and_keys(positional: &[Value], key_message: &str) -> ValResult<MapAndKeys> {
    let Some(first) = positional.first() else {
        return Err("Missing argument $map.".to_string());
    };
    let map = value::as_map(first, "map")?;
    if positional.len() < 2 {
        return Err(format!("Missing argument {key_message}."));
    }
    Ok((map, positional[1..].to_vec()))
}

fn entry_of(map: &[(Value, Value)], key: &Value) -> Option<Value> {
    map.iter()
        .find(|(k, _)| value::values_equal(k, key))
        .map(|(_, v)| v.clone())
}

fn deep_get(map: &[(Value, Value)], keys: &[Value]) -> Value {
    let mut current = Value::Map(Map {
        entries: map.to_vec(),
    });
    for key in keys {
        let entries = match current {
            Value::Map(map) => map.entries,
            Value::List(ref list) if list.items.is_empty() && !list.bracketed => Vec::new(),
            _ => return Value::Null,
        };
        match entry_of(&entries, key) {
            Some(value) => current = value,
            None => return Value::Null,
        }
    }
    current
}

fn merge_entries(
    mut first: Vec<(Value, Value)>,
    second: Vec<(Value, Value)>,
) -> Vec<(Value, Value)> {
    for (key, value) in second {
        match first.iter().position(|(k, _)| value::values_equal(k, &key)) {
            Some(i) => first[i].1 = value,
            None => first.push((key, value)),
        }
    }
    first
}

fn deep_merge(first: Vec<(Value, Value)>, second: Vec<(Value, Value)>) -> Vec<(Value, Value)> {
    let mut out = second;
    for (key, value) in first {
        match out.iter().position(|(k, _)| value::values_equal(k, &key)) {
            Some(i) => {
                // Recursively merge when both sides are maps (the inner order of the second
                // map is kept).
                if let (Value::Map(a), Value::Map(b)) = (&value, &out[i].1) {
                    out[i].1 = Value::Map(Map {
                        entries: deep_merge(a.entries.clone(), b.entries.clone()),
                    });
                }
            }
            None => out.push((key, value)),
        }
    }
    out
}

fn set_path(map: &[(Value, Value)], keys: &[Value], value: Value) -> Value {
    let Some((key, rest)) = keys.split_first() else {
        return value;
    };
    let mut entries = map.to_vec();
    let current = entry_of(&entries, key).unwrap_or_else(|| {
        Value::List(List {
            items: Vec::new(),
            sep: Sep::Space,
            bracketed: false,
        })
    });
    let nested = match current {
        Value::Map(map) => map.entries,
        _ if rest.is_empty() => Vec::new(),
        other => {
            if !rest.is_empty() {
                // The intermediate value is a scalar: dart treats this level as an empty map
                // and keeps writing.
                let _ = other;
                Vec::new()
            } else {
                Vec::new()
            }
        }
    };
    let updated = set_path(&nested, rest, value);
    match entries
        .iter()
        .position(|(k, _)| value::values_equal(k, key))
    {
        Some(i) => entries[i].1 = updated,
        None => entries.push((key.clone(), updated)),
    }
    Value::Map(Map { entries })
}

fn deep_remove(map: &[(Value, Value)], keys: &[Value]) -> Value {
    let Some((key, rest)) = keys.split_first() else {
        return Value::Map(Map {
            entries: map.to_vec(),
        });
    };
    let mut entries = map.to_vec();
    let Some(i) = entries
        .iter()
        .position(|(k, _)| value::values_equal(k, key))
    else {
        // The path does not exist: return the whole map as is (dart never conjures up an
        // empty map).
        return Value::Map(Map { entries });
    };
    if rest.is_empty() {
        entries.remove(i);
        return Value::Map(Map { entries });
    }
    let Value::Map(nested) = &entries[i].1 else {
        // The intermediate value is not a map: dart keeps it as is.
        return Value::Map(Map { entries });
    };
    entries[i].1 = deep_remove(&nested.entries.clone(), rest);
    Value::Map(Map { entries })
}

// ---------------------------------------------------------------------------
// sass:string
// ---------------------------------------------------------------------------

fn string(name: &str, positional: &[Value], keywords: &[(String, Value)]) -> ValResult<Value> {
    let global = match name {
        "length" => "str-length",
        "slice" => "str-slice",
        "index" => "str-index",
        "insert" => "str-insert",
        "quote" => "quote",
        "unquote" => "unquote",
        "to-upper-case" => "to-upper-case",
        "to-lower-case" => "to-lower-case",
        "unique-id" => return Ok(Value::Str(SassStr::unquoted(unique_id()))),
        "split" => return split(positional, keywords),
        _ => return Err(not_in_module(Builtin::String, name)),
    };
    if global == "quote" || global == "unquote" {
        let args = Args::new(&["string"], positional, keywords)?;
        if !matches!(args.req(0)?, Value::Str(_)) {
            return Err(format!(
                "$string: {} is not a string.",
                args.req(0)?.inspect()
            ));
        }
        return Ok(
            builtins::call(global, positional, keywords)?.expect("present in the global table")
        );
    }
    if global == "str-slice" {
        // dart's `str-slice` / `string.slice` report a bare `1.2 is not an int.` for
        // non-integer indexes.
        let args = Args::new(&["string", "start-at", "end-at"], positional, keywords)?;
        return slice(&args);
    }
    Ok(builtins::call(global, positional, keywords)?.expect("present in the global table"))
}

fn int_arg(number: &Number) -> ValResult<i64> {
    if (number.value - number.value.round()).abs() > value::EPSILON {
        return Err(format!("{} is not an int.", number.format()));
    }
    Ok(number.value.round() as i64)
}

/// `string.slice`: negative indexes count from the end; out-of-range indexes are
/// clamped to the bounds.
fn slice(args: &Args) -> ValResult<Value> {
    let source = args.string_owned(0)?;
    let chars = source.text.chars().collect::<Vec<_>>();
    let len = chars.len() as i64;
    let start = int_arg(args.number(1)?)?;
    let start = if start < 0 { len + start + 1 } else { start };
    let end = match args.opt_number(2)? {
        Some(number) => {
            let end = int_arg(number)?;
            if end < 0 { len + end + 1 } else { end }
        }
        None => len,
    };
    let start = start.max(1);
    let end = end.min(len);
    let text = if start > end {
        String::new()
    } else {
        chars[(start - 1) as usize..end as usize].iter().collect()
    };
    Ok(Value::Str(SassStr {
        text,
        quoted: source.quoted,
        escape: false,
    }))
}

/// `unique-id()`: dart returns a random 7-character lowercase alphanumeric string and
/// only guarantees uniqueness within the process.
pub(crate) fn unique_id() -> String {
    use std::cell::Cell;
    thread_local! {
        static COUNTER: Cell<u64> = const { Cell::new(0) };
    }
    let n = COUNTER.with(|c| {
        let n = c.get().wrapping_add(1);
        c.set(n);
        n
    });
    // Use a random value as the prefix so repeated compilations do not collide.
    let mut value = (next_random() * 1e15) as u64 ^ n.wrapping_mul(0x9e37_79b9);
    let mut out = String::with_capacity(7);
    out.push('u');
    for _ in 0..6 {
        let index = (value % 36) as u8;
        value /= 36;
        out.push(match index {
            0..=9 => (b'0' + index) as char,
            _ => (b'a' + index - 10) as char,
        });
    }
    out
}

/// `string.split($string, $separator, $limit: null)` (1.93.2): a bracketed comma list.
fn split(positional: &[Value], keywords: &[(String, Value)]) -> ValResult<Value> {
    let args = Args::new(&["string", "separator", "limit"], positional, keywords)?;
    let text = match args.req(0)? {
        Value::Str(text) => text.text.clone(),
        other => return Err(format!("$string: {} is not a string.", other.inspect())),
    };
    let separator = match args.req(1)? {
        Value::Str(text) => text.text.clone(),
        other => return Err(format!("$separator: {} is not a string.", other.inspect())),
    };
    let limit = match args.opt_number(2)? {
        Some(number) if number.value >= 1.0 => Some(number.value as usize),
        Some(number) if number.value < 1.0 => {
            return Err(format!(
                "$limit: Expected {} to be greater than or equal to 1.",
                number.format()
            ));
        }
        _ => None,
    };
    let pieces: Vec<String> = if text.is_empty() {
        Vec::new()
    } else if separator.is_empty() {
        text.chars().map(|c| c.to_string()).collect()
    } else {
        let mut parts = Vec::new();
        let mut rest = text.as_str();
        // `$limit` is an upper bound on the **number of splits** (observed with dart
        // 1.93.2: limit 2 -> 3 parts).
        let mut splits = 0;
        loop {
            if let Some(limit) = limit {
                if splits >= limit {
                    parts.push(rest.to_string());
                    break;
                }
            }
            match rest.find(&separator) {
                Some(index) => {
                    parts.push(rest[..index].to_string());
                    rest = &rest[index + separator.len()..];
                    splits += 1;
                }
                None => {
                    parts.push(rest.to_string());
                    break;
                }
            }
        }
        parts
    };
    Ok(Value::List(List {
        items: pieces
            .into_iter()
            .map(|piece| Value::Str(SassStr::quoted(piece)))
            .collect(),
        sep: Sep::Comma,
        bracketed: true,
    }))
}

// ---------------------------------------------------------------------------
// sass:list
// ---------------------------------------------------------------------------

fn list(name: &str, positional: &[Value], keywords: &[(String, Value)]) -> ValResult<Value> {
    match name {
        "length" | "nth" | "set-nth" | "join" | "append" | "zip" | "index" => {
            Ok(builtins::call(name, positional, keywords)?.expect("present in the global table"))
        }
        "separator" => Ok(builtins::call("list-separator", positional, keywords)?
            .expect("present in the global table")),
        "is-bracketed" => {
            let args = Args::new(&["list"], positional, keywords)?;
            Ok(Value::Bool(matches!(
                args.req(0)?,
                Value::List(list) if list.bracketed
            )))
        }
        "slash" => {
            if let Some((name, _)) = keywords.first() {
                return Err(format!("No argument named ${name}."));
            }
            Ok(Value::List(List {
                items: positional.to_vec(),
                sep: Sep::Slash,
                bracketed: false,
            }))
        }
        _ => Err(not_in_module(Builtin::List, name)),
    }
}

// ---------------------------------------------------------------------------
// sass:color
// ---------------------------------------------------------------------------

fn color(name: &str, positional: &[Value], keywords: &[(String, Value)]) -> ValResult<Value> {
    if COLOR_MODERN.contains(&name) {
        return modern(name, positional, keywords);
    }
    let global = match name {
        "adjust" => "adjust-color",
        "scale" => "scale-color",
        "change" => "change-color",
        "opacity" => "alpha",
        "hwb" => return hwb(positional, keywords),
        "whiteness" | "blackness" => return hwb_component(name, positional, keywords),
        other => other,
    };
    Ok(builtins::call(global, positional, keywords)?.expect("present in the global table"))
}

/// `hwb($hue, $whiteness, $blackness, $alpha: 1)` -> color.
fn hwb(positional: &[Value], keywords: &[(String, Value)]) -> ValResult<Value> {
    let args = Args::new(
        &["hue", "whiteness", "blackness", "alpha"],
        positional,
        keywords,
    )?;
    let hue = args.number(0)?.angle_degrees("hue")?;
    let white = args.number(1)?.value_in_range(0.0, 100.0, "whiteness")? / 100.0;
    let black = args.number(2)?.value_in_range(0.0, 100.0, "blackness")? / 100.0;
    let alpha = match args.opt_number(3)? {
        Some(number) => number.value_in_range(0.0, 1.0, "alpha")?,
        None => 1.0,
    };
    let mut color = if white + black >= 1.0 {
        let gray = white / (white + black);
        Color::new(gray * 255.0, gray * 255.0, gray * 255.0, alpha)
    } else {
        let (r, g, b) = hue_to_rgb(hue, 1.0, 0.5);
        let scale = 1.0 - white - black;
        Color::new(
            (r * scale + white) * 255.0,
            (g * scale + white) * 255.0,
            (b * scale + white) * 255.0,
            alpha,
        )
    };
    color.space = Some(Space::Hwb);
    color.channels = Some([Some(hue), Some(white * 100.0), Some(black * 100.0)]);
    Ok(Value::Color(color))
}

fn hwb_component(
    name: &str,
    positional: &[Value],
    keywords: &[(String, Value)],
) -> ValResult<Value> {
    let args = Args::new(&["color"], positional, keywords)?;
    let color = args.color(0)?;
    let (r, g, b) = color.channels_f64();
    let (low, high) = (r.min(g).min(b), r.max(g).max(b));
    let value = if name == "whiteness" {
        low / 255.0 * 100.0
    } else {
        (255.0 - high) / 255.0 * 100.0
    };
    Ok(Value::Number(Number::with_unit(value, "%")))
}

/// HSL -> RGB (h in degrees, s/l in 0..1).
fn hue_to_rgb(hue: f64, saturation: f64, lightness: f64) -> (f64, f64, f64) {
    let hue = hue.rem_euclid(360.0) / 360.0;
    if saturation == 0.0 {
        return (lightness, lightness, lightness);
    }
    let q = if lightness < 0.5 {
        lightness * (1.0 + saturation)
    } else {
        lightness + saturation - lightness * saturation
    };
    let p = 2.0 * lightness - q;
    let channel = |mut t: f64| {
        if t < 0.0 {
            t += 1.0;
        }
        if t > 1.0 {
            t -= 1.0;
        }
        if t < 1.0 / 6.0 {
            p + (q - p) * 6.0 * t
        } else if t < 0.5 {
            q
        } else if t < 2.0 / 3.0 {
            p + (q - p) * (2.0 / 3.0 - t) * 6.0
        } else {
            p
        }
    };
    (
        channel(hue + 1.0 / 3.0),
        channel(hue),
        channel(hue - 1.0 / 3.0),
    )
}

// ---- modern color API added in 1.93.2 --------------------------------------

/// Space argument: an unquoted identifier (`rgb` / `hsl` / `oklch` …).
fn space_arg(value: &Value, param: &str) -> ValResult<Space> {
    let text = match value {
        Value::Str(text) => text.text.clone(),
        other => other.inspect(),
    };
    Space::from_name(&text).ok_or_else(|| format!("${param}: Unknown color space \"{text}\"."))
}

fn channel_name(value: &Value) -> ValResult<String> {
    match value {
        Value::Str(text) => Ok(text.text.to_ascii_lowercase()),
        other => Ok(other.inspect().to_ascii_lowercase()),
    }
}

fn color_of(value: &Value) -> ValResult<&Color> {
    match value {
        Value::Color(color) => Ok(color),
        other => Err(format!("$color: {} is not a color.", other.inspect())),
    }
}

/// Channel index: `alpha` occupies its own slot (3), and an unknown name reports
/// dart's message.
///
/// dart uses two different messages in `color.channel` and in
/// `is-missing`/`is-powerless`: the former is `Color #036 has no channel named nope.`
/// (no quotes), the latter is `Color #123456 doesn't have a channel named "hue".`
/// (with quotes).
fn channel_slot(color: &Color, space: Space, name: &str, verbatim: bool) -> ValResult<usize> {
    if name == "alpha" {
        return Ok(3);
    }
    space.channel_index(name).ok_or_else(|| {
        let shown = Value::Color(color.to_space(space)).inspect();
        if verbatim {
            format!("$channel: Color {shown} has no channel named {name}.")
        } else {
            format!("$channel: Color {shown} doesn't have a channel named \"{name}\".")
        }
    })
}

fn channel_value(color: &Color, space: Space, slot: usize) -> Option<f64> {
    if slot == 3 {
        return Some(color.a);
    }
    color.to_space(space).origin_channels()[slot]
}

fn number_or_percent(value: f64, unit: Option<&str>) -> Value {
    Value::Number(match unit {
        Some(unit) => Number::with_unit(value, unit),
        None => Number::unitless(value),
    })
}

fn in_gamut(color: &Color, space: Space) -> bool {
    let channels = color.to_space(space).origin_channels();
    (0..3).all(|i| match (channels[i], space.channel_bounds(i)) {
        (None, _) => true,
        (Some(_), None) => true,
        (Some(value), Some((low, high))) => value >= low && value <= high,
    })
}

fn powerless(color: &Color, space: Space, slot: usize) -> bool {
    let channels = color.to_space(space).origin_channels();
    let get = |i: usize| channels[i].unwrap_or(0.0);
    match (space, slot) {
        (Space::Hsl, 0) => get(1) == 0.0,
        (Space::Hsl, 1) => matches!(get(2), 0.0) || get(2) >= 100.0,
        (Space::Hwb, 0) => get(1) + get(2) >= 100.0,
        (Space::Lch | Space::Oklch, 0) => get(1) == 0.0,
        (Space::Lch | Space::Oklch, 1) => matches!(get(0), 0.0) || get(0) >= 100.0,
        (Space::Lch | Space::Oklch, 2) => get(1) == 0.0,
        _ => false,
    }
}

fn modern(name: &str, positional: &[Value], keywords: &[(String, Value)]) -> ValResult<Value> {
    match name {
        "space" => {
            let args = Args::new(&["color"], positional, keywords)?;
            Ok(Value::Str(SassStr::unquoted(
                color_of(args.req(0)?)?.space().name(),
            )))
        }
        "is-legacy" => {
            let args = Args::new(&["color"], positional, keywords)?;
            Ok(Value::Bool(color_of(args.req(0)?)?.space().is_legacy()))
        }
        "to-space" => {
            let args = Args::new(&["color", "space"], positional, keywords)?;
            let color = color_of(args.req(0)?)?;
            Ok(Value::Color(
                color.to_space(space_arg(args.req(1)?, "space")?),
            ))
        }
        "channel" => {
            let args = Args::new(&["color", "channel", "space"], positional, keywords)?;
            let color = color_of(args.req(0)?)?;
            let space = match args.at(2) {
                Some(value) => space_arg(value, "space")?,
                None => color.space(),
            };
            let slot = channel_slot(color, space, &channel_name(args.req(1)?)?, true)?;
            let value = channel_value(color, space, slot).unwrap_or(0.0);
            let unit = if slot == 3 {
                None
            } else {
                space.channel_unit(slot)
            };
            Ok(number_or_percent(value, unit))
        }
        "is-missing" => {
            let args = Args::new(&["color", "channel"], positional, keywords)?;
            let color = color_of(args.req(0)?)?;
            let space = color.space();
            let slot = channel_slot(color, space, &channel_name(args.req(1)?)?, false)?;
            Ok(Value::Bool(channel_value(color, space, slot).is_none()))
        }
        "is-powerless" => {
            let args = Args::new(&["color", "channel", "space"], positional, keywords)?;
            let color = color_of(args.req(0)?)?;
            let space = match args.at(2) {
                Some(value) => space_arg(value, "space")?,
                None => color.space(),
            };
            let slot = channel_slot(color, space, &channel_name(args.req(1)?)?, false)?;
            Ok(Value::Bool(slot != 3 && powerless(color, space, slot)))
        }
        "is-in-gamut" => {
            let args = Args::new(&["color", "space"], positional, keywords)?;
            let color = color_of(args.req(0)?)?;
            let space = match args.at(1) {
                Some(value) => space_arg(value, "space")?,
                None => color.space(),
            };
            Ok(Value::Bool(in_gamut(color, space)))
        }
        "to-gamut" => {
            let args = Args::new(&["color", "method", "space"], positional, keywords)?;
            let color = color_of(args.req(0)?)?;
            let method = match args.at(1) {
                Some(Value::Str(text)) => text.text.to_ascii_lowercase(),
                Some(other) => {
                    return Err(format!("$method: {} is not a string.", other.inspect()));
                }
                None => {
                    return Err("$method: color.to-gamut() requires a $method argument for \
                         forwards-compatibility with changes in the CSS spec."
                        .to_string());
                }
            };
            let target = match args.at(2) {
                Some(value) => space_arg(value, "space")?,
                None => color.space(),
            };
            if in_gamut(color, target) {
                return Ok(Value::Color(color.clone()));
            }
            let own = color.space();
            let channels = color.origin_channels();
            let filled = [
                channels[0].unwrap_or(0.0),
                channels[1].unwrap_or(0.0),
                channels[2].unwrap_or(0.0),
            ];
            let converted = color_space::convert(own, target, math_channel(own, filled));
            let mapped = match method.as_str() {
                "clip" => color_space::clip_to_gamut(target, converted),
                "local-minde" => color_space::local_minde_to_gamut(target, converted),
                other => {
                    return Err(format!(
                        "$method: Only clip and local-minde are supported, but \"{other}\" was given."
                    ));
                }
            };
            let back = Color::math_to_display(own, color_space::convert(target, own, mapped));
            let mut result =
                Color::from_space(own, [Some(back[0]), Some(back[1]), Some(back[2])], color.a);
            if own.is_legacy() {
                result = Color::new(color.r, color.g, color.b, color.a);
            }
            Ok(Value::Color(result))
        }
        "same" => {
            let args = Args::new(&["color1", "color2"], positional, keywords)?;
            let first = color_of(args.req(0)?)?;
            let second = color_of(args.req(1)?)?;
            Ok(Value::Bool(same(first, second)))
        }
        _ => Err(not_in_module(Builtin::Color, name)),
    }
}

/// Display units -> the math units of `color_space` (`rgb` 0..255, `oklab` lightness
/// 0..100).
fn math_channel(space: Space, ch: [f64; 3]) -> [f64; 3] {
    match space {
        Space::Rgb => ch.map(|c| c / 255.0),
        Space::Oklab | Space::Oklch => [ch[0] / 100.0, ch[1], ch[2]],
        _ => ch,
    }
}

fn same(first: &Color, second: &Color) -> bool {
    let (a_space, b_space) = (first.space(), second.space());
    // Two legacy colors are compared by their **unrounded RGB** (dart:
    // `hsl(0, 0%, 50%)` = `rgb(127.5, …)`).
    if a_space.is_legacy() && b_space.is_legacy() {
        let (ar, ag, ab) = first.channels_f64();
        let (br, bg, bb) = second.channels_f64();
        return (ar - br).abs() < 1e-11
            && (ag - bg).abs() < 1e-11
            && (ab - bb).abs() < 1e-11
            && (first.a - second.a).abs() < 1e-11;
    }
    // Otherwise convert to the space of the first color and compare channels (legacy
    // <-> modern is allowed too; the position is in color1's space).
    let rhs = second.to_space(a_space);
    let (lhs, rhs) = (first.origin_channels(), rhs.origin_channels());
    (0..3).all(|i| match (lhs[i], rhs[i]) {
        (Some(a), Some(b)) => (a - b).abs() < 1e-9,
        (None, None) => true,
        _ => false,
    }) && (first.a - second.a).abs() < 1e-11
}

// ---------------------------------------------------------------------------
// sass:selector
// ---------------------------------------------------------------------------

fn selector(name: &str, positional: &[Value], keywords: &[(String, Value)]) -> ValResult<Value> {
    match name {
        "parse" => {
            let args = Args::new(&["selector"], positional, keywords)?;
            let text = selector_arg(args.req(0)?, Some("selector"), false)?;
            Ok(selector_value(&parse_list(&text, "selector")?))
        }
        "nest" | "append" => combine(name, positional, keywords),
        "unify" => {
            let args = Args::new(&["selector1", "selector2"], positional, keywords)?;
            let first = parse_list(
                &selector_arg(args.req(0)?, Some("selector1"), false)?,
                "selector1",
            )?;
            let second = parse_list(
                &selector_arg(args.req(1)?, Some("selector2"), false)?,
                "selector2",
            )?;
            match unify(&first, &second) {
                Some(sels) => Ok(selector_value(&sels)),
                None => Ok(Value::Null),
            }
        }
        "simple-selectors" => {
            let args = Args::new(&["selector"], positional, keywords)?;
            let text = selector_arg(args.req(0)?, Some("selector"), false)?;
            Ok(Value::List(List {
                items: simple_selectors(&text, "selector")?
                    .into_iter()
                    .map(|s| Value::Str(SassStr::unquoted(s)))
                    .collect(),
                sep: Sep::Comma,
                bracketed: false,
            }))
        }
        "is-superselector" => {
            let args = Args::new(&["super", "sub"], positional, keywords)?;
            let sup = parse_list(&selector_arg(args.req(0)?, Some("super"), false)?, "super")?;
            let sub = parse_list(&selector_arg(args.req(1)?, Some("sub"), false)?, "sub")?;
            Ok(Value::Bool(sup.iter().all(|s| {
                sub.iter()
                    .any(|candidate| complex_superselector(s, candidate))
            })))
        }
        "extend" => {
            let args = Args::new(&["selector", "extendee", "extender"], positional, keywords)?;
            let base = selector_arg(args.req(0)?, Some("selector"), false)?;
            let extendee = selector_arg(args.req(1)?, Some("extendee"), false)?;
            let extender = selector_arg(args.req(2)?, Some("extender"), false)?;
            Ok(replace_value(&base, &extendee, &extender, true)?)
        }
        "replace" => {
            let args = Args::new(
                &["selector", "original", "replacement"],
                positional,
                keywords,
            )?;
            let base = selector_arg(args.req(0)?, Some("selector"), false)?;
            let original = selector_arg(args.req(1)?, Some("original"), false)?;
            let replacement = selector_arg(args.req(2)?, Some("replacement"), false)?;
            Ok(replace_value(&base, &original, &replacement, false)?)
        }
        _ => Err(not_in_module(Builtin::Selector, name)),
    }
}

/// Parse the selector text in the arguments. A `param` of `None` means dart adds no
/// parameter name prefix (`selector.nest` / `selector.append` use the `$selectors...`
/// expansion).
fn selector_arg(value: &Value, param: Option<&str>, allow_parent: bool) -> ValResult<String> {
    let prefix = param.map(|name| format!("${name}: ")).unwrap_or_default();
    let text = match value {
        Value::Str(text) => text.text.clone(),
        other => {
            return Err(format!(
                "{prefix}{} is not a valid selector: it must be a string,\n\
                 a list of strings, or a list of lists of strings.",
                other.inspect()
            ));
        }
    };
    if !allow_parent && text.contains('&') {
        return Err(format!("{prefix}Parent selectors aren't allowed here."));
    }
    Ok(text)
}

/// Parse a "comma-separated list of complex selectors"; on failure report
/// `$selector: expected selector.`
fn parse_list(text: &str, param: &str) -> ValResult<Vec<Sel>> {
    let expected = || format!("${param}: expected selector.");
    let text = text.trim();
    if text.is_empty() {
        return Err(expected());
    }
    let mut out = Vec::new();
    for part in split_top_level_commas(text) {
        let part = part.trim();
        let Some(sel) = parse_complex(part) else {
            return Err(expected());
        };
        out.push(sel);
    }
    if out.is_empty() {
        return Err(expected());
    }
    Ok(out)
}

/// Parse one complex selector: quotes of attribute selectors are dropped per the
/// identifier rules (dart's `AttributeSelector` serialization: `a[href="x"]` ->
/// `a[href=x]`).
fn parse_complex(text: &str) -> Option<Sel> {
    if text.is_empty() || text.contains(['!', ';', '{', '}']) {
        return None;
    }
    let mut sel = parse_sel(text, false)?;
    for part in &mut sel.parts {
        part.compound.simples = part
            .compound
            .simples
            .iter()
            .map(|simple| unquote_attribute(simple))
            .collect();
    }
    Some(sel)
}

/// `[href="x"]` -> `[href=x]` (quotes are only dropped when the value itself is a CSS
/// identifier).
fn unquote_attribute(simple: &str) -> String {
    if !simple.starts_with('[') || !simple.ends_with(']') {
        return simple.to_string();
    }
    let bytes = simple.as_bytes();
    let Some(eq) = bytes.iter().position(|b| *b == b'=') else {
        return simple.to_string();
    };
    let Some(quote) = bytes
        .get(eq + 1)
        .copied()
        .filter(|b| *b == b'"' || *b == b'\'')
    else {
        return simple.to_string();
    };
    let Some(end) = bytes[eq + 2..].iter().rposition(|b| *b == quote) else {
        return simple.to_string();
    };
    let inner = &simple[eq + 2..eq + 2 + end];
    if !value::is_css_identifier(inner) {
        return simple.to_string();
    }
    format!("[{}={}]", &simple[1..eq], inner)
}

fn split_top_level_commas(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    for (i, ch) in text.char_indices() {
        match ch {
            '(' | '[' => depth += 1,
            ')' | ']' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                out.push(&text[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&text[start..]);
    out
}

/// `Sel` -> Sass value: one string per complex selector, combinators become their own
/// items (that is dart's `ComplexSelector.asSassList`; item 2 of
/// `selector.parse(".a > .b")` is `>`).
fn sel_value(sel: &Sel) -> Value {
    let mut items = Vec::new();
    for part in &sel.parts {
        if let Some(combinator) = part.combinator {
            items.push(Value::Str(SassStr::unquoted(combinator.to_string())));
        }
        items.push(Value::Str(SassStr::unquoted(part.compound.render())));
    }
    Value::List(List {
        items,
        sep: Sep::Space,
        bracketed: false,
    })
}

fn selector_value(sels: &[Sel]) -> Value {
    Value::List(List {
        items: sels.iter().map(sel_value).collect(),
        sep: Sep::Comma,
        bracketed: false,
    })
}

fn simple_selectors(text: &str, param: &str) -> ValResult<Vec<String>> {
    let sels = parse_list(text, param)?;
    let expected = || format!("${param}: expected selector.");
    let [sel] = sels.as_slice() else {
        return Err(expected());
    };
    let [part] = sel.parts.as_slice() else {
        return Err(expected());
    };
    if part.combinator.is_some() {
        return Err(expected());
    }
    Ok(part.compound.simples.clone())
}

/// `selector.nest` / `selector.append`: both combine several selectors as a cartesian
/// product; they only differ in how they join (descendant vs compound).
fn combine(name: &str, positional: &[Value], keywords: &[(String, Value)]) -> ValResult<Value> {
    if let Some((name, _)) = keywords.first() {
        return Err(format!("No argument named ${name}."));
    }
    let Some(first) = positional.first() else {
        return Err("At least one selector must be passed.".to_string());
    };
    let allow_parent = name == "nest";
    let mut result = parse_list(&selector_arg(first, None, allow_parent)?, "selectors")?;
    for value in &positional[1..] {
        let next = parse_list(&selector_arg(value, None, allow_parent)?, "selectors")?;
        result = if name == "nest" {
            nest(&result, &next)
        } else {
            append(&result, &next)
        };
    }
    Ok(selector_value(&result))
}

fn nest(left: &[Sel], right: &[Sel]) -> Vec<Sel> {
    let mut out = Vec::new();
    for outer in left {
        let outer_text = outer.render();
        for inner in right {
            let text = if inner.render().contains('&') {
                inner.render().replace('&', &outer_text)
            } else {
                format!("{outer_text} {}", inner.render())
            };
            if let Some(sel) = parse_complex(&text) {
                out.push(sel);
            }
        }
    }
    out
}

fn append(left: &[Sel], right: &[Sel]) -> Vec<Sel> {
    let mut out = Vec::new();
    for outer in left {
        for inner in right {
            let mut parts = outer.parts.clone();
            let Some(last) = parts.last_mut() else {
                continue;
            };
            for simple in &inner.parts[0].compound.simples {
                if !last.compound.simples.contains(simple) {
                    last.compound.simples.push(simple.clone());
                }
            }
            for part in &inner.parts[1..] {
                parts.push(part.clone());
            }
            out.push(Sel {
                parts,
                line_break: false,
            });
        }
    }
    out
}

/// Sort weight of a simple selector inside a compound selector (dart's serialization
/// order): type/universal < id < class/attribute/placeholder < pseudo-class <
/// pseudo-element.
fn simple_rank(simple: &str) -> u8 {
    match simple.chars().next() {
        Some('#') => 1,
        Some('.') | Some('%') | Some('[') => 2,
        Some(':') if simple.starts_with("::") => 4,
        Some(':') => 3,
        _ => 0,
    }
}

/// The type selector inside a compound selector (`a` / `*` / `ns|a`).
fn type_selector(compound: &Compound) -> Option<&String> {
    compound.simples.iter().find(|simple| {
        simple
            .chars()
            .next()
            .is_some_and(|ch| ch.is_alphanumeric() || ch == '*' || ch == '|')
    })
}

/// Merge compound selectors (dart's `unifyCompound`): the type selectors must agree,
/// otherwise they cannot be merged.
fn merge_compound(first: &Compound, second: &Compound) -> Option<Compound> {
    let left = type_selector(first);
    let right = type_selector(second);
    if let (Some(x), Some(y)) = (&left, &right) {
        if x != y && x.as_str() != "*" && y.as_str() != "*" {
            return None;
        }
    }
    let mut simples = first.simples.clone();
    for simple in &second.simples {
        if !simples.contains(simple) {
            simples.push(simple.clone());
        }
    }
    // When a type selector is present, the universal `*` is no longer output.
    if simples.len() > 1 {
        let has_type = simples.iter().any(|simple| {
            simple
                .chars()
                .next()
                .is_some_and(|ch| ch.is_alphanumeric() || ch == '|')
        });
        if has_type {
            simples.retain(|simple| simple != "*");
        }
    }
    simples.sort_by_key(|simple| simple_rank(simple));
    Some(Compound { simples })
}

fn unify(left: &[Sel], right: &[Sel]) -> Option<Vec<Sel>> {
    let mut out: Vec<Sel> = Vec::new();
    for a in left {
        for b in right {
            for sel in unify_pair(a, b) {
                if !out.iter().any(|kept| kept.parts == sel.parts) {
                    out.push(sel);
                }
            }
        }
    }
    (!out.is_empty()).then_some(out)
}

fn unify_pair(a: &Sel, b: &Sel) -> Vec<Sel> {
    let (Some(a_last), Some(b_last)) = (a.parts.last(), b.parts.last()) else {
        return Vec::new();
    };
    let combinator = match (a_last.combinator, b_last.combinator) {
        (Some(x), Some(y)) if x != y => return Vec::new(),
        (x, y) => x.or(y),
    };
    let a_prefix = &a.parts[..a.parts.len() - 1];
    let b_prefix = &b.parts[..b.parts.len() - 1];
    let Some(compound) = merge_compound(&a_last.compound, &b_last.compound) else {
        return Vec::new();
    };
    // When both sides attach their tail with the same explicit combinator, the two
    // prefixes merge vertically (observed with dart:
    // `.a > .b` + `.c > .d` -> `.c.a > .b.d`).
    let vertical = a_last.combinator.is_some() && a_last.combinator == b_last.combinator;
    let prefixes = match (a_prefix.is_empty(), b_prefix.is_empty()) {
        (true, true) => vec![Vec::new()],
        (false, true) => vec![a_prefix.to_vec()],
        (true, false) => vec![b_prefix.to_vec()],
        (false, false) => match combine_prefixes(a_prefix, b_prefix, vertical) {
            Some(prefixes) => prefixes,
            None => return Vec::new(),
        },
    };
    prefixes
        .into_iter()
        .map(|mut parts| {
            parts.push(super::extend::Part {
                combinator,
                compound: compound.clone(),
            });
            Sel {
                parts,
                line_break: false,
            }
        })
        .collect()
}

/// Weaving of two prefixes: when both attach their tail with the same explicit
/// combinator they merge vertically, otherwise each order gets its own copy.
fn combine_prefixes(
    a: &[super::extend::Part],
    b: &[super::extend::Part],
    vertical: bool,
) -> Option<Vec<Vec<super::extend::Part>>> {
    if vertical && a.len() == b.len() {
        let mut parts = Vec::with_capacity(a.len());
        for (y, x) in b.iter().zip(a) {
            parts.push(super::extend::Part {
                combinator: x.combinator,
                compound: merge_compound(&y.compound, &x.compound)?,
            });
        }
        return Some(vec![parts]);
    }
    let mut first = a.to_vec();
    first.extend(b.to_vec());
    let mut second = b.to_vec();
    second.extend(a.to_vec());
    Some(vec![first, second])
}
fn complex_superselector(sup: &Sel, sub: &Sel) -> bool {
    let (Some(sup_last), Some(sub_last)) = (sup.parts.last(), sub.parts.last()) else {
        return false;
    };
    if !compound_superselector(&sup_last.compound, &sub_last.compound) {
        return false;
    }
    let sup_combinator = sup_last.combinator;
    match sup_combinator {
        Some(c) => {
            if sub_last.combinator != Some(c) || sub.parts.len() < 2 || sup.parts.len() < 2 {
                return false;
            }
            complex_superselector(
                &Sel {
                    parts: sup.parts[..sup.parts.len() - 1].to_vec(),
                    line_break: false,
                },
                &Sel {
                    parts: sub.parts[..sub.parts.len() - 1].to_vec(),
                    line_break: false,
                },
            )
        }
        None => {
            if sup.parts.len() == 1 {
                return true;
            }
            // Descendant combinator: the prefix of sup must match some suffix of the sub
            // prefix in order.
            let sup_prefix = &sup.parts[..sup.parts.len() - 1];
            let sub_prefix = &sub.parts[..sub.parts.len() - 1];
            if sub_prefix.is_empty() {
                return false;
            }
            for start in 0..sub_prefix.len() {
                if suffix_super(sup_prefix, &sub_prefix[start..]) {
                    return true;
                }
            }
            false
        }
    }
}

fn suffix_super(sup: &[super::extend::Part], sub: &[super::extend::Part]) -> bool {
    if sup.is_empty() {
        return true;
    }
    if sub.is_empty() || sup.len() > sub.len() {
        return false;
    }
    let offset = sub.len() - sup.len();
    for (i, part) in sup.iter().enumerate() {
        let candidate = &sub[offset + i];
        if !compound_superselector(&part.compound, &candidate.compound) {
            return false;
        }
        // Combinator: when sup has one it must match exactly; a descendant combinator in
        // sup is more lenient.
        if let Some(c) = part.combinator {
            if candidate.combinator != Some(c) {
                return false;
            }
        }
    }
    true
}

fn compound_superselector(sup: &Compound, sub: &Compound) -> bool {
    let type_of = |compound: &Compound| {
        compound.simples.iter().find_map(|simple| {
            simple
                .chars()
                .next()
                .filter(|ch| ch.is_alphanumeric() || *ch == '*' || *ch == '|')
                .map(|_| simple.clone())
        })
    };
    let sup_type = type_of(sup);
    let sub_type = type_of(sub);
    if let Some(sup_type) = &sup_type {
        if sup_type != "*" && sub_type.as_ref() != Some(sup_type) {
            return false;
        }
    }
    sup.simples.iter().all(|simple| {
        if simple == "*" || Some(simple) == sup_type.as_ref() && sub_type.is_some() {
            true
        } else {
            sub.simples.contains(simple)
                || (simple.starts_with(':') && sub.simples.contains(simple))
        }
    })
}

/// `selector.replace` / `selector.extend`: replace the matching compound selectors with
/// `replacement` (extend keeps the original selector, replace does not). dart replaces
/// **all** matching positions.
fn replace_value(
    base: &str,
    original: &str,
    replacement: &str,
    keep_original: bool,
) -> ValResult<Value> {
    let base_sels = parse_list(base, "selector")?;
    let originals = parse_list(original, "original")?;
    let replacements = parse_list(replacement, "replacement")?;
    let mut out = Vec::new();
    for sel in &base_sels {
        if keep_original {
            out.push(sel.clone());
        }
        let mut variants: Vec<Vec<super::extend::Part>> = vec![Vec::new()];
        for part in &sel.parts {
            let hit = originals.iter().any(|target| {
                matches!(target.parts.as_slice(), [single] if single.combinator.is_none()
                    && compound_superselector(&single.compound, &part.compound))
            });
            let mut next = Vec::new();
            for prefix in &variants {
                if hit {
                    for replacement in &replacements {
                        let mut parts = prefix.clone();
                        for (index, repl) in replacement.parts.iter().enumerate() {
                            let mut repl = repl.clone();
                            if index == 0 {
                                repl.combinator = part.combinator;
                            }
                            parts.push(repl);
                        }
                        next.push(parts);
                    }
                } else {
                    let mut parts = prefix.clone();
                    parts.push(part.clone());
                    next.push(parts);
                }
            }
            variants = next;
        }
        for parts in variants {
            let sel = Sel {
                parts,
                line_break: false,
            };
            if !out.iter().any(|kept: &Sel| kept.parts == sel.parts) {
                out.push(sel);
            }
        }
    }
    Ok(selector_value(&out))
}
