//! The typed Sass value model: numbers (compound units), colors, strings, list / map,
//! function values.
//!
//! Unlike the string-based values of less, every value here carries semantics, and the
//! serialization rules match dart-sass 1.35.2: numbers use 10 decimal places, color
//! literals keep their spelling, and strings pick the quote style with fewer escapes.

use std::cell::Cell;
use std::rc::Rc;

use oxc_css_parser::ast::*;

use super::color_space::{self, Space};

/// The value model. `Raw` carries text that cannot be interpreted structurally
/// (`progid:`, url, unicode-range and so on).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Value {
    Number(Number),
    Color(Color),
    Str(SassStr),
    Bool(bool),
    Null,
    List(List),
    Map(Map),
    ArgList(ArgList),
    Function(Function),
    /// A first-class function value obtained from `get-function("name")` (the target of
    /// `call()`).
    FuncRef(FuncRef),
    Raw(String),
}

/// A first-class function: name + origin (built-in module member / user module namespace
/// / plain CSS function).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FuncRef {
    pub(crate) name: String,
    /// A built-in module member (`meta.get-function("div", $module: "math")`).
    pub(crate) module: Option<crate::scss::modules::Builtin>,
    /// The namespace of a user module.
    pub(crate) namespace: Option<String>,
    /// `$css: true`: emit a plain CSS function call as is.
    pub(crate) css: bool,
}

/// list separator. Comma/space/slash output `, ` / ` ` / ` / ` respectively.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Sep {
    Comma,
    Space,
    /// A true slash list (`list.slash()`), which dart serializes as `1 / 2`.
    Slash,
    /// `a/b` as written in the source: dart treats it as a "slash number / string" and
    /// serializes it without spaces.
    SlashTight,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct List {
    pub(crate) items: Vec<Value>,
    pub(crate) sep: Sep,
    pub(crate) bracketed: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Map {
    pub(crate) entries: Vec<(Value, Value)>,
}

/// Parameter list (`$args...`): the extra positional arguments + the keyword arguments not
/// claimed by any parameter.
/// `keywords_accessed` records "whether anyone read the keyword map" (`keywords($args)` or
/// another `...` expansion), and dart uses it to decide whether to report "extra named
/// arguments" (observed: once read, nothing is reported).
#[derive(Debug, Clone, Default)]
pub(crate) struct ArgList {
    pub(crate) positional: Vec<Value>,
    pub(crate) keywords: Vec<(String, Value)>,
    pub(crate) sep: Option<Sep>,
    pub(crate) keywords_accessed: Rc<Cell<bool>>,
}

impl PartialEq for ArgList {
    fn eq(&self, other: &Self) -> bool {
        self.positional == other.positional
            && self.keywords == other.keywords
            && self.sep == other.sep
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Function {
    pub(crate) name: String,
    /// Arguments: usually a comma list.
    pub(crate) args: Box<Value>,
}

/// Remove the whitespace on both sides of `=` (`opacity = 65` in function arguments ->
/// `opacity=65`).
pub(crate) fn strip_eq_spaces(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pending_space = false;
    let mut after_eq = false;
    let mut quote: Option<char> = None;
    for ch in text.chars() {
        if let Some(q) = quote {
            out.push(ch);
            if ch == q {
                quote = None;
            }
            continue;
        }
        if ch == '\'' || ch == '"' {
            quote = Some(ch);
            out.push(ch);
            continue;
        }
        if ch.is_whitespace() {
            if !after_eq {
                pending_space = true;
            }
            continue;
        }
        if ch == '=' {
            while out.ends_with(' ') {
                out.pop();
            }
            out.push('=');
            pending_space = false;
            after_eq = true;
            continue;
        }
        after_eq = false;
        if pending_space && !out.is_empty() && !out.ends_with(' ') {
            out.push(' ');
        }
        pending_space = false;
        out.push(ch);
    }
    out
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SassStr {
    pub(crate) text: String,
    pub(crate) quoted: bool,
    /// An identifier decoded from CSS source: re-serialized per the CSS escaping rules
    /// (`\9` -> `\9 `).
    /// Strings produced by computation/interpolation are "ready text" and are output as is.
    pub(crate) escape: bool,
}

/// Number + unit algebra: both `numer` / `denom` keep the original unit spelling in
/// order of appearance.
///
/// `2px * 3px` -> `6px*px`; `10px / 2px` -> `5`; compound units cannot go into CSS
/// directly (dart-sass errors), but `inspect()` must be able to print them.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Number {
    pub(crate) value: f64,
    pub(crate) numer: Vec<String>,
    pub(crate) denom: Vec<String>,
    /// A "slash number" such as `1/2`: the value is computed, but the CSS output keeps the
    /// original spelling on both sides.
    /// dart-sass expresses the same semantics with `SassNumber.withSlash`.
    pub(crate) slash: Option<Box<(Value, Value)>>,
}

/// A color: keeps both the "original literal spelling" and the RGBA value. Literals are
/// output as is; only computed values are normalized.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Color {
    pub(crate) r: f64,
    pub(crate) g: f64,
    pub(crate) b: f64,
    pub(crate) a: f64,
    pub(crate) literal: Option<String>,
    /// Colors coming from HSL operations carry their original HSL components (degrees /
    /// percentages), and later HSL operations use them directly instead of deriving them
    /// back from the rounded RGB (dart's `_hue/_saturation/_lightness`).
    pub(crate) hsl: Option<(f64, f64, f64)>,
    /// The space the color belongs to (`color.space()`): rgb/hsl/hwb for legacy, the
    /// respective one for modern constructions.
    pub(crate) space: Option<Space>,
    /// Original channels of a non-legacy space (`None` is CSS's `none`/missing).
    /// The `hsl` space uses the `hsl` field above (to avoid rounding loss).
    pub(crate) channels: Option<[Option<f64>; 3]>,
}

pub(crate) type ValResult<T> = Result<T, String>;

fn list_map(list: &List, f: impl Fn(&Value) -> Value) -> List {
    List {
        items: list.items.iter().map(f).collect(),
        sep: list.sep,
        bracketed: list.bracketed,
    }
}

impl Number {
    pub(crate) fn unitless(value: f64) -> Self {
        Self {
            value,
            numer: Vec::new(),
            denom: Vec::new(),
            slash: None,
        }
    }

    /// `unit` is given in its final CSS form (escaping is the caller's job); `%` is passed
    /// through as `"%"`.
    pub(crate) fn with_unit(value: f64, unit: &str) -> Self {
        Self {
            value,
            numer: vec![unit.to_string()],
            denom: Vec::new(),
            slash: None,
        }
    }

    /// The original spelling of a division: the result is computed numerically, but
    /// serialization uses the `inspect()` of both sides.
    pub(crate) fn with_slash(result: Self, left: Value, right: Value) -> Self {
        Self {
            slash: Some(Box::new((left, right))),
            ..result
        }
    }

    /// Drop the slash spelling (dart-sass calls `withoutSlash()` on assignment / when
    /// taking part in an operation).
    pub(crate) fn without_slash(mut self) -> Self {
        self.slash = None;
        self
    }

    pub(crate) fn is_unitless(&self) -> bool {
        self.numer.is_empty() && self.denom.is_empty()
    }

    /// Whether this is a "simple unit": unitless or a single numerator unit. Only such
    /// values can go into CSS directly.
    pub(crate) fn is_simple(&self) -> bool {
        self.denom.is_empty() && self.numer.len() <= 1
    }

    pub(crate) fn unit(&self) -> &str {
        self.numer.first().map(String::as_str).unwrap_or("")
    }

    /// Unit string: `px*px/s*s`. Units are stored in their final CSS form (`px`, `%`,
    /// `px\9 `).
    pub(crate) fn unit_string(&self) -> String {
        let mut out = self.numer.join("*");
        if !self.denom.is_empty() {
            out.push('/');
            out.push_str(&self.denom.join("*"));
        }
        out
    }

    /// CSS text of the number (without unit): 10 decimal places, trailing zeros removed,
    /// large integers fully expanded.
    pub(crate) fn format_value(value: f64) -> String {
        if value.is_nan() {
            return "NaN".to_string();
        }
        if value.is_infinite() {
            return if value > 0.0 { "Infinity" } else { "-Infinity" }.to_string();
        }
        // dart-sass rounds the shortest decimal representation of the double to 10 decimal
        // places (string-level half-up: 1.23456789015 -> 1.2345678902).
        let mut text = format!("{value}");
        if text.contains('e') || text.contains('E') {
            text = expand_exponent(&text);
        }
        let text = round_decimal(&text, 10);
        if text == "-0" { "0".to_string() } else { text }
    }

    /// CSS text with units. Compound units are reported by the caller first.
    pub(crate) fn format(&self) -> String {
        if let Some(boxed) = &self.slash {
            let (left, right) = &**boxed;
            return format!("{}/{}", left.inspect(), right.inspect());
        }
        // Compound units use dart's debug notation: `px*px`, `px*s^-1` (the error message
        // must match).
        let mut out = Self::format_value(self.value);
        let mut parts: Vec<String> = self.numer.clone();
        parts.extend(self.denom.iter().map(|unit| format!("{unit}^-1")));
        out.push_str(&parts.join("*"));
        out
    }

    pub(crate) fn add(&self, rhs: &Self) -> ValResult<Self> {
        self.arith(rhs, 1.0, 1.0)
    }

    pub(crate) fn sub(&self, rhs: &Self) -> ValResult<Self> {
        self.arith(rhs, 1.0, -1.0)
    }

    /// Addition/subtraction: the units must be convertible (same unit or same dimension),
    /// otherwise it is an error.
    fn arith(&self, rhs: &Self, lhs_scale: f64, rhs_scale: f64) -> ValResult<Self> {
        let (lhs_value, rhs_value, numer, denom) = if self.is_unitless() && rhs.is_unitless() {
            (self.value, rhs.value, Vec::new(), Vec::new())
        } else if self.is_unitless() || rhs.is_unitless() {
            // A unitless value takes the other side's unit (dart-sass: 1 + 1px == 2px).
            let (numer, denom) = if self.is_unitless() {
                (rhs.numer.clone(), rhs.denom.clone())
            } else {
                (self.numer.clone(), self.denom.clone())
            };
            (self.value, rhs.value, numer, denom)
        } else if self.numer == rhs.numer && self.denom == rhs.denom {
            (
                self.value,
                rhs.value,
                self.numer.clone(),
                self.denom.clone(),
            )
        } else if let (Some(factor), true) = (
            convert_factor(rhs.unit(), self.unit()),
            self.is_simple() && rhs.is_simple(),
        ) {
            (
                self.value,
                rhs.value * factor,
                self.numer.clone(),
                Vec::new(),
            )
        } else {
            return Err(format!(
                "{} and {} have incompatible units.",
                self.format(),
                rhs.format()
            ));
        };
        Ok(Self {
            value: lhs_value * lhs_scale + rhs_value * rhs_scale,
            numer,
            denom,
            slash: None,
        })
    }

    pub(crate) fn mul(&self, rhs: &Self) -> Self {
        let mut numer = self.numer.clone();
        numer.extend(rhs.numer.iter().cloned());
        let mut denom = self.denom.clone();
        denom.extend(rhs.denom.iter().cloned());
        let (numer, denom) = cancel(numer, denom);
        Self {
            value: self.value * rhs.value,
            numer,
            denom,
            slash: None,
        }
    }

    pub(crate) fn div(&self, rhs: &Self) -> ValResult<Self> {
        let mut numer = self.numer.clone();
        numer.extend(rhs.denom.iter().cloned());
        let mut denom = self.denom.clone();
        denom.extend(rhs.numer.iter().cloned());
        let (numer, denom) = cancel(numer, denom);
        Ok(Self {
            value: self.value / rhs.value,
            numer,
            denom,
            slash: None,
        })
    }

    pub(crate) fn rem(&self, rhs: &Self) -> ValResult<Self> {
        let rhs_value = match convert_factor(rhs.unit(), self.unit()) {
            Some(factor) if self.is_simple() && rhs.is_simple() => rhs.value * factor,
            _ if self.numer == rhs.numer && self.denom == rhs.denom => rhs.value,
            _ => {
                return Err(format!(
                    "{} and {} have incompatible units.",
                    self.format(),
                    rhs.format()
                ));
            }
        };
        // dart-sass `moduloLikeSass`: the sign follows the divisor (Dart's `%` is a
        // non-negative remainder).
        let value = if rhs_value > 0.0 {
            self.value % rhs_value
        } else if rhs_value == 0.0 {
            f64::NAN
        } else {
            let rem = self.value % rhs_value;
            if rem == 0.0 { 0.0 } else { rem + rhs_value }
        };
        Ok(Self {
            value,
            numer: self.numer.clone(),
            denom: self.denom.clone(),
            slash: None,
        })
    }

    /// Convert `rhs` into a number in this unit (dart-sass `_coerceUnits`,
    /// coerceUnitless=true).
    pub(crate) fn coerce(&self, rhs: &Self) -> ValResult<f64> {
        if self.numer == rhs.numer && self.denom == rhs.denom {
            return Ok(rhs.value);
        }
        if self.is_unitless() || rhs.is_unitless() {
            return Ok(rhs.value);
        }
        if self.is_simple() && rhs.is_simple() {
            if let Some(factor) = convert_factor(rhs.unit(), self.unit()) {
                return Ok(rhs.value * factor);
            }
        }
        Err(format!(
            "{} and {} have incompatible units.",
            self.format(),
            rhs.format()
        ))
    }

    /// Comparison: the units must be compatible; a unitless and a united value compare by
    /// their numeric value.
    pub(crate) fn compare(&self, rhs: &Self) -> ValResult<std::cmp::Ordering> {
        let rhs_value = if self.numer == rhs.numer && self.denom == rhs.denom {
            rhs.value
        } else if let (Some(factor), true) = (
            convert_factor(rhs.unit(), self.unit()),
            self.is_simple() && rhs.is_simple(),
        ) {
            rhs.value * factor
        } else if self.is_unitless() || rhs.is_unitless() {
            rhs.value
        } else {
            return Err(format!(
                "{} and {} have incompatible units.",
                self.format(),
                rhs.format()
            ));
        };
        self.value
            .partial_cmp(&rhs_value)
            .ok_or_else(|| "Numbers cannot be compared.".to_string())
    }
}

/// Round a decimal string to `places` decimal places (half-up), then strip trailing
/// zeros.
fn round_decimal(text: &str, places: usize) -> String {
    let (sign, body) = match text.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", text),
    };
    let (int_part, frac_part) = match body.split_once('.') {
        Some((i, f)) => (i.to_string(), f.to_string()),
        None => (body.to_string(), String::new()),
    };
    if frac_part.len() <= places {
        let frac = frac_part.trim_end_matches('0');
        return if frac.is_empty() {
            format!("{sign}{int_part}")
        } else {
            format!("{sign}{int_part}.{frac}")
        };
    }
    let mut digits: Vec<u8> = format!("{int_part}{frac_part}").into_bytes();
    let keep = int_part.len() + places;
    let round_up = digits[keep] >= b'5';
    digits.truncate(keep);
    let mut point = int_part.len();
    if round_up {
        let mut i = digits.len();
        loop {
            if i == 0 {
                digits.insert(0, b'1');
                point += 1;
                break;
            }
            i -= 1;
            if digits[i] == b'9' {
                digits[i] = b'0';
            } else {
                digits[i] += 1;
                break;
            }
        }
    }
    let int_part: String = String::from_utf8_lossy(&digits[..point]).into_owned();
    let frac_part: String = String::from_utf8_lossy(&digits[point..]).into_owned();
    let frac = frac_part.trim_end_matches('0');
    let int_part = if int_part.is_empty() {
        "0".to_string()
    } else {
        int_part
    };
    if frac.is_empty() {
        format!("{sign}{int_part}")
    } else {
        format!("{sign}{int_part}.{frac}")
    }
}

/// Expand `1e21` / `1e-7` into a fixed-point decimal.
fn expand_exponent(text: &str) -> String {
    let negative = text.starts_with('-');
    let body = text.trim_start_matches('-');
    let Some((mantissa, exp)) = body.split_once(['e', 'E']) else {
        return text.to_string();
    };
    let exp: i32 = exp.parse().unwrap_or(0);
    let (int_part, frac_part) = match mantissa.split_once('.') {
        Some((i, f)) => (i.to_string(), f.to_string()),
        None => (mantissa.to_string(), String::new()),
    };
    let digits: String = format!("{int_part}{frac_part}");
    let point = int_part.len() as i32 + exp;
    let mut out = String::new();
    if negative {
        out.push('-');
    }
    if point <= 0 {
        out.push_str("0.");
        for _ in 0..(-point) {
            out.push('0');
        }
        out.push_str(&digits);
    } else if (point as usize) >= digits.len() {
        out.push_str(&digits);
        for _ in 0..(point as usize - digits.len()) {
            out.push('0');
        }
    } else {
        out.push_str(&digits[..point as usize]);
        out.push('.');
        out.push_str(&digits[point as usize..]);
    }
    out
}

fn cancel(mut numer: Vec<String>, mut denom: Vec<String>) -> (Vec<String>, Vec<String>) {
    let mut i = 0;
    while i < numer.len() {
        if let Some(j) = denom.iter().position(|d| *d == numer[i]) {
            numer.remove(i);
            denom.remove(j);
        } else {
            i += 1;
        }
    }
    (numer, denom)
}

/// Conversion factor between units of the same dimension: the value in `rhs` unit times
/// factor = the value expressed in `lhs` unit.
fn convert_factor(rhs: &str, lhs: &str) -> Option<f64> {
    if rhs == lhs {
        return Some(1.0);
    }
    let (rg, rf) = unit_kind(rhs)?;
    let (lg, lf) = unit_kind(lhs)?;
    (rg == lg).then_some(rf / lf)
}

/// Conversion factor between units of the same dimension (also reused for the bound
/// conversion of `@for`).
pub(crate) fn convert_factor_public(rhs: &str, lhs: &str) -> Option<f64> {
    convert_factor(rhs, lhs)
}

/// Unit dimensions and their conversion table (in the order used by dart's error
/// messages): `("length", ["in", ...])`.
pub(crate) fn unit_kind_names(unit: &str) -> Option<(&'static str, &'static [&'static str])> {
    let (kind, _) = unit_kind(unit)?;
    let names: &'static [&'static str] = match kind {
        "length" => &["in", "cm", "pc", "mm", "q", "pt", "px"],
        "angle" => &["deg", "grad", "rad", "turn"],
        "time" => &["s", "ms"],
        "frequency" => &["Hz", "kHz"],
        "resolution" => &["dpi", "dpcm", "dppx"],
        _ => return None,
    };
    Some((kind, names))
}

fn unit_kind(unit: &str) -> Option<(&'static str, f64)> {
    Some(match unit.to_ascii_lowercase().as_str() {
        "px" => ("length", 1.0),
        "cm" => ("length", 96.0 / 2.54),
        "mm" => ("length", 96.0 / 25.4),
        "q" => ("length", 96.0 / 101.6),
        "in" => ("length", 96.0),
        "pt" => ("length", 96.0 / 72.0),
        "pc" => ("length", 16.0),
        "deg" => ("angle", 1.0),
        "grad" => ("angle", 0.9),
        "rad" => ("angle", 180.0 / std::f64::consts::PI),
        "turn" => ("angle", 360.0),
        "s" => ("time", 1.0),
        "ms" => ("time", 0.001),
        "hz" => ("frequency", 1.0),
        "khz" => ("frequency", 1000.0),
        "dpi" => ("resolution", 1.0),
        "dpcm" => ("resolution", 2.54),
        "dppx" => ("resolution", 96.0),
        _ => return None,
    })
}

/// dart's `fuzzyLessThan`: `0.4999999999999` counts as equal to `0.5` (it is not
/// rounded down to "less").
pub(crate) fn fuzzy_round(value: f64) -> f64 {
    const EPSILON: f64 = 1e-11;
    let fraction = value % 1.0;
    if value > 0.0 {
        if fraction < 0.5 - EPSILON {
            value.floor()
        } else {
            value.ceil()
        }
    } else if fraction <= -0.5 + EPSILON {
        value.floor()
    } else {
        value.ceil()
    }
}

impl Color {
    /// Channels are stored raw as f64 (dart 1.93's `SassColor.rgbInternal`): chained
    /// operations keep full precision, and rounding only happens when `to_css()`
    /// serializes.
    pub(crate) fn new(r: f64, g: f64, b: f64, a: f64) -> Self {
        Self {
            r,
            g,
            b,
            a,
            literal: None,
            hsl: None,
            space: None,
            channels: None,
        }
    }

    /// Parse `#rgb` / `#rrggbb` / `#rrggbbaa`; the original literal spelling is kept.
    pub(crate) fn from_hex(raw: &str) -> Option<Self> {
        let hex = raw.strip_prefix('#')?;
        let digits: Vec<u8> = hex
            .chars()
            .map(|c| c.to_digit(16).map(|d| d as u8))
            .collect::<Option<Vec<_>>>()?;
        let nibble = |i: usize| digits[i] as f64;
        // The 6/8 digit forms are two-hex-digit bytes, not nibbles.
        let byte = |i: usize| digits[2 * i] as f64 * 16.0 + digits[2 * i + 1] as f64;
        let (r, g, b, a) = match digits.len() {
            3 => (nibble(0) * 17.0, nibble(1) * 17.0, nibble(2) * 17.0, 1.0),
            4 => (
                nibble(0) * 17.0,
                nibble(1) * 17.0,
                nibble(2) * 17.0,
                nibble(3) / 15.0,
            ),
            6 => (byte(0), byte(1), byte(2), 1.0),
            8 => (byte(0), byte(1), byte(2), byte(3) / 255.0),
            _ => return None,
        };
        Some(Self {
            r,
            g,
            b,
            a,
            literal: Some(raw.to_string()),
            hsl: None,
            space: None,
            channels: None,
        })
    }

    /// Color name (case-insensitive). The literal spelling is kept by the caller.
    pub(crate) fn from_name(name: &str) -> Option<Self> {
        let lower = name.to_ascii_lowercase();
        if lower == "transparent" {
            return Some(Self::new(0.0, 0.0, 0.0, 0.0));
        }
        NAMED_COLORS
            .iter()
            .find(|(n, _)| *n == lower)
            .map(|(_, rgb)| {
                Self::new(
                    ((rgb >> 16) & 0xff) as f64,
                    ((rgb >> 8) & 0xff) as f64,
                    (rgb & 0xff) as f64,
                    1.0,
                )
            })
    }

    /// Serialization rounds to the nearest integer (dart's `fuzzyRound`).
    fn rounded(&self) -> (u8, u8, u8) {
        let ch = |v: f64| fuzzy_round(v).clamp(0.0, 255.0) as u8;
        (ch(self.r), ch(self.g), ch(self.b))
    }

    fn name(&self) -> Option<&'static str> {
        let (r, g, b) = self.rounded();
        let packed = ((r as u32) << 16) | ((g as u32) << 8) | b as u32;
        NAMED_COLORS
            .iter()
            .find(|(_, hex)| *hex == packed)
            .map(|(name, _)| *name)
    }

    pub(crate) fn to_css(&self) -> String {
        if let Some(literal) = &self.literal {
            return literal.clone();
        }
        if let Some(space) = self.space.filter(|s| !s.is_legacy()) {
            return self.to_css_space(space);
        }
        let (r, g, b) = self.rounded();
        if self.a >= 1.0 {
            if let Some(name) = self.name() {
                return name.to_string();
            }
            return format!("#{r:02x}{g:02x}{b:02x}");
        }
        format!("rgba({r}, {g}, {b}, {})", Number::format_value(self.a))
    }

    /// `color.space()`: an explicit space wins, then the legacy hsl/hwb markers.
    pub(crate) fn space(&self) -> Space {
        self.space.unwrap_or(Space::Rgb)
    }

    /// Channels of the current space (display units; `None` means missing).
    pub(crate) fn origin_channels(&self) -> [Option<f64>; 3] {
        let space = self.space();
        if let Some(channels) = self.channels {
            return channels;
        }
        if space == Space::Hsl {
            if let Some((h, s, l)) = self.hsl {
                return [Some(h), Some(s), Some(l)];
            }
        }
        if space == Space::Rgb {
            return [Some(self.r), Some(self.g), Some(self.b)];
        }
        let ch = Self::math_to_display(space, color_space::from_srgb(space, self.unit_rgb()));
        [Some(ch[0]), Some(ch[1]), Some(ch[2])]
    }

    fn unit_rgb(&self) -> [f64; 3] {
        [self.r / 255.0, self.g / 255.0, self.b / 255.0]
    }

    /// Display units -> math units of the space (the lightness of `oklab`/`oklch` is 0..1
    /// mathematically).
    pub(crate) fn math_to_display(space: Space, ch: [f64; 3]) -> [f64; 3] {
        match space {
            Space::Rgb => ch.map(|c| c * 255.0),
            Space::Oklab | Space::Oklch => [ch[0] * 100.0, ch[1], ch[2]],
            _ => ch,
        }
    }

    fn to_math(space: Space, ch: [f64; 3]) -> [f64; 3] {
        match space {
            Space::Rgb => ch.map(|c| c / 255.0),
            Space::Oklab | Space::Oklch => [ch[0] / 100.0, ch[1], ch[2]],
            _ => ch,
        }
    }

    fn from_math(space: Space, ch: [f64; 3]) -> [f64; 3] {
        match space {
            Space::Rgb => ch.map(|c| c * 255.0),
            Space::Oklab | Space::Oklch => [ch[0] * 100.0, ch[1], ch[2]],
            _ => ch,
        }
    }

    /// Modern construction: channels are stored in display units, and the RGB goes through
    /// the rounding and literal rules of `Color::new`.
    pub(crate) fn from_space(space: Space, channels: [Option<f64>; 3], alpha: f64) -> Self {
        let filled = [
            channels[0].unwrap_or(0.0),
            channels[1].unwrap_or(0.0),
            channels[2].unwrap_or(0.0),
        ];
        let rgb = color_space::to_srgb(space, Self::to_math(space, filled));
        let mut color = Self::new(rgb[0] * 255.0, rgb[1] * 255.0, rgb[2] * 255.0, alpha);
        color.space = Some(space);
        color.channels = Some(channels);
        color
    }

    /// Convert to another space (`color.to-space()`).
    pub(crate) fn to_space(&self, space: Space) -> Self {
        let from = self.space();
        if from == space {
            return self.clone();
        }
        let channels = self.origin_channels();
        let filled = [
            channels[0].unwrap_or(0.0),
            channels[1].unwrap_or(0.0),
            channels[2].unwrap_or(0.0),
        ];
        let converted = color_space::convert(from, space, Self::to_math(from, filled));
        let display = Self::from_math(space, converted);
        let rgb = color_space::to_srgb(space, Self::to_math(space, display));
        // The color is stored in the **target space**: a legacy target records `space` too,
        // otherwise `channel($c, "hue", hsl)` would fall back to RGB channels; the
        // serialization side still follows the legacy rules (hex).
        let mut color = Self::new(rgb[0] * 255.0, rgb[1] * 255.0, rgb[2] * 255.0, self.a);
        color.space = Some(space);
        color.channels = Some([Some(display[0]), Some(display[1]), Some(display[2])]);
        if space == Space::Hsl {
            color.hsl = Some((display[0], display[1], display[2]));
        }
        color
    }

    /// A non-legacy space is serialized in its own space (`lab()` / `oklch()` /
    /// `color()`).
    fn to_css_space(&self, space: Space) -> String {
        let channels = self.origin_channels();
        let text = (0..3)
            .map(|i| match channels[i] {
                None => "none".to_string(),
                Some(value) => {
                    let number = Number::format_value(value);
                    match space.channel_unit(i) {
                        Some(unit) => format!("{number}{unit}"),
                        None => number,
                    }
                }
            })
            .collect::<Vec<_>>()
            .join(" ");
        let alpha = if self.a >= 1.0 {
            String::new()
        } else {
            format!(" / {}", Number::format_value(self.a))
        };
        if matches!(space, Space::Lab | Space::Lch | Space::Oklab | Space::Oklch) {
            format!("{}({text}{alpha})", space.name())
        } else {
            format!("color({} {text}{alpha})", space.name())
        }
    }
}

impl SassStr {
    pub(crate) fn quoted(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            quoted: true,
            escape: false,
        }
    }

    pub(crate) fn unquoted(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            quoted: false,
            escape: false,
        }
    }

    /// An identifier read from a CSS source file (must be serialized per the CSS escaping
    /// rules).
    pub(crate) fn ident(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            quoted: false,
            escape: true,
        }
    }

    pub(crate) fn to_css(&self) -> String {
        if self.quoted {
            quote_string(&self.text)
        } else if self.escape {
            escape_unquoted(&self.text)
        } else {
            escape_private_use(&self.text)
        }
    }
}

/// Use single quotes only when the string contains `"` and no `'` (dart's
/// `_visitQuotedString`: scan with double quotes first, and when a `"` is found while a `'`
/// has already appeared, start over and force double quotes).
pub(crate) fn quote_string(text: &str) -> String {
    let quote = if text.contains('"') && !text.contains('\'') {
        '\''
    } else {
        '"'
    };
    let mut out = String::with_capacity(text.len() + 2);
    out.push(quote);
    let chars: Vec<char> = text.chars().collect();
    for (i, &ch) in chars.iter().enumerate() {
        match ch {
            '\\' => out.push_str("\\\\"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            // NUL is not legal in a CSS string: dart writes U+FFFD (in an identifier it is
            // `\\0 ` instead).
            '\0' => out.push('\u{FFFD}'),
            // A tab is written as is (tab is not in dart's control-character escape list).
            '\t' => out.push('\t'),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\{:x}", c as u32));
                // When a hex escape is immediately followed by a hex digit/whitespace, add a
                // space to terminate it.
                if let Some(&next) = chars.get(i + 1) {
                    if next.is_ascii_hexdigit() || next == ' ' || next == '\t' {
                        out.push(' ');
                    }
                }
            }
            c if is_private_use(c) => push_private_use(&mut out, &chars, i),
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// Serialization of unquoted strings/identifiers: illegal characters are escaped; a
/// trailing hex escape gets a terminating space.
pub(crate) fn escape_unquoted(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let chars: Vec<char> = text.chars().collect();
    for (i, &ch) in chars.iter().enumerate() {
        let cp = ch as u32;
        let at_start = i == 0;
        let after_dash = i == 1 && chars[0] == '-';
        if cp < 0x20 || cp == 0x7f || ((at_start || after_dash) && ch.is_ascii_digit()) {
            out.push_str(&format!("\\{cp:x} "));
        } else if is_private_use(ch) {
            push_private_use(&mut out, &chars, i);
        } else if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' || cp >= 0x80 {
            out.push(ch);
        } else if ch == '\\' {
            out.push_str("\\\\");
        } else {
            out.push('\\');
            out.push(ch);
        }
    }
    out
}

/// dart-sass's `isPrivateUse`: BMP private use area U+E000-U+F8FF and planes 15/16.
///
/// Icon fonts use them as glyph code points (`content: "\e644"`), and dart always writes
/// them as hex escapes in expanded mode; without escaping, the output would carry non-ASCII
/// bytes and an extra `@charset` would appear.
fn is_private_use(ch: char) -> bool {
    matches!(ch as u32, 0xE000..=0xF8FF | 0xF0000..=0x10FFFF)
}

/// Minimal escaping for unquoted strings: only the private use area becomes a hex escape,
/// everything else is kept as is (dart's `_visitUnquotedString`).
fn escape_private_use(text: &str) -> String {
    if !text.chars().any(is_private_use) {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    for (i, &ch) in chars.iter().enumerate() {
        if !is_private_use(ch) {
            out.push(ch);
            continue;
        }
        push_private_use(&mut out, &chars, i);
    }
    out
}

/// Write a private use character as a hex escape; add a terminating space when a hex digit
/// or whitespace follows.
fn push_private_use(out: &mut String, chars: &[char], i: usize) {
    out.push_str(&format!("\\{:x}", chars[i] as u32));
    if let Some(&next) = chars.get(i + 1) {
        if next.is_ascii_hexdigit() || next == ' ' || next == '\t' {
            out.push(' ');
        }
    }
}

impl Value {
    pub(crate) fn assert_number(&self, name: &str) -> ValResult<&Number> {
        match self {
            Value::Number(number) => Ok(number),
            other => Err(format!("${name}: {} is not a number.", other.inspect())),
        }
    }

    pub(crate) fn assert_color(&self, name: &str) -> ValResult<&Color> {
        match self {
            Value::Color(color) => Ok(color),
            other => Err(format!("${name}: {} is not a color.", other.inspect())),
        }
    }

    pub(crate) fn assert_string(&self, name: &str) -> ValResult<&SassStr> {
        match self {
            Value::Str(text) => Ok(text),
            other => Err(format!("${name}: {} is not a string.", other.inspect())),
        }
    }

    pub(crate) fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// Output as a CSS value. `null` returns an empty string and is dropped by the
    /// declaration layer; maps / empty lists / compound units are errors.
    pub(crate) fn to_css(&self) -> ValResult<String> {
        Ok(match self {
            // A number with slash spelling is output as written (`10/4px`), even if a
            // compound unit cannot be a value -- dart only validates units after real
            // evaluation (`$x: 10/4px; a: $x` is what errors).
            Value::Number(n) if n.slash.is_some() => n.format(),
            Value::Number(n) => {
                if !n.is_simple() {
                    return Err(format!("{} isn't a valid CSS value.", self.inspect()));
                }
                n.format()
            }
            Value::Color(c) => c.to_css(),
            Value::Str(s) => s.to_css(),
            Value::Bool(b) => b.to_string(),
            Value::Null => String::new(),
            Value::List(list) => {
                if list.items.is_empty() {
                    if list.bracketed {
                        return Ok("[]".to_string());
                    }
                    return Err("() isn't a valid CSS value.".to_string());
                }
                let sep = match list.sep {
                    Sep::Comma => ", ",
                    Sep::Space => " ",
                    Sep::Slash => " / ",
                    Sep::SlashTight => "/",
                };
                let mut parts = Vec::new();
                for item in &list.items {
                    // null and the empty list are transparent elements in list serialization
                    // (`1 null` -> `1`).
                    if item.is_null() {
                        continue;
                    }
                    if matches!(item, Value::List(l) if l.items.is_empty() && !l.bracketed) {
                        continue;
                    }
                    parts.push(item.to_css()?);
                }
                let body = parts.join(sep);
                if list.bracketed {
                    format!("[{body}]")
                } else {
                    body
                }
            }
            Value::Map(_) => return Err(format!("{} isn't a valid CSS value.", self.inspect())),
            Value::ArgList(args) => {
                if args.positional.is_empty() {
                    return Err("() isn't a valid CSS value.".to_string());
                }
                // Used as a value, `$rest` is just a comma list (observed with dart:
                // `y: 2, 3`).
                Value::List(List {
                    items: args.positional.clone(),
                    sep: Sep::Comma,
                    bracketed: false,
                })
                .to_css()?
            }
            Value::Function(f) => format!("{}({})", f.name, f.args.to_css_args()?),
            Value::FuncRef(reference) => {
                return Err(format!(
                    "get-function({}) isn't a valid CSS value.",
                    quote_string(&reference.name)
                ));
            }
            Value::Raw(text) => text.clone(),
        })
    }

    /// Function argument serialization: null keeps its placeholder (`foo(null, 1)` ->
    /// `foo(, 1)`), unlike a value list.
    pub(crate) fn to_css_args(&self) -> ValResult<String> {
        match self {
            Value::List(list) if list.sep == Sep::Comma && !list.bracketed => {
                let mut parts = Vec::with_capacity(list.items.len());
                for item in &list.items {
                    parts.push(if item.is_null() {
                        String::new()
                    } else {
                        item.to_css()?
                    });
                }
                Ok(parts.join(", "))
            }
            other => other.to_css(),
        }
    }

    /// Text of the `#{}` interpolation: strings use their raw text (quotes removed), the
    /// rest is like CSS.
    /// Interpolation text: null is an empty string; a string yields its bare text; the rest
    /// is serialized as CSS.
    /// Values that cannot go into CSS, such as maps / empty lists, error right here (dart
    /// errors too).
    pub(crate) fn interpolation(&self) -> ValResult<String> {
        match self {
            Value::Null => Ok(String::new()),
            Value::Str(s) => Ok(s.text.clone()),
            // List members in an interpolation are unquoted as well.
            Value::List(list) if !list.items.is_empty() => {
                let sep = match list.sep {
                    Sep::Comma => ", ",
                    Sep::Space => " ",
                    Sep::Slash => " / ",
                    Sep::SlashTight => "/",
                };
                let parts = list
                    .items
                    .iter()
                    .filter(|item| {
                        !item.is_null()
                            && !matches!(item, Value::List(l) if l.items.is_empty() && !l.bracketed)
                    })
                    .map(Value::interpolation)
                    .collect::<ValResult<Vec<_>>>()?;
                let body = parts.join(sep);
                Ok(if list.bracketed {
                    format!("[{body}]")
                } else {
                    body
                })
            }
            other => other.to_css(),
        }
    }

    /// On variable assignment dart-sass calls `withoutSlash()` (recursively through lists).
    pub(crate) fn without_slash(&self) -> Value {
        match self {
            Value::Number(n) => Value::Number(n.clone().without_slash()),
            Value::List(l) => Value::List(list_map(l, |v| v.without_slash())),
            other => other.clone(),
        }
    }

    /// `inspect()`: the debug representation, where compound units are visible (`6px*px`).
    pub(crate) fn inspect(&self) -> String {
        match self {
            Value::Number(n) => n.format(),
            Value::Color(c) => c.to_css(),
            Value::Str(s) => s.to_css(),
            Value::Bool(b) => b.to_string(),
            Value::Null => "null".to_string(),
            Value::List(list) => {
                let sep = match list.sep {
                    Sep::Comma => ", ",
                    Sep::Space => " ",
                    Sep::Slash => " / ",
                    Sep::SlashTight => "/",
                };
                // dart's inspect: an empty list is `()`, a single-element comma list carries a
                // trailing comma `(1,)`.
                if list.items.is_empty() {
                    return if list.bracketed {
                        "[]".to_string()
                    } else {
                        "()".to_string()
                    };
                }
                if list.sep == Sep::Comma && !list.bracketed && list.items.len() == 1 {
                    return format!("({},)", list.items[0].inspect());
                }
                let body = list
                    .items
                    .iter()
                    .map(Value::inspect)
                    .collect::<Vec<_>>()
                    .join(sep);
                if list.bracketed {
                    format!("[{body}]")
                } else {
                    body
                }
            }
            Value::Map(map) => {
                let body = map
                    .entries
                    .iter()
                    .map(|(k, v)| format!("{}: {}", k.inspect(), v.inspect()))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("({body})")
            }
            Value::ArgList(args) => {
                // dart's `inspect()` only looks at positional arguments (keyword arguments are
                // invisible): `()` is always `()`.
                if args.positional.is_empty() {
                    "()".to_string()
                } else if args.positional.len() == 1 {
                    // dart: the inspect of a single-element comma list has a trailing comma
                    // (`(2px,)`).
                    format!("({},)", args.positional[0].inspect())
                } else {
                    args.positional
                        .iter()
                        .map(Value::inspect)
                        .collect::<Vec<_>>()
                        .join(", ")
                }
            }
            Value::Function(f) => format!("{}({})", f.name, f.args.inspect()),
            Value::FuncRef(reference) => {
                format!("get-function({})", quote_string(&reference.name))
            }
            Value::Raw(text) => text.clone(),
        }
    }
}

/// Binary arithmetic (the semantics of dart-sass's Value methods).
pub(crate) const EPSILON: f64 = 1e-11;

fn fuzzy_eq(a: f64, b: f64) -> bool {
    (a - b).abs() < EPSILON
}

fn undefined(op: &SassBinaryOperatorKind, lhs: &Value, rhs: &Value) -> String {
    use SassBinaryOperatorKind::*;
    let symbol = match op {
        Plus => "+",
        Minus => "-",
        Multiply => "*",
        Division => "/",
        Modulo => "%",
        EqualsEquals => "==",
        ExclamationEquals => "!=",
        GreaterThan => ">",
        GreaterThanOrEqual => ">=",
        LessThan => "<",
        LessThanOrEqual => "<=",
        And => "and",
        Or => "or",
    };
    format!(
        "Undefined operation \"{} {} {}\".",
        lhs.inspect(),
        symbol,
        rhs.inspect()
    )
}

/// String concatenation for non-numeric operations (dart-sass `Value.plus` /
/// `SassString.plus`).
fn concat(lhs: &Value, rhs: &Value, op: &SassBinaryOperatorKind) -> Value {
    use SassBinaryOperatorKind::*;
    match (op, lhs) {
        (Plus, Value::Str(a)) => Value::Str(SassStr {
            text: match rhs {
                Value::Str(b) => format!("{}{}", a.text, b.text),
                // When the right operand is a string, use its quoted form (`1 + "x"` ->
                // `"1x"`).
                other => format!(
                    "{}{}",
                    a.text,
                    other.to_css().unwrap_or_else(|_| other.inspect())
                ),
            },
            quoted: a.quoted,
            escape: false,
        }),
        (Plus, other) if matches!(rhs, Value::Str(_)) => {
            let Value::Str(b) = rhs else {
                unreachable!("guaranteed by the guard")
            };
            Value::Str(SassStr {
                text: format!(
                    "{}{}",
                    other.to_css().unwrap_or_else(|_| other.inspect()),
                    b.text
                ),
                quoted: b.quoted,
                escape: false,
            })
        }
        _ => {
            let sep = match op {
                Minus => "-",
                Division => "/",
                _ => "",
            };
            Value::Str(SassStr::unquoted(format!(
                "{}{}{}",
                lhs.inspect(),
                sep,
                rhs.inspect()
            )))
        }
    }
}

pub(crate) fn binary(op: &SassBinaryOperatorKind, lhs: &Value, rhs: &Value) -> ValResult<Value> {
    use SassBinaryOperatorKind::*;
    Ok(match op {
        Plus => match (lhs, rhs) {
            (Value::Number(a), Value::Number(b)) => Value::Number(a.add(b)?),
            (Value::Number(_), Value::Color(_))
            | (Value::Color(_), Value::Number(_))
            | (Value::Color(_), Value::Color(_)) => return Err(undefined(op, lhs, rhs)),
            _ => concat(lhs, rhs, op),
        },
        Minus => match (lhs, rhs) {
            (Value::Number(a), Value::Number(b)) => Value::Number(a.sub(b)?),
            (Value::Number(_), Value::Color(_))
            | (Value::Color(_), Value::Number(_))
            | (Value::Color(_), Value::Color(_)) => return Err(undefined(op, lhs, rhs)),
            _ => concat(lhs, rhs, op),
        },
        Multiply => match (lhs, rhs) {
            (Value::Number(a), Value::Number(b)) => Value::Number(a.mul(b)),
            _ => return Err(undefined(op, lhs, rhs)),
        },
        Modulo => match (lhs, rhs) {
            (Value::Number(a), Value::Number(b)) => Value::Number(a.rem(b)?),
            _ => return Err(undefined(op, lhs, rhs)),
        },
        Division => return binary_div(lhs, rhs),
        EqualsEquals => Value::Bool(values_equal(lhs, rhs)),
        ExclamationEquals => Value::Bool(!values_equal(lhs, rhs)),
        GreaterThan | GreaterThanOrEqual | LessThan | LessThanOrEqual => match (lhs, rhs) {
            (Value::Number(a), Value::Number(b)) => {
                let right = a.coerce(b)?;
                let (left, right) = (a.value, right);
                let eq = fuzzy_eq(left, right);
                Value::Bool(match op {
                    GreaterThan => left > right && !eq,
                    GreaterThanOrEqual => left > right || eq,
                    LessThan => left < right && !eq,
                    _ => left < right || eq,
                })
            }
            _ => return Err(undefined(op, lhs, rhs)),
        },
        And => {
            if lhs.truthy() {
                rhs.clone()
            } else {
                lhs.clone()
            }
        }
        Or => {
            if lhs.truthy() {
                lhs.clone()
            } else {
                rhs.clone()
            }
        }
    })
}

/// Division: numbers divide; division involving a color is an undefined operation; the
/// rest concatenates as strings (dart semantics).
pub(crate) fn binary_div(lhs: &Value, rhs: &Value) -> ValResult<Value> {
    match (lhs, rhs) {
        (Value::Number(a), Value::Number(b)) => Ok(Value::Number(a.div(b)?)),
        (Value::Color(_), Value::Number(_)) | (Value::Color(_), Value::Color(_)) => Err(format!(
            "Undefined operation \"{} / {}\".",
            lhs.inspect(),
            rhs.inspect()
        )),
        _ => Ok(Value::Str(SassStr::unquoted(format!(
            "{}/{}",
            lhs.inspect(),
            rhs.inspect()
        )))),
    }
}

/// Name comparison after normalization: `$a_b` ≡ `$a-b` (dart's identifier rules).
pub(crate) fn names_eq(a: &str, b: &str) -> bool {
    a == b || (a.contains('_') || b.contains('_')) && a.replace('_', "-") == b.replace('_', "-")
}

/// List semantics: a single value is a 1-element list, `null` is a 1-element list too, and
/// a map is a list of `(k v)` pairs.
pub(crate) fn list_items(value: &Value) -> Option<Vec<Value>> {
    match value {
        Value::List(list) => Some(list.items.clone()),
        Value::ArgList(args) => Some(args.positional.clone()),
        Value::Map(map) => Some(
            map.entries
                .iter()
                .map(|(k, v)| {
                    Value::List(List {
                        items: vec![k.clone(), v.clone()],
                        sep: Sep::Space,
                        bracketed: false,
                    })
                })
                .collect(),
        ),
        other => Some(vec![other.clone()]),
    }
}

/// Map semantics: `()` (the empty list) is an empty map too (so does dart's `assertMap`).
pub(crate) fn as_map(value: &Value, name: &str) -> ValResult<Vec<(Value, Value)>> {
    match value {
        Value::Map(map) => Ok(map.entries.clone()),
        Value::List(list) if list.items.is_empty() && !list.bracketed => Ok(Vec::new()),
        other => Err(format!("${name}: {} is not a map.", other.inspect())),
    }
}

impl Number {
    /// `assertInt`: `1.5 is not an int.`
    pub(crate) fn assert_int(&self, name: &str) -> ValResult<i64> {
        if (self.value - self.value.round()).abs() > EPSILON {
            return Err(format!("${name}: {} is not an int.", self.format()));
        }
        Ok(self.value.round() as i64)
    }

    /// dart `valueInRange`: out of range reports `Expected 120 to be within 0 and 100.`
    pub(crate) fn value_in_range(&self, min: f64, max: f64, name: &str) -> ValResult<f64> {
        if self.value < min - EPSILON || self.value > max + EPSILON {
            return Err(format!(
                "${name}: Expected {} to be within {} and {}.",
                self.format(),
                Self::format_value(min),
                Self::format_value(max)
            ));
        }
        Ok(self.value)
    }

    /// Convert an angle argument to degrees (`1turn` -> 360).
    pub(crate) fn angle_degrees(&self, name: &str) -> ValResult<f64> {
        match self.unit() {
            "" | "deg" => Ok(self.value),
            "grad" => Ok(self.value * 0.9),
            "rad" => Ok(self.value * 180.0 / std::f64::consts::PI),
            "turn" => Ok(self.value * 360.0),
            _ => Err(format!(
                "${name}: Expected {} to have an angle unit (deg, grad, rad, turn).",
                self.format()
            )),
        }
    }
}

impl Color {
    /// Unrounded RGB channels (used for computation).
    pub(crate) fn channels_f64(&self) -> (f64, f64, f64) {
        (self.r, self.g, self.b)
    }

    /// Rounded RGB channels (the displayed form of `red()` / `ie-hex-str()` and `==`).
    pub(crate) fn channels(&self) -> (u8, u8, u8) {
        self.rounded()
    }
}

pub(crate) fn values_equal(lhs: &Value, rhs: &Value) -> bool {
    match (lhs, rhs) {
        (Value::Number(a), Value::Number(b)) => number_eq(a, b),
        (Value::Color(a), Value::Color(b)) => {
            let (ar, ag, ab) = a.channels_f64();
            let (br, bg, bb) = b.channels_f64();
            fuzzy_eq(ar, br) && fuzzy_eq(ag, bg) && fuzzy_eq(ab, bb) && fuzzy_eq(a.a, b.a)
        }
        (Value::Str(a), Value::Str(b)) => a.text == b.text,
        (Value::Bool(a), Value::Bool(b)) => a == b,
        (Value::Function(a), Value::Function(b)) => {
            a.name.eq_ignore_ascii_case(&b.name) && values_equal(&a.args, &b.args)
        }
        (Value::Raw(a), Value::Raw(b)) => a == b,
        (Value::FuncRef(a), Value::FuncRef(b)) => a == b,
        (Value::Null, Value::Null) => true,
        (Value::List(a), Value::List(b)) => {
            a.sep == b.sep
                && a.bracketed == b.bracketed
                && a.items.len() == b.items.len()
                && a.items
                    .iter()
                    .zip(&b.items)
                    .all(|(x, y)| values_equal(x, y))
        }
        (Value::Map(a), Value::Map(b)) => {
            a.entries.len() == b.entries.len()
                && a.entries.iter().all(|(k, v)| {
                    b.entries
                        .iter()
                        .any(|(k2, v2)| values_equal(k, k2) && values_equal(v, v2))
                })
        }
        _ => false,
    }
}

/// Normalized form of a convertible unit: (dimension name, multiplier).
fn canonical_unit(unit: &str) -> (String, f64) {
    match unit_kind(unit) {
        Some((kind, factor)) => (kind.to_string(), factor),
        None => (unit.to_string(), 1.0),
    }
}

fn canonical_parts(numer: &[String], denom: &[String]) -> (Vec<String>, f64) {
    let mut names = Vec::with_capacity(numer.len() + denom.len());
    let mut factor = 1.0;
    for unit in numer {
        let (name, f) = canonical_unit(unit);
        names.push(name);
        factor *= f;
    }
    for unit in denom {
        let (name, f) = canonical_unit(unit);
        names.push(name);
        factor /= f;
    }
    names.sort();
    (names, factor)
}

fn number_eq(a: &Number, b: &Number) -> bool {
    if a.numer.len() != b.numer.len() || a.denom.len() != b.denom.len() {
        return false;
    }
    if a.is_unitless() && b.is_unitless() {
        return fuzzy_eq(a.value, b.value);
    }
    let (a_names, a_factor) = canonical_parts(&a.numer, &a.denom);
    let (b_names, b_factor) = canonical_parts(&b.numer, &b.denom);
    a_names == b_names && fuzzy_eq(a.value * a_factor, b.value * b_factor)
}

impl Value {
    pub(crate) fn truthy(&self) -> bool {
        !matches!(self, Value::Bool(false) | Value::Null)
    }
}

// ---------------------------------------------------------------------------
// AST → Value
// ---------------------------------------------------------------------------

/// A declaration value: split on top-level commas -> split on spaces -> split on slash.
#[cfg(test)]
pub(crate) fn from_components(src: &str, comps: &[ComponentValue]) -> ValResult<Value> {
    let groups = split_on(comps, DelimiterKind::Comma);
    if groups.len() > 1 {
        let items = groups
            .into_iter()
            .map(|group| space_list(src, group))
            .collect::<ValResult<Vec<_>>>()?;
        return Ok(Value::List(List {
            items,
            sep: Sep::Comma,
            bracketed: false,
        }));
    }
    space_list(src, comps)
}

/// Function arguments / bracket contents: a comma is a separator, the rest is like a
/// declaration value.
pub(crate) fn from_sequence(
    src: &str,
    comps: &[ComponentValue],
    bracketed: bool,
) -> ValResult<Value> {
    let groups = split_on(comps, DelimiterKind::Comma);
    let sep = if groups.len() > 1 {
        Sep::Comma
    } else {
        Sep::Space
    };
    let items = groups
        .into_iter()
        .map(|group| space_list(src, group))
        .collect::<ValResult<Vec<_>>>()?;
    if !bracketed && items.len() == 1 {
        return Ok(items.into_iter().next().unwrap());
    }
    Ok(Value::List(List {
        items,
        sep,
        bracketed,
    }))
}

fn split_on<'a, 'b>(
    comps: &'b [ComponentValue<'a>],
    kind: DelimiterKind,
) -> Vec<&'b [ComponentValue<'a>]> {
    let mut groups = Vec::new();
    let mut start = 0;
    for (i, c) in comps.iter().enumerate() {
        if matches!(c, ComponentValue::Delimiter(d) if same_delim(&d.kind, &kind)) {
            groups.push(&comps[start..i]);
            start = i + 1;
        }
    }
    groups.push(&comps[start..]);
    groups
}

fn is_delim(c: &ComponentValue, kind: DelimiterKind) -> bool {
    matches!(c, ComponentValue::Delimiter(d) if same_delim(&d.kind, &kind))
}

fn same_delim(a: &DelimiterKind, b: &DelimiterKind) -> bool {
    matches!(
        (a, b),
        (DelimiterKind::Comma, DelimiterKind::Comma)
            | (DelimiterKind::Solidus, DelimiterKind::Solidus)
            | (DelimiterKind::Semicolon, DelimiterKind::Semicolon)
    )
}

/// Space separation: whitespace only splits when "neither side is a slash" (`1 / 2` is a
/// slash list).
fn space_list(src: &str, comps: &[ComponentValue]) -> ValResult<Value> {
    if comps.is_empty() {
        return Ok(Value::List(List {
            items: Vec::new(),
            sep: Sep::Space,
            bracketed: false,
        }));
    }
    let mut runs: Vec<&[ComponentValue]> = Vec::new();
    let mut start = 0;
    for i in 1..comps.len() {
        let prev = &comps[i - 1];
        let cur = &comps[i];
        let gap = &src[prev.span().end.min(src.len())..cur.span().start.min(src.len())];
        let whitespace = gap.chars().any(char::is_whitespace);
        let slash_adjacent =
            is_delim(prev, DelimiterKind::Solidus) || is_delim(cur, DelimiterKind::Solidus);
        if whitespace && !slash_adjacent {
            runs.push(&comps[start..i]);
            start = i;
        }
    }
    runs.push(&comps[start..]);

    let values = runs
        .into_iter()
        .map(|run| slash_list(src, run))
        .collect::<ValResult<Vec<_>>>()?;
    if values.len() == 1 {
        Ok(values.into_iter().next().unwrap())
    } else {
        Ok(Value::List(List {
            items: values,
            sep: Sep::Space,
            bracketed: false,
        }))
    }
}

fn slash_list(src: &str, comps: &[ComponentValue]) -> ValResult<Value> {
    let groups = split_on(comps, DelimiterKind::Solidus);
    if groups.len() > 1 {
        let items = groups
            .into_iter()
            .map(|group| space_list(src, group))
            .collect::<ValResult<Vec<_>>>()?;
        return Ok(Value::List(List {
            items,
            sep: Sep::SlashTight,
            bracketed: false,
        }));
    }
    if comps.len() == 1 {
        return from_component(src, &comps[0]);
    }
    // Adjacent components without whitespace (such as the token after a function name) fall
    // back to a space list.
    let items = comps
        .iter()
        .map(|c| from_component(src, c))
        .collect::<ValResult<Vec<_>>>()?;
    Ok(Value::List(List {
        items,
        sep: Sep::Space,
        bracketed: false,
    }))
}

pub(crate) fn from_component(src: &str, comp: &ComponentValue) -> ValResult<Value> {
    match comp {
        ComponentValue::Number(n) => Ok(Value::Number(Number::unitless(parse_number(n.raw)))),
        ComponentValue::Dimension(d) => Ok(Value::Number(Number::with_unit(
            parse_number(d.value.raw),
            &escape_unquoted(d.unit.name),
        ))),
        ComponentValue::Percentage(p) => {
            // `10\25`: a percent sign written as an escape, which dart-sass keeps escaped
            // (`10\%`).
            if slice(src, p.span).contains('\\') {
                return Ok(Value::Raw(format!(
                    "{}{}",
                    Number::format_value(parse_number(p.value.raw)),
                    escape_unquoted("%")
                )));
            }
            Ok(Value::Number(Number::with_unit(
                parse_number(p.value.raw),
                "%",
            )))
        }
        ComponentValue::Ratio(r) => Ok(Value::List(List {
            items: vec![
                Value::Number(Number::unitless(parse_number(r.numerator.raw))),
                Value::Number(Number::unitless(parse_number(r.denominator.raw))),
            ],
            sep: Sep::SlashTight,
            bracketed: false,
        })),
        ComponentValue::HexColor(h) => {
            let raw = slice(src, h.span);
            if raw.contains('\\') {
                // `#000\9`: dart-sass splits it into a color + an escaped identifier (a space
                // is added in between).
                if let Some((color, rest)) = split_escaped_hex(&raw) {
                    return Ok(Value::List(List {
                        items: vec![
                            Value::Color(color),
                            Value::Str(SassStr::ident(decode_css_string(rest))),
                        ],
                        sep: Sep::Space,
                        bracketed: false,
                    }));
                }
            }
            match Color::from_hex(&raw) {
                Some(color) => Ok(Value::Color(color)),
                None => Ok(Value::Raw(raw)),
            }
        }
        ComponentValue::InterpolableIdent(ident) => match ident {
            InterpolableIdent::Literal(i) => Ok(ident_value(i.name)),
            _ => Ok(Value::Raw(slice(src, *ident.span()))),
        },
        ComponentValue::InterpolableStr(s) => match s {
            InterpolableStr::Literal(text) => Ok(Value::Str(SassStr::quoted(text.value))),
            _ => Ok(Value::Raw(slice(src, *s.span()))),
        },
        ComponentValue::Function(f) => function_value(src, f),
        ComponentValue::Url(u) => Ok(Value::Str(SassStr::unquoted(rewrite_raw(src, u.span)))),
        ComponentValue::UnicodeRange(_) => Ok(Value::Raw(slice(src, *comp.span()))),
        ComponentValue::Delimiter(d) => Ok(Value::Raw(slice(src, d.span))),
        ComponentValue::BracketBlock(b) => from_sequence(src, &b.value, true),
        ComponentValue::SassParenthesizedExpression(p) => from_component(src, &p.expr),
        ComponentValue::SassUnaryExpression(u) => unary_value(src, u),
        ComponentValue::SassBinaryExpression(b) => binary_value(src, b),
        ComponentValue::SassList(l) => {
            let sep = match &l.comma_spans {
                Some(spans) if !spans.is_empty() => Sep::Comma,
                _ => Sep::Space,
            };
            let items = l
                .elements
                .iter()
                .map(|c| from_component(src, c))
                .collect::<ValResult<Vec<_>>>()?;
            Ok(Value::List(List {
                items,
                sep,
                bracketed: false,
            }))
        }
        ComponentValue::SassMap(m) => {
            if m.items.is_empty() {
                // dart-sass parses `()` as an empty map, and semantically it is treated as an
                // empty list.
                return Ok(Value::List(List {
                    items: Vec::new(),
                    sep: Sep::Space,
                    bracketed: false,
                }));
            }
            let entries = m
                .items
                .iter()
                .map(|item| {
                    Ok((
                        from_component(src, &item.key)?,
                        from_component(src, &item.value)?,
                    ))
                })
                .collect::<ValResult<Vec<_>>>()?;
            Ok(Value::Map(Map { entries }))
        }
        ComponentValue::TokenWithSpan(t) => Ok(Value::Raw(slice(src, t.span))),
        ComponentValue::ImportantAnnotation(_) => Ok(Value::Raw(slice(src, *comp.span()))),
        other => Err(format!(
            "SCSS value not supported yet: {}",
            slice(src, *other.span())
        )),
    }
}

/// Split `#000\9` into `(#000, "\\9")`; only 3/4/6/8 hex digits are accepted after
/// `#`.
fn split_escaped_hex(raw: &str) -> Option<(Color, &str)> {
    let body = raw.strip_prefix('#')?;
    let digits = body.find('\\').unwrap_or(body.len());
    for len in [8, 6, 4, 3] {
        if digits >= len {
            if let Some(color) = Color::from_hex(&format!("#{}", &body[..len])) {
                return Some((color, &body[len..]));
            }
        }
    }
    None
}

pub(crate) fn ident_value(name: &str) -> Value {
    match name {
        "true" => return Value::Bool(true),
        "false" => return Value::Bool(false),
        "null" => return Value::Null,
        _ => {}
    }
    if let Some(color) = Color::from_name(name) {
        return Value::Color(Color {
            literal: Some(name.to_string()),
            ..color
        });
    }
    Value::Str(SassStr::ident(name))
}

fn unary_value(src: &str, u: &SassUnaryExpression) -> ValResult<Value> {
    let inner = from_component(src, &u.expr)?;
    match &u.op.kind {
        SassUnaryOperatorKind::Plus => Ok(inner),
        SassUnaryOperatorKind::Minus => match inner {
            Value::Number(n) => Ok(Value::Number(Number {
                value: -n.value,
                numer: n.numer,
                denom: n.denom,
                slash: None,
            })),
            other => Ok(Value::Str(SassStr::unquoted(format!(
                "-{}",
                other.inspect()
            )))),
        },
        SassUnaryOperatorKind::Not => Ok(Value::Bool(!inner.truthy())),
    }
}

fn binary_value(src: &str, b: &SassBinaryExpression) -> ValResult<Value> {
    let lhs = from_component(src, &b.left)?;
    let rhs = from_component(src, &b.right)?;
    binary(&b.op.kind, &lhs, &rhs)
}

fn function_value(src: &str, f: &oxc_css_parser::ast::Function<'_>) -> ValResult<Value> {
    let name = match &f.name {
        FunctionName::Ident(InterpolableIdent::Literal(i)) => i.name.to_string(),
        _ => return Ok(Value::Raw(slice(src, f.span))),
    };
    let lower = name.to_ascii_lowercase();
    // calc / -webkit-calc: not SassScript, the inside is kept verbatim (only whitespace is
    // collapsed).
    if lower == "calc" || lower.ends_with("-calc") {
        let inner = inner_text(src, f.span, &name);
        return Ok(Value::Raw(format!(
            "{}({})",
            name.to_ascii_lowercase(),
            collapse_ws(&inner)
        )));
    }
    // `progid:...` is the verbatim form of an IE filter: passed through as is (whitespace
    // kept), only the quotes of strings are normalized.
    if name.contains("progid") {
        return Ok(Value::Raw(normalize_raw(&requote_strings(&slice(
            src, f.span,
        )))));
    }
    if has_raw_args(&f.args) {
        // Other verbatim arguments (`alpha(opacity=65)`): no spaces around `=` (observed
        // with dart-sass).
        return Ok(Value::Raw(strip_eq_spaces(&rewrite_raw(src, f.span))));
    }
    let args = from_sequence(src, &f.args, false)?;
    if let Some(value) = crate::scss::builtins::call_value(&lower, &args)? {
        return Ok(with_original_name(value, &name, &lower));
    }
    Ok(Value::Function(Function {
        name,
        args: Box::new(args),
    }))
}

/// A passed-through built-in function keeps the function name casing from the source
/// (bootstrap5's `RGBA(...)` exists to bypass old sass behavior, and dart keeps it as is).
pub(crate) fn with_original_name(value: Value, name: &str, lower: &str) -> Value {
    if name == lower {
        return value;
    }
    let swap = |text: &str| -> Option<String> {
        match text.strip_prefix(lower) {
            Some(rest) if rest.starts_with('(') => Some(format!("{name}{rest}")),
            _ => None,
        }
    };
    match value {
        Value::Str(text) if !text.quoted => match swap(&text.text) {
            Some(text) => Value::Str(SassStr::unquoted(text)),
            None => Value::Str(text),
        },
        Value::Raw(text) => match swap(&text) {
            Some(text) => Value::Raw(text),
            None => Value::Raw(text),
        },
        other => other,
    }
}

/// Built-in functions (S0 only implements the few needed for value serialization; the rest
/// pass through as unknown functions).
/// An argument contains "verbatim text that is not SassScript" (`alpha(opacity=65)`,
/// `url(...)`).
pub(crate) fn has_raw_args(args: &[ComponentValue]) -> bool {
    args.iter().any(|c| match c {
        ComponentValue::TokenWithSpan(_) => true,
        ComponentValue::Delimiter(d) => {
            !matches!(d.kind, DelimiterKind::Comma | DelimiterKind::Solidus)
        }
        _ => false,
    })
}

pub(crate) fn parse_number(raw: &str) -> f64 {
    raw.parse::<f64>().unwrap_or(0.0)
}

pub(crate) fn slice(src: &str, span: oxc_css_parser::pos::Span) -> String {
    src.get(span.start..span.end).unwrap_or("").to_string()
}

fn inner_text(src: &str, span: oxc_css_parser::pos::Span, name: &str) -> String {
    let text = slice(src, span);
    let open = text.find('(').map(|i| i + 1).unwrap_or(text.len());
    let close = text.rfind(')').unwrap_or(text.len());
    let _ = name;
    text.get(open..close).unwrap_or("").to_string()
}

/// Collapse whitespace and normalize string quotes (used for calc / progid / url / at-rule
/// prelude).
pub(crate) fn rewrite_raw(src: &str, span: oxc_css_parser::pos::Span) -> String {
    normalize_raw(&requote_strings(&slice(src, span)))
}

/// Rewrite the strings in verbatim text by dart-sass's quoting rules (fewer escapes win;
/// double quotes on a tie).
pub(crate) fn requote_strings(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            out.push(ch);
            if let Some(next) = chars.next() {
                out.push(next);
            }
            continue;
        }
        if ch != '\'' && ch != '"' {
            out.push(ch);
            continue;
        }
        let quote = ch;
        let mut raw = String::new();
        raw.push(quote);
        while let Some(c) = chars.next() {
            raw.push(c);
            if c == '\\' {
                if let Some(next) = chars.next() {
                    raw.push(next);
                }
            } else if c == quote {
                break;
            }
        }
        out.push_str(&quote_string(&decode_css_string(&raw)));
    }
    out
}

/// Decode a quoted CSS string (including hex escapes).
pub(crate) fn decode_css_string(raw: &str) -> String {
    let inner = raw
        .strip_prefix('\'')
        .or_else(|| raw.strip_prefix('"'))
        .unwrap_or(raw);
    let inner = inner
        .strip_suffix('\'')
        .or_else(|| inner.strip_suffix('"'))
        .unwrap_or(inner);
    decode_css_escapes(inner)
}

/// Decode the escapes inside a string body (without the surrounding quotes).
pub(crate) fn decode_css_escapes(inner: &str) -> String {
    let mut out = String::with_capacity(inner.len());
    let chars: Vec<char> = inner.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '\\' {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        i += 1;
        if i >= chars.len() {
            break;
        }
        if chars[i] == '\n' {
            i += 1;
            continue;
        }
        if chars[i].is_ascii_hexdigit() {
            let mut hex = String::new();
            while i < chars.len() && hex.len() < 6 && chars[i].is_ascii_hexdigit() {
                hex.push(chars[i]);
                i += 1;
            }
            if i < chars.len() && chars[i].is_whitespace() {
                i += 1;
            }
            if let Ok(cp) = u32::from_str_radix(&hex, 16) {
                if let Some(c) = char::from_u32(cp) {
                    out.push(c);
                    continue;
                }
            }
            out.push('\u{FFFD}');
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// CSS identifier check (used for dart-sass's attribute selector quote-removal rule).
pub(crate) fn is_css_identifier(text: &str) -> bool {
    let mut chars = text.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    let rest = chars.as_str();
    let start_ok = if first == '-' {
        rest.chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_' || !c.is_ascii())
    } else {
        first.is_ascii_alphabetic() || first == '_' || !first.is_ascii()
    };
    if !start_ok {
        return false;
    }
    text.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || !c.is_ascii())
}

pub(crate) fn normalize_raw(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut quote: Option<char> = None;
    let mut pending_space = false;
    let mut chars = text.chars().peekable();
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
            '\'' | '"' => {
                if pending_space && !out.is_empty() && !out.ends_with(' ') {
                    out.push(' ');
                }
                pending_space = false;
                quote = Some(ch);
                out.push(ch);
            }
            c if c.is_whitespace() => pending_space = true,
            c => {
                if pending_space && !out.is_empty() && !out.ends_with(' ') {
                    out.push(' ');
                }
                pending_space = false;
                out.push(c);
            }
        }
    }
    out
}

/// Inside `calc()`: collapse whitespace but keep the leading/trailing ones (observed with
/// dart-sass).
pub(crate) fn collapse_ws(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_space = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            in_space = true;
        } else {
            if in_space {
                out.push(' ');
                in_space = false;
            }
            out.push(ch);
        }
    }
    if in_space {
        out.push(' ');
    }
    out
}

const NAMED_COLORS: &[(&str, u32)] = &[
    ("aliceblue", 0xf0f8ff),
    ("antiquewhite", 0xfaebd7),
    ("aqua", 0x00ffff),
    ("aquamarine", 0x7fffd4),
    ("azure", 0xf0ffff),
    ("beige", 0xf5f5dc),
    ("bisque", 0xffe4c4),
    ("black", 0x000000),
    ("blanchedalmond", 0xffebcd),
    ("blue", 0x0000ff),
    ("blueviolet", 0x8a2be2),
    ("brown", 0xa52a2a),
    ("burlywood", 0xdeb887),
    ("cadetblue", 0x5f9ea0),
    ("chartreuse", 0x7fff00),
    ("chocolate", 0xd2691e),
    ("coral", 0xff7f50),
    ("cornflowerblue", 0x6495ed),
    ("cornsilk", 0xfff8dc),
    ("crimson", 0xdc143c),
    ("cyan", 0x00ffff),
    ("darkblue", 0x00008b),
    ("darkcyan", 0x008b8b),
    ("darkgoldenrod", 0xb8860b),
    ("darkgray", 0xa9a9a9),
    ("darkgreen", 0x006400),
    ("darkgrey", 0xa9a9a9),
    ("darkkhaki", 0xbdb76b),
    ("darkmagenta", 0x8b008b),
    ("darkolivegreen", 0x556b2f),
    ("darkorange", 0xff8c00),
    ("darkorchid", 0x9932cc),
    ("darkred", 0x8b0000),
    ("darksalmon", 0xe9967a),
    ("darkseagreen", 0x8fbc8f),
    ("darkslateblue", 0x483d8b),
    ("darkslategray", 0x2f4f4f),
    ("darkslategrey", 0x2f4f4f),
    ("darkturquoise", 0x00ced1),
    ("darkviolet", 0x9400d3),
    ("deeppink", 0xff1493),
    ("deepskyblue", 0x00bfff),
    ("dimgray", 0x696969),
    ("dimgrey", 0x696969),
    ("dodgerblue", 0x1e90ff),
    ("firebrick", 0xb22222),
    ("floralwhite", 0xfffaf0),
    ("forestgreen", 0x228b22),
    ("fuchsia", 0xff00ff),
    ("gainsboro", 0xdcdcdc),
    ("ghostwhite", 0xf8f8ff),
    ("gold", 0xffd700),
    ("goldenrod", 0xdaa520),
    ("gray", 0x808080),
    ("green", 0x008000),
    ("greenyellow", 0xadff2f),
    ("grey", 0x808080),
    ("honeydew", 0xf0fff0),
    ("hotpink", 0xff69b4),
    ("indianred", 0xcd5c5c),
    ("indigo", 0x4b0082),
    ("ivory", 0xfffff0),
    ("khaki", 0xf0e68c),
    ("lavender", 0xe6e6fa),
    ("lavenderblush", 0xfff0f5),
    ("lawngreen", 0x7cfc00),
    ("lemonchiffon", 0xfffacd),
    ("lightblue", 0xadd8e6),
    ("lightcoral", 0xf08080),
    ("lightcyan", 0xe0ffff),
    ("lightgoldenrodyellow", 0xfafad2),
    ("lightgray", 0xd3d3d3),
    ("lightgreen", 0x90ee90),
    ("lightgrey", 0xd3d3d3),
    ("lightpink", 0xffb6c1),
    ("lightsalmon", 0xffa07a),
    ("lightseagreen", 0x20b2aa),
    ("lightskyblue", 0x87cefa),
    ("lightslategray", 0x778899),
    ("lightslategrey", 0x778899),
    ("lightsteelblue", 0xb0c4de),
    ("lightyellow", 0xffffe0),
    ("lime", 0x00ff00),
    ("limegreen", 0x32cd32),
    ("linen", 0xfaf0e6),
    ("magenta", 0xff00ff),
    ("maroon", 0x800000),
    ("mediumaquamarine", 0x66cdaa),
    ("mediumblue", 0x0000cd),
    ("mediumorchid", 0xba55d3),
    ("mediumpurple", 0x9370db),
    ("mediumseagreen", 0x3cb371),
    ("mediumslateblue", 0x7b68ee),
    ("mediumspringgreen", 0x00fa9a),
    ("mediumturquoise", 0x48d1cc),
    ("mediumvioletred", 0xc71585),
    ("midnightblue", 0x191970),
    ("mintcream", 0xf5fffa),
    ("mistyrose", 0xffe4e1),
    ("moccasin", 0xffe4b5),
    ("navajowhite", 0xffdead),
    ("navy", 0x000080),
    ("oldlace", 0xfdf5e6),
    ("olive", 0x808000),
    ("olivedrab", 0x6b8e23),
    ("orange", 0xffa500),
    ("orangered", 0xff4500),
    ("orchid", 0xda70d6),
    ("palegoldenrod", 0xeee8aa),
    ("palegreen", 0x98fb98),
    ("paleturquoise", 0xafeeee),
    ("palevioletred", 0xdb7093),
    ("papayawhip", 0xffefd5),
    ("peachpuff", 0xffdab9),
    ("peru", 0xcd853f),
    ("pink", 0xffc0cb),
    ("plum", 0xdda0dd),
    ("powderblue", 0xb0e0e6),
    ("purple", 0x800080),
    ("rebeccapurple", 0x663399),
    ("red", 0xff0000),
    ("rosybrown", 0xbc8f8f),
    ("royalblue", 0x4169e1),
    ("saddlebrown", 0x8b4513),
    ("salmon", 0xfa8072),
    ("sandybrown", 0xf4a460),
    ("seagreen", 0x2e8b57),
    ("seashell", 0xfff5ee),
    ("sienna", 0xa0522d),
    ("silver", 0xc0c0c0),
    ("skyblue", 0x87ceeb),
    ("slateblue", 0x6a5acd),
    ("slategray", 0x708090),
    ("slategrey", 0x708090),
    ("snow", 0xfffafa),
    ("springgreen", 0x00ff7f),
    ("steelblue", 0x4682b4),
    ("tan", 0xd2b48c),
    ("teal", 0x008080),
    ("thistle", 0xd8bfd8),
    ("tomato", 0xff6347),
    ("turquoise", 0x40e0d0),
    ("violet", 0xee82ee),
    ("wheat", 0xf5deb3),
    ("white", 0xffffff),
    ("whitesmoke", 0xf5f5f5),
    ("yellow", 0xffff00),
    ("yellowgreen", 0x9acd32),
];

#[cfg(test)]
mod tests {
    use super::*;
    use oxc_css_parser::{Allocator, Parser, Syntax};

    fn eval(expr: &str) -> Value {
        let css = format!(".x{{y:{expr}}}");
        let alloc = Allocator::default();
        let mut parser = Parser::new(&alloc, &css, Syntax::Scss);
        let ast = parser.parse::<oxc_css_parser::ast::Stylesheet>().unwrap();
        let stmt = &ast.statements[0];
        let oxc_css_parser::ast::Statement::QualifiedRule(rule) = stmt else {
            panic!("rule");
        };
        let oxc_css_parser::ast::Statement::Declaration(decl) = &rule.block.statements[0] else {
            panic!("decl");
        };
        from_components(&css, &decl.value).unwrap()
    }

    fn css(expr: &str) -> String {
        eval(expr).to_css().unwrap()
    }

    #[test]
    fn number_ten_decimals_and_trim() {
        assert_eq!(Number::unitless(1.0 / 3.0).format(), "0.3333333333");
        assert_eq!(Number::unitless(1.0).format(), "1");
        assert_eq!(Number::with_unit(0.5, "em").format(), "0.5em");
        assert_eq!(Number::with_unit(0.0, "px").format(), "0px");
        assert_eq!(Number::unitless(1e21).format(), "1000000000000000000000");
        assert_eq!(Number::unitless(0.00000000001).format(), "0");
        assert_eq!(Number::unitless(-0.00000000001).format(), "0");
    }

    #[test]
    fn number_rounds_shortest_decimal() {
        // dart-sass takes the "shortest decimal string, then round to 10 places", not an
        // f64 binary round
        assert_eq!(Number::format_value(1.23456789015), "1.2345678902");
        assert_eq!(Number::format_value(-1.23456789015), "-1.2345678902");
        assert_eq!(Number::format_value(0.99999999996), "1");
        assert_eq!(Number::format_value(2.00000000005), "2.0000000001");
        assert_eq!(Number::format_value(0.1 + 0.2), "0.3");
        assert_eq!(Number::format_value(1e-7), "0.0000001");
        assert_eq!(Number::format_value(9999999999.99999999995), "10000000000");
    }

    #[test]
    fn compound_units() {
        let n = Number::with_unit(2.0, "px").mul(&Number::with_unit(3.0, "px"));
        assert_eq!(n.value, 6.0);
        assert_eq!(n.format(), "6px*px");
        // Compound units cannot go into CSS directly (dart-sass errors)
        assert!(Value::Number(n).to_css().is_err());

        let d = Number::with_unit(10.0, "px")
            .div(&Number::with_unit(2.0, "px"))
            .unwrap();
        assert_eq!(d.format(), "5");

        let d2 = Number::with_unit(1.0, "px")
            .div(&Number::with_unit(2.0, "s"))
            .unwrap();
        assert_eq!(d2.format(), "0.5px*s^-1");
    }

    #[test]
    fn incompatible_units_error() {
        let err = Number::with_unit(1.0, "px")
            .add(&Number::with_unit(1.0, "em"))
            .unwrap_err();
        assert!(err.contains("incompatible units"), "{err}");
        assert!(
            Number::with_unit(1.0, "px")
                .compare(&Number::with_unit(1.0, "em"))
                .is_err()
        );
        // Convertible within the same dimension
        let sum = Number::with_unit(1.0, "in")
            .add(&Number::with_unit(1.0, "px"))
            .unwrap();
        assert_eq!(sum.format(), "1.0104166667in");
    }

    #[test]
    fn color_literal_vs_computed() {
        assert_eq!(css("#FF0000"), "#FF0000");
        assert_eq!(css("#f00"), "#f00");
        assert_eq!(css("fuchsia"), "fuchsia");
        assert_eq!(css("hsl(0,100%,50%)"), "red");
        assert_eq!(css("rgb(18,52,86)"), "#123456");
        assert_eq!(css("rgba(0,0,0,.5)"), "rgba(0, 0, 0, 0.5)");
        assert_eq!(css("mix(red,blue)"), "purple");
    }

    #[test]
    fn quote_choice() {
        assert_eq!(css("\"a\\\"b\""), "'a\"b'");
        assert_eq!(css("\"it's\""), "\"it's\"");
        assert_eq!(css("'a\"b'"), "'a\"b'");
    }

    #[test]
    fn empty_parens_is_empty_list() {
        let value = eval("()");
        match value {
            Value::List(list) => {
                assert!(list.items.is_empty());
                assert!(!list.bracketed);
            }
            other => panic!("expected empty list, got {other:?}"),
        }
        assert!(eval("()").to_css().is_err());
        assert_eq!(css("() 1"), "1");
    }

    #[test]
    fn bracket_list_separator() {
        assert_eq!(css("[1, 2]"), "[1, 2]");
        assert_eq!(css("[1 2]"), "[1 2]");
        let Value::List(list) = eval("[1, 2]") else {
            panic!("list");
        };
        assert!(list.bracketed);
        assert_eq!(list.sep, Sep::Comma);
        assert_eq!(list.items.len(), 2);
    }

    #[test]
    fn slash_and_space_lists() {
        assert_eq!(css("12px/1.5 sans-serif"), "12px/1.5 sans-serif");
        assert_eq!(css("10px 10px / 5px"), "10px 10px/5px");
        assert_eq!(css("1 / 2"), "1/2");
        assert_eq!(css("(1 / 2)"), "0.5");
        assert_eq!(css("1 + 2"), "3");
        assert_eq!(css("2 * 3"), "6");
    }

    #[test]
    fn escaped_units_and_hex() {
        // IE hack: the unit of `1px\9` is an escaped identifier, so the trailing space must
        // be kept
        assert_eq!(css("1px\\9"), "1px\\9 ");
        assert_eq!(css("1p\\9x"), "1p\\9 x");
        assert_eq!(css("1px\\9\\9"), "1px\\9 \\9 ");
        assert_eq!(css("c\\9"), "c\\9 ");
        // A color token ends at the escape: `#000\9` = color + escaped identifier
        assert_eq!(css("#000\\9"), "#000 \\9 ");
        assert_eq!(css("10\\25"), "10\\%");
    }

    #[test]
    fn ident_escaping() {
        assert_eq!(css("\\9"), "\\9 ");
        assert_eq!(css("\\5FAE\\8F6F"), "\u{5FAE}\u{8F6F}");
    }
}
