//! Sass 类型化值模型：数字（复合单位）、颜色、字符串、list / map、函数值。
//!
//! 与 less 的字符串式值不同，这里每个值都带语义，序列化规则对齐 dart-sass 1.35.2：
//! 数字 10 位小数、颜色字面量保留原拼写、字符串选转义更少的引号。

use std::cell::Cell;
use std::rc::Rc;

use oxc_css_parser::ast::*;

use super::color_space::{self, Space};

/// 值模型。`Raw` 承载无法结构化解释的原文（`progid:`、url、unicode-range 等）。
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
    /// `get-function("name")` 得到的一等函数值（`call()` 的目标）。
    FuncRef(FuncRef),
    Raw(String),
}

/// 一等函数：名字 + 来源（内置模块成员 / 用户模块命名空间 / 纯 CSS 函数）。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FuncRef {
    pub(crate) name: String,
    /// 内置模块成员（`meta.get-function("div", $module: "math")`）。
    pub(crate) module: Option<crate::scss::modules::Builtin>,
    /// 用户模块的命名空间。
    pub(crate) namespace: Option<String>,
    /// `$css: true`：调用时原样产出 CSS 函数。
    pub(crate) css: bool,
}

/// list 分隔符。逗号/空格/slash 分别输出 `, ` / ` ` / ` / `。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Sep {
    Comma,
    Space,
    /// 真正的斜杠 list（`list.slash()`），dart 序列化成 `1 / 2`。
    Slash,
    /// 源码里写的 `a/b`：dart 把它当成「斜杠数 / 字符串」，序列化不带空格。
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

/// 参数列表（`$args...`）：多出的位置实参 + 未被形参认领的关键字实参。
/// `keywords_accessed` 记录“有没有人读过关键字 map”（`keywords($args)` 或再 `...` 展开），
/// dart 用它决定要不要报「多余的命名实参」（实测：读过就不报）。
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
    /// 实参：通常是逗号 list。
    pub(crate) args: Box<Value>,
}

/// 去掉 `=` 两侧空白（函数实参里的 `opacity = 65` → `opacity=65`）。
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
    /// 从 CSS 源里解出的标识符：序列化时按 CSS 转义规则重写（`\9` → `\9 `）。
    /// 计算/插值产生的字符串是「已就绪的文本」，原样输出。
    pub(crate) escape: bool,
}

/// 数字 + 单位代数：`numer` / `denom` 都按出现顺序保存原始单位拼写。
///
/// `2px * 3px` → `6px*px`；`10px / 2px` → `5`；复合单位不能直接进 CSS（dart-sass 报错），
/// 但 `inspect()` 需要能打印。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Number {
    pub(crate) value: f64,
    pub(crate) numer: Vec<String>,
    pub(crate) denom: Vec<String>,
    /// `1/2` 这类「斜杠数」：值已算出，但 CSS 输出保留原始两侧拼写。
    /// dart-sass 用 `SassNumber.withSlash` 表达同一语义。
    pub(crate) slash: Option<Box<(Value, Value)>>,
}

/// 颜色：同时保留「原始字面量拼写」和 RGBA。字面量原样输出，计算值才规范化。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Color {
    pub(crate) r: f64,
    pub(crate) g: f64,
    pub(crate) b: f64,
    pub(crate) a: f64,
    pub(crate) literal: Option<String>,
    /// 来自 HSL 运算的颜色会带上原始 HSL 分量（度 / 百分比），后续 HSL 运算
    /// 直接用它，而不是从取整后的 RGB 反推（dart 的 `_hue/_saturation/_lightness`）。
    pub(crate) hsl: Option<(f64, f64, f64)>,
    /// 颜色所属空间（`color.space()`）：legacy 是 rgb/hsl/hwb，现代构造是各自的。
    pub(crate) space: Option<Space>,
    /// 非 legacy 空间的原始通道（`None` 是 CSS 的 `none`/missing）。
    /// `hsl` 空间用上面的 `hsl` 字段（避免取整损失）。
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

    /// `unit` 按最终 CSS 形态给出（调用方负责转义）；`%` 直接传 `"%"`。
    pub(crate) fn with_unit(value: f64, unit: &str) -> Self {
        Self {
            value,
            numer: vec![unit.to_string()],
            denom: Vec::new(),
            slash: None,
        }
    }

    /// 除法的原始写法：结果按数值计算，但序列化用两侧的 `inspect()`。
    pub(crate) fn with_slash(result: Self, left: Value, right: Value) -> Self {
        Self {
            slash: Some(Box::new((left, right))),
            ..result
        }
    }

    /// 去掉斜杠写法（变量赋值 / 参与运算时 dart-sass 会调 `withoutSlash()`）。
    pub(crate) fn without_slash(mut self) -> Self {
        self.slash = None;
        self
    }

    pub(crate) fn is_unitless(&self) -> bool {
        self.numer.is_empty() && self.denom.is_empty()
    }

    /// 是否是「简单单位」：无单位或单个分子单位。只有这种值能直接进 CSS。
    pub(crate) fn is_simple(&self) -> bool {
        self.denom.is_empty() && self.numer.len() <= 1
    }

    pub(crate) fn unit(&self) -> &str {
        self.numer.first().map(String::as_str).unwrap_or("")
    }

    /// 单位串：`px*px/s*s`。单位按最终 CSS 形态保存（`px`、`%`、`px\9 `）。
    pub(crate) fn unit_string(&self) -> String {
        let mut out = self.numer.join("*");
        if !self.denom.is_empty() {
            out.push('/');
            out.push_str(&self.denom.join("*"));
        }
        out
    }

    /// 数字（不含单位）的 CSS 文本：10 位小数、去尾零、大整数完整展开。
    pub(crate) fn format_value(value: f64) -> String {
        if value.is_nan() {
            return "NaN".to_string();
        }
        if value.is_infinite() {
            return if value > 0.0 { "Infinity" } else { "-Infinity" }.to_string();
        }
        // dart-sass 把 double 的最短十进制表示四舍五入到 10 位小数
        // （字符串级 half-up：1.23456789015 → 1.2345678902）。
        let mut text = format!("{value}");
        if text.contains('e') || text.contains('E') {
            text = expand_exponent(&text);
        }
        let text = round_decimal(&text, 10);
        if text == "-0" { "0".to_string() } else { text }
    }

    /// 带单位的 CSS 文本。复合单位由调用方先报错。
    pub(crate) fn format(&self) -> String {
        if let Some(boxed) = &self.slash {
            let (left, right) = &**boxed;
            return format!("{}/{}", left.inspect(), right.inspect());
        }
        // 复合单位按 dart 的调试写法：`px*px`、`px*s^-1`（错误文案要对齐）。
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

    /// 加减：单位必须可换算（同单位或同量纲），否则报错。
    fn arith(&self, rhs: &Self, lhs_scale: f64, rhs_scale: f64) -> ValResult<Self> {
        let (lhs_value, rhs_value, numer, denom) = if self.is_unitless() && rhs.is_unitless() {
            (self.value, rhs.value, Vec::new(), Vec::new())
        } else if self.is_unitless() || rhs.is_unitless() {
            // 无单位值沿用另一侧的单位（dart-sass: 1 + 1px == 2px）。
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
        // dart-sass `moduloLikeSass`：符号跟随除数（Dart `%` 是非负余数）。
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

    /// 把 `rhs` 换算成本单位的数值（dart-sass `_coerceUnits`，coerceUnitless=true）。
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

    /// 比较：单位需兼容；无单位与有单位按数值比较。
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
            .ok_or_else(|| "无法比较的数字。".to_string())
    }
}

/// 十进制字符串按 `places` 位小数四舍五入（half-up），再去掉尾随零。
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

/// 把 `1e21` / `1e-7` 展开成定点十进制。
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

/// 同量纲单位换算因子：`rhs` 单位的值 * factor = 以 `lhs` 单位表示的值。
fn convert_factor(rhs: &str, lhs: &str) -> Option<f64> {
    if rhs == lhs {
        return Some(1.0);
    }
    let (rg, rf) = unit_kind(rhs)?;
    let (lg, lf) = unit_kind(lhs)?;
    (rg == lg).then_some(rf / lf)
}

/// 同量纲单位换算因子（供 `@for` 的边界换算复用）。
pub(crate) fn convert_factor_public(rhs: &str, lhs: &str) -> Option<f64> {
    convert_factor(rhs, lhs)
}

/// 单位量纲与其换算表（dart 报错文案里的顺序）：`("length", ["in", ...])`。
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

/// dart 的 `fuzzyLessThan`：`0.4999999999999` 与 `0.5` 视为相等（不取整成「小」）。
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
    /// 通道按 f64 原样存储（dart 1.93 的 `SassColor.rgbInternal`）：链式运算
    /// 不丢精度，只在 `to_css()` 序列化时取整。
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

    /// 从 `#rgb` / `#rrggbb` / `#rrggbbaa` 解析；保留原字面量拼写。
    pub(crate) fn from_hex(raw: &str) -> Option<Self> {
        let hex = raw.strip_prefix('#')?;
        let digits: Vec<u8> = hex
            .chars()
            .map(|c| c.to_digit(16).map(|d| d as u8))
            .collect::<Option<Vec<_>>>()?;
        let nibble = |i: usize| digits[i] as f64;
        // 6/8 位写法是「两位一组」的字节，不是 nibble。
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

    /// 颜色名（大小写不敏感）。字面量拼写由调用方保留。
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

    /// 序列化取整：四舍五入到最近整数（dart 的 `fuzzyRound`）。
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

    /// `color.space()`：显式空间优先，其次是 legacy 的 hsl/hwb 标记。
    pub(crate) fn space(&self) -> Space {
        self.space.unwrap_or(Space::Rgb)
    }

    /// 当前空间的通道（显示单位；`None` 表示 missing）。
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

    /// 显示单位 → 空间数学单位（`oklab`/`oklch` 的 lightness 数学上是 0..1）。
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

    /// 现代构造：通道按显示单位存，RGB 走 `Color::new` 的取整与字面量规则。
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

    /// 转到另一个空间（`color.to-space()`）。
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
        // 颜色在**目标空间**里存：legacy 目标也记 `space`，否则 `channel($c, "hue", hsl)`
        // 会退化回 RGB 通道；序列化那侧仍按 legacy 规则（十六进制）。
        let mut color = Self::new(rgb[0] * 255.0, rgb[1] * 255.0, rgb[2] * 255.0, self.a);
        color.space = Some(space);
        color.channels = Some([Some(display[0]), Some(display[1]), Some(display[2])]);
        if space == Space::Hsl {
            color.hsl = Some((display[0], display[1], display[2]));
        }
        color
    }

    /// 非 legacy 空间按自身空间序列化（`lab()` / `oklch()` / `color()`）。
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

    /// 从 CSS 源文件里读出的标识符（需要按 CSS 转义规则序列化）。
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

/// 只在字符串含 `"` 且不含 `'` 时用单引号（dart 的 `_visitQuotedString`：先按
/// 双引号扫描，遇到 `"` 又已经出现 `'` 时整体重来并强制双引号）。
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
            // NUL 在 CSS 字符串里不合法：dart 写成 U+FFFD（标识符里则是 `\\0 `）。
            '\0' => out.push('\u{FFFD}'),
            // 制表符原样写出（dart 的控制字符转义列表里没有 tab）。
            '\t' => out.push('\t'),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\{:x}", c as u32));
                // 十六进制转义后面紧跟十六进制字符/空白时，补一个空格断句。
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

/// 未加引号字符串/标识符的序列化：非法字符转义；结尾是十六进制转义时补空格。
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

/// dart-sass 的 `isPrivateUse`：BMP 私用区 U+E000–U+F8FF 与 15/16 平面。
///
/// 图标字体把它们当作字形码位（`content: "\e644"`），dart 在展开模式下总是写成
/// 十六进制转义；不转义的话产物会带上非 ASCII 字节，进而多出一条 `@charset`。
fn is_private_use(ch: char) -> bool {
    matches!(ch as u32, 0xE000..=0xF8FF | 0xF0000..=0x10FFFF)
}

/// 未加引号字符串的最小转义：只把私用区写成十六进制转义，其余原样
/// （dart 的 `_visitUnquotedString`）。
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

/// 把私用区字符写成十六进制转义；后面紧跟十六进制字符或空白时补一个空格断句。
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

    /// 作为 CSS 值输出。`null` 返回空串由声明层丢弃；map/空 list/复合单位报错。
    pub(crate) fn to_css(&self) -> ValResult<String> {
        Ok(match self {
            // 带斜杠写法的数字直接按原写法输出（`10/4px`），即使复合单位不能算值
            // —— dart 只在真正求值后才校验单位（`$x: 10/4px; a: $x` 才报错）。
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
                    // null 与空 list 在 list 序列化里是透明元素（`1 null` → `1`）。
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
                // `$rest` 用作值时就相当于一个逗号 list（dart 实测：`y: 2, 3`）。
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

    /// 函数实参序列化：null 保留占位（`foo(null, 1)` → `foo(, 1)`），与值 list 不同。
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

    /// 插值 `#{}` 的文本：字符串取原文（去引号），其余同 CSS。
    /// 插值文本：null 为空串；字符串给出裸文本；其余按 CSS 序列化。
    /// map / 空 list 之类不能进 CSS 的值在这里就报错（dart 同样报错）。
    pub(crate) fn interpolation(&self) -> ValResult<String> {
        match self {
            Value::Null => Ok(String::new()),
            Value::Str(s) => Ok(s.text.clone()),
            // 插值里 list 的成员同样不引号。
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

    /// 变量赋值时 dart-sass 调 `withoutSlash()`（list 递归）。
    pub(crate) fn without_slash(&self) -> Value {
        match self {
            Value::Number(n) => Value::Number(n.clone().without_slash()),
            Value::List(l) => Value::List(list_map(l, |v| v.without_slash())),
            other => other.clone(),
        }
    }

    /// `inspect()`：调试表示，复合单位在这里可见（`6px*px`）。
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
                // dart 的 inspect：空 list 是 `()`，单元素逗号 list 带尾逗号 `(1,)`。
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
                // dart 的 `inspect()` 只看位置实参（关键字实参不可见）：`()` 恒为 `()`。
                if args.positional.is_empty() {
                    "()".to_string()
                } else if args.positional.len() == 1 {
                    // dart：单元素逗号 list 的 inspect 带尾逗号（`(2px,)`）。
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

/// 二元算术（dart-sass 的 Value 方法语义）。
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

/// 非数值运算的字符串拼接（dart-sass `Value.plus` / `SassString.plus`）。
fn concat(lhs: &Value, rhs: &Value, op: &SassBinaryOperatorKind) -> Value {
    use SassBinaryOperatorKind::*;
    match (op, lhs) {
        (Plus, Value::Str(a)) => Value::Str(SassStr {
            text: match rhs {
                Value::Str(b) => format!("{}{}", a.text, b.text),
                // 右操作数是字符串时用它的引号形态（`1 + "x"` → `"1x"`）。
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
                unreachable!("已由 guard 保证")
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

/// 除法：数字相除；颜色涉及除法是未定义操作；其余按字符串拼接（dart 语义）。
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

/// 名字归一比较：`$a_b` ≡ `$a-b`（dart 的标识符规则）。
pub(crate) fn names_eq(a: &str, b: &str) -> bool {
    a == b || (a.contains('_') || b.contains('_')) && a.replace('_', "-") == b.replace('_', "-")
}

/// list 语义：单值是一个 1 元素 list，`null` 也是 1 元素 list，map 是 `(k v)` 对的 list。
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

/// map 语义：`()`（空 list）也是空 map（dart 的 `assertMap` 如此）。
pub(crate) fn as_map(value: &Value, name: &str) -> ValResult<Vec<(Value, Value)>> {
    match value {
        Value::Map(map) => Ok(map.entries.clone()),
        Value::List(list) if list.items.is_empty() && !list.bracketed => Ok(Vec::new()),
        other => Err(format!("${name}: {} is not a map.", other.inspect())),
    }
}

impl Number {
    /// `assertInt`：`1.5 is not an int.`
    pub(crate) fn assert_int(&self, name: &str) -> ValResult<i64> {
        if (self.value - self.value.round()).abs() > EPSILON {
            return Err(format!("${name}: {} is not an int.", self.format()));
        }
        Ok(self.value.round() as i64)
    }

    /// dart `valueInRange`：越界报 `Expected 120 to be within 0 and 100.`
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

    /// 角度参数换算成度（`1turn` → 360）。
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
    /// 未取整的 RGB 通道（参与运算用）。
    pub(crate) fn channels_f64(&self) -> (f64, f64, f64) {
        (self.r, self.g, self.b)
    }

    /// 取整后的 RGB 通道（`red()` / `ie-hex-str()` 与 `==` 的展示口径）。
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

/// 单位能否换算的规范化形式：(量纲名, 乘数)。
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

/// 声明值：顶层逗号切分 → 空格切分 → slash 切分。
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

/// 函数实参 / bracket 内容：逗号是分隔符，其余同声明值。
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

/// 空格分隔：空白只在「两侧都不是 slash」时断开（`1 / 2` 是 slash list）。
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
    // 无空白但并列的组件（如函数名后面的 token）按空格 list 保底。
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
            // `10\25`：转义写出来的百分号，dart-sass 保留转义（`10\%`）。
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
                // `#000\9`：dart-sass 把它切成颜色 + 转义标识符（中间补空格）。
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
                // dart-sass 把 `()` 解析成空 map，语义上按空 list 处理。
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
        other => Err(format!("SCSS 值尚未支持: {}", slice(src, *other.span()))),
    }
}

/// 把 `#000\9` 拆成 `(#000, "\\9")`；`#` 后只认 3/4/6/8 位十六进制。
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
    // calc / -webkit-calc：不是 SassScript，内部按原文保留（仅折叠空白）。
    if lower == "calc" || lower.ends_with("-calc") {
        let inner = inner_text(src, f.span, &name);
        return Ok(Value::Raw(format!(
            "{}({})",
            name.to_ascii_lowercase(),
            collapse_ws(&inner)
        )));
    }
    // `progid:...` 是 IE filter 的原文写法：整体透传（空白保留），只规范化字符串引号。
    if name.contains("progid") {
        return Ok(Value::Raw(normalize_raw(&requote_strings(&slice(
            src, f.span,
        )))));
    }
    if has_raw_args(&f.args) {
        // 其余原文参数（`alpha(opacity=65)`）：`=` 两侧不留空格（dart-sass 实测）。
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

/// 透传的内置函数要沿用源码里的函数名大小写（bootstrap5 的 `RGBA(...)` 就是
/// 为了绕开旧版 sass 的行为，dart 原样保留）。
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

/// 内置函数（S0 只做值序列化必需的几个；其余按未知函数透传）。
/// 实参里出现「不是 SassScript 的原文」（`alpha(opacity=65)`、`url(...)`）。
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

/// 折叠空白并规范化字符串引号（用于 calc / progid / url / at-rule prelude）。
pub(crate) fn rewrite_raw(src: &str, span: oxc_css_parser::pos::Span) -> String {
    normalize_raw(&requote_strings(&slice(src, span)))
}

/// 把原文里的字符串按 dart-sass 引号规则重写（少转义优先、平手用双引号）。
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

/// 解码带引号的 CSS 字符串（含十六进制转义）。
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

/// 解码字符串体里的转义（不含外层引号）。
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

/// CSS 标识符判定（dart-sass 的 attribute selector 去引号规则）。
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

/// `calc()` 内部：折叠空白但保留首尾空格（dart-sass 实测）。
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
        // dart-sass 是「最短十进制串再四舍五入到 10 位」，不是 f64 二进制四舍五入
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
        // 复合单位不能直接进 CSS（dart-sass 报错）
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
        // 同量纲可换算
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
        // IE hack：`1px\9` 的单位是转义标识符，尾随空格必须保留
        assert_eq!(css("1px\\9"), "1px\\9 ");
        assert_eq!(css("1p\\9x"), "1p\\9 x");
        assert_eq!(css("1px\\9\\9"), "1px\\9 \\9 ");
        assert_eq!(css("c\\9"), "c\\9 ");
        // 颜色 token 在转义处结束：`#000\9` = 颜色 + 转义标识符
        assert_eq!(css("#000\\9"), "#000 \\9 ");
        assert_eq!(css("10\\25"), "10\\%");
    }

    #[test]
    fn ident_escaping() {
        assert_eq!(css("\\9"), "\\9 ");
        assert_eq!(css("\\5FAE\\8F6F"), "微软");
    }
}
