//! Sass 全局内置函数（dart-sass 1.35.2 的 legacy 函数表）。
//!
//! 表外的名字一律返回 `None`，由调用方**原样透传为纯 CSS**——这条边界很重要：
//! `sqrt(2)` / `getCssVar(--x)` 之类必须原样出现在产物里，不能因为「有函数表」就报错。
//! 表内的函数严格按 dart 语义求值，参数绑定（位置 / 关键字 / 默认值）与 `@function`
//! 共用规则。

use crate::scss::color_space::Space;
use crate::scss::value::{self, Color, List, Map, Number, SassStr, Sep, ValResult, Value};

/// 一次内置函数调用的形参绑定结果。
pub(crate) struct Args {
    params: &'static [&'static str],
    values: Vec<Option<Value>>,
}

impl Args {
    pub(crate) fn new(
        params: &'static [&'static str],
        positional: &[Value],
        keywords: &[(String, Value)],
    ) -> ValResult<Self> {
        let mut values = vec![None; params.len()];
        let mut used = vec![false; keywords.len()];
        for (i, slot) in values.iter_mut().enumerate() {
            if i < positional.len() {
                if let Some(j) = keywords
                    .iter()
                    .position(|(k, _)| value::names_eq(k, params[i]))
                {
                    used[j] = true;
                    return Err(format!(
                        "Argument ${} was passed both by position and by name.",
                        params[i]
                    ));
                }
                *slot = Some(positional[i].clone());
            } else {
                if let Some(j) = keywords
                    .iter()
                    .position(|(k, _)| value::names_eq(k, params[i]))
                {
                    used[j] = true;
                    *slot = Some(keywords[j].1.clone());
                }
            }
        }
        // 多余位置实参。
        if positional.len() > params.len() {
            return Err(too_many(params.len(), positional.len()));
        }
        // 未知关键字实参。
        let unknown = keywords
            .iter()
            .zip(&used)
            .filter(|(_, used)| !**used)
            .map(|((name, _), _)| format!("${name}"))
            .collect::<Vec<_>>();
        if !unknown.is_empty() {
            return Err(format!(
                "No {} named {}.",
                if unknown.len() == 1 {
                    "argument"
                } else {
                    "arguments"
                },
                unknown.join(" or ")
            ));
        }
        Ok(Self { params, values })
    }

    pub(crate) fn at(&self, i: usize) -> Option<&Value> {
        self.values.get(i)?.as_ref()
    }

    pub(crate) fn req(&self, i: usize) -> ValResult<&Value> {
        self.at(i)
            .ok_or_else(|| format!("Missing argument ${}.", self.params[i]))
    }

    pub(crate) fn number(&self, i: usize) -> ValResult<&Number> {
        self.req(i)?.assert_number(self.params[i])
    }

    pub(crate) fn opt_number(&self, i: usize) -> ValResult<Option<&Number>> {
        self.at(i)
            .map(|v| v.assert_number(self.params[i]))
            .transpose()
    }

    pub(crate) fn color(&self, i: usize) -> ValResult<&Color> {
        self.req(i)?.assert_color(self.params[i])
    }

    pub(crate) fn string(&self, i: usize) -> ValResult<&SassStr> {
        self.req(i)?.assert_string(self.params[i])
    }

    /// 同 `string`，但把 `calc(...)` 这类「无法求值的函数」按其字面文本当成
    /// 未加引号字符串（dart 的 `type-of(calc(...))` 就是 `string`）。
    pub(crate) fn string_owned(&self, i: usize) -> ValResult<SassStr> {
        match self.req(i)? {
            Value::Str(text) => Ok(text.clone()),
            other @ (Value::Function(_) | Value::Raw(_)) => Ok(SassStr::unquoted(other.inspect())),
            other => Err(format!(
                "${}: {} is not a string.",
                self.params[i],
                other.inspect()
            )),
        }
    }
}

fn too_many(allowed: usize, passed: usize) -> String {
    format!(
        "Only {allowed} {} allowed, but {passed} {} passed.",
        if allowed == 1 {
            "argument"
        } else {
            "arguments"
        },
        if passed == 1 { "was" } else { "were" }
    )
}

/// 实参个数与文案里的函数名。
macro_rules! args {
    ($params:expr, $positional:expr, $keywords:expr) => {
        Args::new($params, $positional, $keywords)?
    };
}

/// 某个名字是不是内置函数（`function-exists()` 用）。空实参调用一次即可判定：
/// 内置函数会报「缺参数」，未知名字返回 `Ok(None)`。
pub(crate) fn is_builtin(name: &str) -> bool {
    !matches!(call(name, &[], &[]), Ok(None))
}

/// 内置函数分派。返回 `None` 表示「不是内置函数」（调用方透传成 CSS）。
pub(crate) fn call(
    name: &str,
    positional: &[Value],
    keywords: &[(String, Value)],
) -> ValResult<Option<Value>> {
    let value = match name {
        // ---- 颜色构造 ----
        "rgb" | "rgba" => rgb(name, positional, keywords)?,
        "hsl" | "hsla" => hsl(name, positional, keywords)?,
        // 现代颜色空间构造（1.35.2 里是 CSS 透传，1.93.2 是真正的颜色值）。
        "hwb" => modern_color("hwb", positional, keywords)?,
        "lab" => modern_color("lab", positional, keywords)?,
        "lch" => modern_color("lch", positional, keywords)?,
        "oklab" => modern_color("oklab", positional, keywords)?,
        "oklch" => modern_color("oklch", positional, keywords)?,
        "color" => modern_color("color", positional, keywords)?,
        // ---- 颜色调整 ----
        "mix" => mix(positional, keywords)?,
        "lighten" => lighten(positional, keywords)?,
        "darken" => darken(positional, keywords)?,
        "saturate" => saturate(positional, keywords)?,
        "desaturate" => desaturate(positional, keywords)?,
        "adjust-hue" => adjust_hue(positional, keywords)?,
        "grayscale" => grayscale(positional, keywords)?,
        "complement" => complement(positional, keywords)?,
        "invert" => invert(positional, keywords)?,
        "opacify" | "fade-in" => opacify(positional, keywords)?,
        "transparentize" | "fade-out" => transparentize(positional, keywords)?,
        "adjust-color" => adjust_color(positional, keywords, Mode::Adjust)?,
        "change-color" => adjust_color(positional, keywords, Mode::Change)?,
        "scale-color" => adjust_color(positional, keywords, Mode::Scale)?,
        "red" => channel(positional, keywords, Channel::Red)?,
        "green" => channel(positional, keywords, Channel::Green)?,
        "blue" => channel(positional, keywords, Channel::Blue)?,
        "hue" => channel(positional, keywords, Channel::Hue)?,
        "saturation" => channel(positional, keywords, Channel::Saturation)?,
        "lightness" => channel(positional, keywords, Channel::Lightness)?,
        "alpha" | "opacity" => alpha(positional, keywords)?,
        "ie-hex-str" => ie_hex_str(positional, keywords)?,
        // ---- 数字 ----
        "percentage" => percentage(positional, keywords)?,
        "round" | "ceil" | "floor" | "abs" => round_like(name, positional, keywords)?,
        "unit" => unit(positional, keywords)?,
        "unitless" => unitless(positional, keywords)?,
        "comparable" => comparable(positional, keywords)?,
        // `min()` / `max()` 只在实参能当纯 CSS 数学函数解析时透传（见 `eval`），
        // 其余情况（`min($a, 2)`）走这里的 Sass 语义。
        "min" | "max" => min_max(name, positional, keywords)?,
        "clamp" => clamp(positional, keywords)?,
        // ---- 字符串 ----
        "str-length" => str_length(positional, keywords)?,
        "str-slice" => str_slice(positional, keywords)?,
        "str-index" => str_index(positional, keywords)?,
        "str-insert" => str_insert(positional, keywords)?,
        "to-upper-case" => change_case(positional, keywords, true)?,
        "to-lower-case" => change_case(positional, keywords, false)?,
        "quote" => quote(positional, keywords)?,
        "unquote" => unquote(positional, keywords)?,
        "inspect" => inspect(positional, keywords)?,
        // ---- list ----
        "length" => length(positional, keywords)?,
        "nth" => nth(positional, keywords)?,
        "index" => index(positional, keywords)?,
        "append" => append(positional, keywords)?,
        "join" => join(positional, keywords)?,
        "zip" => zip(positional, keywords)?,
        "list-separator" => list_separator(positional, keywords)?,
        "set-nth" => set_nth(positional, keywords)?,
        // ---- map ----
        "map-get" => map_get(positional, keywords)?,
        "map-has-key" => map_has_key(positional, keywords)?,
        "map-merge" => map_merge(positional, keywords)?,
        "map-keys" => map_keys(positional, keywords)?,
        "map-values" => map_values(positional, keywords)?,
        "map-remove" => map_remove(positional, keywords)?,
        // ---- meta ----
        "type-of" => type_of(positional, keywords)?,
        "keywords" => keywords_map(positional, keywords)?,
        "feature-exists" => feature_exists(positional, keywords)?,
        "unique-id" => unique_id(positional, keywords)?,
        "is-bracketed" => is_bracketed(positional, keywords)?,
        _ => return Ok(None),
    };
    Ok(Some(value))
}

/// 旧的「值即实参」入口（`value::function_value` 用）：只有一个逗号 list 时按位置拆分。
pub(crate) fn call_value(name: &str, args: &Value) -> ValResult<Option<Value>> {
    let positional = match args {
        Value::List(list) if list.sep == Sep::Comma => list.items.clone(),
        Value::List(list) if list.items.is_empty() => Vec::new(),
        other => vec![other.clone()],
    };
    call(name, &positional, &[])
}

// ---------------------------------------------------------------------------
// 数字
// ---------------------------------------------------------------------------

fn percentage(positional: &[Value], keywords: &[(String, Value)]) -> ValResult<Value> {
    let args = args!(&["number"], positional, keywords);
    let number = args.number(0)?;
    if number.unit() == "%" {
        return Ok(Value::Number(number.clone()));
    }
    if !number.is_unitless() {
        return Err(format!(
            "$number: Expected {} to have no units.",
            number.format()
        ));
    }
    Ok(Value::Number(Number::with_unit(number.value * 100.0, "%")))
}

fn round_like(name: &str, positional: &[Value], keywords: &[(String, Value)]) -> ValResult<Value> {
    let args = args!(&["number"], positional, keywords);
    let number = args.number(0)?;
    let value = match name {
        "round" => number.value.round(),
        "ceil" => number.value.ceil(),
        "floor" => number.value.floor(),
        _ => number.value.abs(),
    };
    Ok(Value::Number(Number {
        value,
        numer: number.numer.clone(),
        denom: number.denom.clone(),
        slash: None,
    }))
}

fn unit(positional: &[Value], keywords: &[(String, Value)]) -> ValResult<Value> {
    let args = args!(&["number"], positional, keywords);
    Ok(Value::Str(SassStr::quoted(args.number(0)?.unit_string())))
}

fn unitless(positional: &[Value], keywords: &[(String, Value)]) -> ValResult<Value> {
    let args = args!(&["number"], positional, keywords);
    Ok(Value::Bool(args.number(0)?.is_unitless()))
}

fn comparable(positional: &[Value], keywords: &[(String, Value)]) -> ValResult<Value> {
    let args = args!(&["number1", "number2"], positional, keywords);
    let a = args.number(0)?;
    let b = args.number(1)?;
    Ok(Value::Bool(
        a.is_unitless() || b.is_unitless() || a.compare(b).is_ok(),
    ))
}

// ---------------------------------------------------------------------------
// 字符串
// ---------------------------------------------------------------------------

/// `min()` / `max()` 的 Sass 语义（dart 的 `_minMax`）：实参不是数字（或单位不可比）
/// 时报错，而不是退化成字符串。
fn min_max(name: &str, positional: &[Value], keywords: &[(String, Value)]) -> ValResult<Value> {
    if let Some((name, _)) = keywords.first() {
        return Err(format!("No argument named ${name}."));
    }
    let mut best: Option<&Number> = None;
    for value in positional {
        let Value::Number(number) = value else {
            return Err(format!("{} is not a number.", value.inspect()));
        };
        best = Some(match best {
            None => number,
            Some(current) => {
                current.compare(number)?;
                let smaller = current
                    .compare(number)
                    .is_ok_and(|ordering| ordering == std::cmp::Ordering::Greater);
                if (name == "min") == smaller {
                    number
                } else {
                    current
                }
            }
        });
    }
    match best {
        Some(number) => Ok(Value::Number(number.clone())),
        None => Err("At least one argument must be passed.".to_string()),
    }
}

/// `clamp($min, $number, $max)` = `max($min, min($number, $max))`（dart 的 `_clamp`）。
/// 单位不可比时报错，调用方（CSS 数学函数路径）据此退化成纯 CSS 透传。
fn clamp(positional: &[Value], keywords: &[(String, Value)]) -> ValResult<Value> {
    let args = args!(&["min", "number", "max"], positional, keywords);
    let (low, number, high) = (args.number(0)?, args.number(1)?, args.number(2)?);
    let upper = if number.compare(high)? == std::cmp::Ordering::Greater {
        high
    } else {
        number
    };
    Ok(Value::Number(
        if low.compare(upper)? == std::cmp::Ordering::Greater {
            low.clone()
        } else {
            upper.clone()
        },
    ))
}

fn str_length(positional: &[Value], keywords: &[(String, Value)]) -> ValResult<Value> {
    let args = args!(&["string"], positional, keywords);
    Ok(Value::Number(Number::unitless(
        args.string_owned(0)?.text.chars().count() as f64,
    )))
}

fn str_slice(positional: &[Value], keywords: &[(String, Value)]) -> ValResult<Value> {
    let args = args!(&["string", "start-at", "end-at"], positional, keywords);
    let source = args.string_owned(0)?;
    let text = &source.text;
    let chars = text.chars().collect::<Vec<_>>();
    let len = chars.len() as i64;
    let start = args.number(1)?.assert_int("start-at")?;
    let start = if start < 0 { len + start + 1 } else { start };
    let end = match args.opt_number(2)? {
        Some(n) => {
            let end = n.assert_int("end-at")?;
            if end < 0 { len + end + 1 } else { end }
        }
        None => -1,
    };
    let start = start.max(1);
    let end = if end < 0 { len } else { end.min(len) };
    let slice: String = if start > end {
        String::new()
    } else {
        chars[(start - 1) as usize..end as usize].iter().collect()
    };
    Ok(Value::Str(SassStr {
        text: slice,
        quoted: source.quoted,
        escape: false,
    }))
}

fn str_index(positional: &[Value], keywords: &[(String, Value)]) -> ValResult<Value> {
    let args = args!(&["string", "substring"], positional, keywords);
    let haystack = args.string_owned(0)?;
    let text = &haystack.text;
    let needle = &args.string(1)?.text;
    Ok(match text.find(needle.as_str()) {
        Some(byte) => Value::Number(Number::unitless(text[..byte].chars().count() as f64 + 1.0)),
        None => Value::Null,
    })
}

fn str_insert(positional: &[Value], keywords: &[(String, Value)]) -> ValResult<Value> {
    let args = args!(&["string", "insert", "index"], positional, keywords);
    let source = args.string_owned(0)?;
    let insert_owned = args.string_owned(1)?;
    let insert = &insert_owned.text;
    let chars = source.text.chars().collect::<Vec<_>>();
    let len = chars.len() as i64;
    let index = args.number(2)?.assert_int("index")?;
    let at = if index < 0 {
        (len + index + 1).max(0) as usize
    } else if index == 0 {
        0
    } else {
        (index - 1).min(len) as usize
    };
    let mut out = String::new();
    out.extend(&chars[..at]);
    out.push_str(insert);
    out.extend(&chars[at..]);
    Ok(Value::Str(SassStr {
        text: out,
        quoted: source.quoted,
        escape: false,
    }))
}

fn change_case(
    positional: &[Value],
    keywords: &[(String, Value)],
    upper: bool,
) -> ValResult<Value> {
    let args = args!(&["string"], positional, keywords);
    let source = args.string_owned(0)?;
    let text = if upper {
        source.text.to_uppercase()
    } else {
        source.text.to_lowercase()
    };
    Ok(Value::Str(SassStr {
        text,
        quoted: source.quoted,
        escape: false,
    }))
}

fn quote(positional: &[Value], keywords: &[(String, Value)]) -> ValResult<Value> {
    let args = args!(&["string"], positional, keywords);
    let source = args.req(0)?;
    Ok(Value::Str(SassStr::quoted(match source {
        Value::Str(s) => s.text.clone(),
        other => other.inspect(),
    })))
}

fn unquote(positional: &[Value], keywords: &[(String, Value)]) -> ValResult<Value> {
    let args = args!(&["string"], positional, keywords);
    let source = args.req(0)?;
    Ok(Value::Str(SassStr::unquoted(match source {
        Value::Str(s) => s.text.clone(),
        other => other.inspect(),
    })))
}

fn inspect(positional: &[Value], keywords: &[(String, Value)]) -> ValResult<Value> {
    let args = args!(&["value"], positional, keywords);
    Ok(Value::Str(SassStr::unquoted(args.req(0)?.inspect())))
}

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

fn length(positional: &[Value], keywords: &[(String, Value)]) -> ValResult<Value> {
    let args = args!(&["list"], positional, keywords);
    Ok(Value::Number(Number::unitless(
        value::list_items(args.req(0)?)
            .map(|items| items.len())
            .unwrap_or(1) as f64,
    )))
}

fn nth(positional: &[Value], keywords: &[(String, Value)]) -> ValResult<Value> {
    let args = args!(&["list", "n"], positional, keywords);
    let list = args.req(0)?;
    let items = value::list_items(list).unwrap_or_else(|| vec![list.clone()]);
    let index = args.number(1)?.assert_int("n")?;
    let len = items.len() as i64;
    if index == 0 {
        return Err("$n: List index may not be 0.".to_string());
    }
    let index = if index < 0 { len + index } else { index - 1 };
    if index < 0 || index >= len {
        return Err(format!(
            "$n: Invalid index {index} for a list with {len} elements."
        ));
    }
    Ok(items[index as usize].clone())
}

fn index(positional: &[Value], keywords: &[(String, Value)]) -> ValResult<Value> {
    let args = args!(&["list", "value"], positional, keywords);
    let list = args.req(0)?;
    let items = value::list_items(list).unwrap_or_else(|| vec![list.clone()]);
    Ok(
        match items
            .iter()
            .position(|item| value::values_equal(item, args.req(1).expect("已校验")))
        {
            Some(i) => Value::Number(Number::unitless(i as f64 + 1.0)),
            None => Value::Null,
        },
    )
}

fn append(positional: &[Value], keywords: &[(String, Value)]) -> ValResult<Value> {
    let args = args!(&["list", "val", "separator"], positional, keywords);
    let list = args.req(0)?;
    let mut items = value::list_items(list).unwrap_or_else(|| vec![list.clone()]);
    items.push(args.req(1)?.clone());
    let sep = separator(&args, 2, list)?;
    Ok(Value::List(List {
        items,
        sep,
        bracketed: matches!(list, Value::List(l) if l.bracketed),
    }))
}

fn join(positional: &[Value], keywords: &[(String, Value)]) -> ValResult<Value> {
    let args = args!(
        &["list1", "list2", "separator", "bracketed"],
        positional,
        keywords
    );
    let first = args.req(0)?;
    let second = args.req(1)?;
    let mut items = value::list_items(first).unwrap_or_else(|| vec![first.clone()]);
    items.extend(value::list_items(second).unwrap_or_else(|| vec![second.clone()]));
    let sep = separator(&args, 2, first)?;
    let bracketed = match args.at(3) {
        None => matches!(first, Value::List(l) if l.bracketed),
        Some(other) => other.truthy(),
    };
    Ok(Value::List(List {
        items,
        sep,
        bracketed,
    }))
}

/// `$separator`：`auto` 跟随第一个 list，`comma` / `space` / `slash` 显式指定。
fn separator(args: &Args, index: usize, list: &Value) -> ValResult<Sep> {
    match args.at(index) {
        None => Ok(match list {
            Value::List(l) => l.sep,
            _ => Sep::Space,
        }),
        Some(Value::Str(s)) if s.text == "auto" => Ok(match list {
            Value::List(l) => l.sep,
            _ => Sep::Space,
        }),
        Some(Value::Str(s)) if s.text == "comma" => Ok(Sep::Comma),
        Some(Value::Str(s)) if s.text == "space" => Ok(Sep::Space),
        Some(Value::Str(s)) if s.text == "slash" => Ok(Sep::Slash),
        Some(other) => Err(format!(
            "$separator: Must be \"space\", \"comma\", \"slash\", or \"auto\", was {}.",
            other.inspect()
        )),
    }
}

fn zip(positional: &[Value], keywords: &[(String, Value)]) -> ValResult<Value> {
    // dart 的签名是 `zip($lists...)`：所有位置实参都是 list。
    if let Some((name, _)) = keywords.first() {
        return Err(format!("No argument named ${name}."));
    }
    if positional.is_empty() {
        return Ok(Value::List(List {
            items: Vec::new(),
            sep: Sep::Comma,
            bracketed: false,
        }));
    }
    let columns = positional
        .iter()
        .map(|list| value::list_items(list).unwrap_or_else(|| vec![list.clone()]))
        .collect::<Vec<_>>();
    let rows = columns.iter().map(Vec::len).min().unwrap_or(0);
    Ok(Value::List(List {
        items: (0..rows)
            .map(|row| {
                Value::List(List {
                    items: columns.iter().map(|column| column[row].clone()).collect(),
                    sep: Sep::Space,
                    bracketed: false,
                })
            })
            .collect(),
        sep: Sep::Comma,
        bracketed: false,
    }))
}

fn list_separator(positional: &[Value], keywords: &[(String, Value)]) -> ValResult<Value> {
    let args = args!(&["list"], positional, keywords);
    let name = match args.req(0)? {
        Value::List(list) => match list.sep {
            Sep::Comma => "comma",
            Sep::Space => "space",
            Sep::Slash | Sep::SlashTight => "slash",
        },
        Value::ArgList(args) => match args.sep {
            Some(Sep::Comma) => "comma",
            Some(Sep::Slash | Sep::SlashTight) => "slash",
            _ => "space",
        },
        _ => "space",
    };
    Ok(Value::Str(SassStr::unquoted(name)))
}

fn set_nth(positional: &[Value], keywords: &[(String, Value)]) -> ValResult<Value> {
    let args = args!(&["list", "n", "value"], positional, keywords);
    let list = args.req(0)?;
    let mut items = value::list_items(list).unwrap_or_else(|| vec![list.clone()]);
    let index = args.number(1)?.assert_int("n")?;
    let len = items.len() as i64;
    if index == 0 {
        return Err("$n: List index may not be 0.".to_string());
    }
    let index = if index < 0 { len + index } else { index - 1 };
    if index < 0 || index >= len {
        return Err(format!(
            "$n: Invalid index {index} for a list with {len} elements."
        ));
    }
    items[index as usize] = args.req(2)?.clone();
    Ok(Value::List(List {
        items,
        sep: match list {
            Value::List(l) => l.sep,
            _ => Sep::Space,
        },
        bracketed: matches!(list, Value::List(l) if l.bracketed),
    }))
}

// ---------------------------------------------------------------------------
// map
// ---------------------------------------------------------------------------

fn map_get(positional: &[Value], keywords: &[(String, Value)]) -> ValResult<Value> {
    let args = args!(&["map", "key"], positional, keywords);
    let map = value::as_map(args.req(0)?, "map")?;
    let key = args.req(1)?;
    Ok(map
        .iter()
        .find(|(k, _)| value::values_equal(k, key))
        .map(|(_, v)| v.clone())
        .unwrap_or(Value::Null))
}

fn map_has_key(positional: &[Value], keywords: &[(String, Value)]) -> ValResult<Value> {
    let args = args!(&["map", "key"], positional, keywords);
    let map = value::as_map(args.req(0)?, "map")?;
    let key = args.req(1)?;
    Ok(Value::Bool(
        map.iter().any(|(k, _)| value::values_equal(k, key)),
    ))
}

fn map_merge(positional: &[Value], keywords: &[(String, Value)]) -> ValResult<Value> {
    let args = args!(&["map1", "map2"], positional, keywords);
    let first = value::as_map(args.req(0)?, "map1")?;
    let second = value::as_map(args.req(1)?, "map2")?;
    let mut entries = first;
    for (key, value) in second {
        match entries
            .iter()
            .position(|(k, _)| value::values_equal(k, &key))
        {
            Some(i) => entries[i].1 = value,
            None => entries.push((key, value)),
        }
    }
    Ok(Value::Map(Map { entries }))
}

fn map_keys(positional: &[Value], keywords: &[(String, Value)]) -> ValResult<Value> {
    let args = args!(&["map"], positional, keywords);
    let map = value::as_map(args.req(0)?, "map")?;
    Ok(Value::List(List {
        items: map.into_iter().map(|(k, _)| k).collect(),
        sep: Sep::Comma,
        bracketed: false,
    }))
}

fn map_values(positional: &[Value], keywords: &[(String, Value)]) -> ValResult<Value> {
    let args = args!(&["map"], positional, keywords);
    let map = value::as_map(args.req(0)?, "map")?;
    Ok(Value::List(List {
        items: map.into_iter().map(|(_, v)| v).collect(),
        sep: Sep::Comma,
        bracketed: false,
    }))
}

fn map_remove(positional: &[Value], keywords: &[(String, Value)]) -> ValResult<Value> {
    // dart 的签名是 `map-remove($map, $keys...)`。
    if let Some((name, _)) = keywords.first() {
        return Err(format!("No argument named ${name}."));
    }
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

// ---------------------------------------------------------------------------
// meta
// ---------------------------------------------------------------------------

fn type_of(positional: &[Value], keywords: &[(String, Value)]) -> ValResult<Value> {
    let args = args!(&["value"], positional, keywords);
    let name = match args.req(0)? {
        Value::Number(_) => "number",
        Value::Color(_) => "color",
        Value::Str(_) => "string",
        Value::Bool(_) => "bool",
        Value::Null => "null",
        Value::List(_) => "list",
        Value::Map(_) => "map",
        Value::ArgList(_) => "arglist",
        // dart 把这些「写进 CSS 的函数」都当作字符串（`calc` / `min` / 未知函数）。
        Value::Function(_) => "string",
        Value::FuncRef(_) => "function",
        Value::Raw(_) => "string",
    };
    Ok(Value::Str(SassStr::unquoted(name)))
}

/// `feature-exists()`：1.35.2 只认这五个（`module` 返回 false，是兼容探针）。
fn feature_exists(positional: &[Value], keywords: &[(String, Value)]) -> ValResult<Value> {
    let args = args!(&["feature"], positional, keywords);
    let feature = &args.string(0)?.text;
    Ok(Value::Bool(matches!(
        feature.as_str(),
        "global-variable-shadowing"
            | "extend-selector-pseudoclass"
            | "units-level-3"
            | "at-error"
            | "custom-property"
    )))
}

fn unique_id(positional: &[Value], keywords: &[(String, Value)]) -> ValResult<Value> {
    let args = args!(&[], positional, keywords);
    let _ = args;
    Ok(Value::Str(SassStr::unquoted(super::modules::unique_id())))
}

fn is_bracketed(positional: &[Value], keywords: &[(String, Value)]) -> ValResult<Value> {
    let args = args!(&["list"], positional, keywords);
    Ok(Value::Bool(matches!(
        args.req(0)?,
        Value::List(list) if list.bracketed
    )))
}

fn keywords_map(positional: &[Value], keywords: &[(String, Value)]) -> ValResult<Value> {
    let args = args!(&["args"], positional, keywords);
    match args.req(0)? {
        Value::ArgList(list) => {
            list.keywords_accessed.set(true);
            Ok(Value::Map(Map {
                entries: list
                    .keywords
                    .iter()
                    .map(|(name, value)| {
                        (Value::Str(SassStr::unquoted(name.clone())), value.clone())
                    })
                    .collect(),
            }))
        }
        other => Err(format!(
            "$args: {} is not an argument list.",
            other.inspect()
        )),
    }
}

// ---------------------------------------------------------------------------
// 颜色
// ---------------------------------------------------------------------------

/// dart `SassString.isSpecialNumber` 的前缀集合：颜色函数的实参只要命中一个，
/// 整个调用就透传成纯 CSS（bootstrap 大量使用 `rgb(var(--x))`）。
const SPECIAL_NUMBERS: [&str; 6] = ["calc(", "clamp(", "var(", "env(", "max(", "min("];

fn is_special(value: &Value) -> bool {
    let text = match value {
        Value::Str(s) if !s.quoted => s.text.to_ascii_lowercase(),
        Value::Raw(text) => text.to_ascii_lowercase(),
        Value::Function(f) => format!("{}(", f.name.to_ascii_lowercase()),
        _ => return false,
    };
    SPECIAL_NUMBERS
        .iter()
        .any(|prefix| text.starts_with(prefix))
}

fn is_var(value: &Value) -> bool {
    match value {
        Value::Str(s) if !s.quoted => s.text.to_ascii_lowercase().starts_with("var("),
        Value::Raw(text) => text.to_ascii_lowercase().starts_with("var("),
        Value::Function(f) => f.name.eq_ignore_ascii_case("var"),
        _ => false,
    }
}

/// dart 的 `_functionString`：`name(a, b)`，实参按 inspect 形态用逗号加空格分开。
fn function_string(name: &str, items: &[Value]) -> Value {
    Value::Raw(format!(
        "{name}({})",
        items
            .iter()
            .map(Value::inspect)
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

struct ColorArgs {
    items: Vec<Value>,
    alpha: Option<Value>,
    /// 空格分隔写法（`rgb(0 0 0 / 50%)`）：最多只能 3 个元素。
    space: bool,
    /// 末项是带 `/` 的字符串（`0/var(--o)`）：整条调用透传成纯 CSS。
    slash_text: bool,
}

impl ColorArgs {
    /// `0 0 0 / 50%`：空格 list 的末项是「斜杠数」`0/50%`，右边就是 alpha。
    fn from_value(args: &Value) -> Self {
        match args {
            Value::List(list) if list.sep == Sep::Space => {
                let mut items = list.items.clone();
                let slash = match items.last() {
                    Some(Value::Number(n)) => n
                        .slash
                        .as_ref()
                        .map(|pair| (pair.0.clone(), pair.1.clone())),
                    _ => None,
                };
                match slash {
                    Some((left, alpha)) => {
                        items.pop();
                        items.push(left);
                        Self {
                            items,
                            alpha: Some(alpha),
                            space: true,
                            slash_text: false,
                        }
                    }
                    None => {
                        // `rgb(0 0 0 / var(--o))`：末项不是数字而是 `0/var(--o)` 这样的
                        // 字符串，dart 直接把整条调用当 CSS 输出。
                        let slash_text = matches!(
                            items.last(),
                            Some(Value::Str(s)) if !s.quoted && s.text.contains('/')
                        );
                        Self {
                            items,
                            alpha: None,
                            space: true,
                            slash_text,
                        }
                    }
                }
            }
            Value::List(list) if list.sep == Sep::Comma => Self {
                items: list.items.clone(),
                alpha: None,
                space: false,
                slash_text: false,
            },
            other => Self {
                items: vec![other.clone()],
                alpha: None,
                space: false,
                slash_text: false,
            },
        }
    }

    /// 通道个数校验：逗号写法最多 4 个实参，空格写法只能 3 个元素
    /// （alpha 必须跟在 `/` 后面），两者的文案不同（argument / element）。
    fn check_count(&self, names: [&str; 3]) -> ValResult<()> {
        let (max, kind) = if self.space {
            (3, "element")
        } else {
            (4, "argument")
        };
        if self.items.len() > max {
            return Err(format!(
                "Only {max} {kind}s allowed, but {} were passed.",
                self.items.len()
            ));
        }
        if self.items.len() < 3 {
            return Err(format!("Missing {kind} ${}.", names[self.items.len()]));
        }
        Ok(())
    }

    fn all(&self) -> impl Iterator<Item = &Value> {
        self.items.iter().chain(self.alpha.iter())
    }
}

/// 颜色函数的参数没有「函数签名」，dart 直接在 `_rgb` / `_hsl` 里读实参，
/// 所以这里绕过 `Args`（它要求固定形参表）。
fn color_call_args(positional: &[Value], keywords: &[(String, Value)]) -> ValResult<Value> {
    if keywords.is_empty() {
        return Ok(match positional {
            [] => Value::List(List {
                items: Vec::new(),
                sep: Sep::Comma,
                bracketed: false,
            }),
            [single] => single.clone(),
            many => Value::List(List {
                items: many.to_vec(),
                sep: Sep::Comma,
                bracketed: false,
            }),
        });
    }
    // 关键字实参：按 dart 的形参名拼回位置序列（`rgb($red: 1, $green: 2, $blue: 3)`）。
    const RGB: [&str; 4] = ["red", "green", "blue", "alpha"];
    const HSL: [&str; 4] = ["hue", "saturation", "lightness", "alpha"];
    let names = if keywords
        .iter()
        .any(|(name, _)| value::names_eq(name, "hue"))
    {
        HSL
    } else {
        RGB
    };
    let mut items = Vec::new();
    for (name, value) in keywords {
        let Some(index) = names.iter().position(|n| value::names_eq(name, n)) else {
            return Err(format!("No argument named ${name}."));
        };
        while items.len() <= index {
            items.push(Value::Null);
        }
        items[index] = value.clone();
    }
    Ok(Value::List(List {
        items,
        sep: Sep::Comma,
        bracketed: false,
    }))
}

fn rgb(name: &str, positional: &[Value], keywords: &[(String, Value)]) -> ValResult<Value> {
    let channels = color_call_args(positional, keywords)?;
    let args = ColorArgs::from_value(&channels);
    if args.slash_text {
        return Ok(function_string(name, &[channels]));
    }
    // `rgba($color, $alpha)` 两实参形态（dart 的 `_rgbTwoArg`）。
    if name == "rgba" && args.items.len() == 2 && args.alpha.is_none() && !args.space {
        return rgba_two_arg(name, &args.items);
    }
    // 特殊数字（`var()` / `calc()` / `env()` / `min()` …）整条透传为纯 CSS；
    // 这个判定在参数个数校验之前（dart 实测：`rgb(var(--x))` 不报缺参数）。
    if args.all().any(is_special) {
        return Ok(function_string(
            name,
            &args.all().cloned().collect::<Vec<_>>(),
        ));
    }
    args.check_count(["red", "green", "blue"])?;
    let channel = |index: usize, label: &str| -> ValResult<f64> {
        match &args.items[index] {
            Value::Number(num) if num.unit() == "%" => Ok(num.value * 255.0 / 100.0),
            Value::Number(num) => Ok(num.value),
            other => Err(format!("${label}: {} is not a number.", other.inspect())),
        }
    };
    let r = channel(0, "red")?;
    let g = channel(1, "green")?;
    let b = channel(2, "blue")?;
    let a = match args.alpha.as_ref().or(args.items.get(3)) {
        Some(Value::Number(num)) if num.unit() == "%" => num.value / 100.0,
        Some(Value::Number(num)) => num.value,
        Some(other) => return Err(format!("$alpha: {} is not a number.", other.inspect())),
        None => 1.0,
    };
    Ok(Value::Color(Color::new(r, g, b, a)))
}

fn rgba_two_arg(name: &str, items: &[Value]) -> ValResult<Value> {
    let (first, second) = (&items[0], &items[1]);
    let passthrough = || function_string(name, items);
    // `rgba($color, …)` 的通道按未取整值输出（dart：`rgba(253.04, 253.32, 253.6, …)`）。
    let channels = |color: &Color| {
        let (r, g, b) = color.channels_f64();
        format!(
            "{}, {}, {}",
            Number::format_value(r),
            Number::format_value(g),
            Number::format_value(b)
        )
    };
    if is_var(first) {
        return Ok(passthrough());
    }
    if is_var(second) {
        return Ok(match first {
            Value::Color(color) => Value::Raw(format!(
                "{name}({}, {})",
                channels(color),
                second.inspect()
            )),
            _ => passthrough(),
        });
    }
    if is_special(second) {
        let Value::Color(color) = first else {
            return Err(format!("$color: {} is not a color.", first.inspect()));
        };
        return Ok(Value::Raw(format!(
            "{name}({}, {})",
            channels(color),
            second.inspect()
        )));
    }
    let Value::Color(color) = first else {
        return Err(format!("$color: {} is not a color.", first.inspect()));
    };
    let alpha = match second {
        Value::Number(num) if num.unit() == "%" => num.value / 100.0,
        Value::Number(num) => num.value,
        other => return Err(format!("$alpha: {} is not a number.", other.inspect())),
    };
    Ok(Value::Color(Color {
        a: alpha,
        literal: None,
        ..color.clone()
    }))
}

/// `lab()` / `oklch()` / `color(display-p3 …)` / `hwb(…)`：通道按显示单位存，
/// `none` 映射成 missing（现代 CSS 语义）。
fn modern_color(
    name: &str,
    positional: &[Value],
    keywords: &[(String, Value)],
) -> ValResult<Value> {
    let args = color_call_args(positional, keywords)?;
    let args = ColorArgs::from_value(&args);
    // 实参里有 `var()` / `env()` / `min()` 之类：整条调用是纯 CSS，原样透传
    // （`color(var(--x))` 不是「在现代颜色空间 x 里构造颜色」）。
    if args.items.iter().any(is_special) {
        return Ok(function_string(name, &args.items));
    }
    let mut items = args.items.clone();
    let space = if name == "color" {
        let Some(first) = items.first() else {
            return Err("Missing argument $space.".to_string());
        };
        let text = match first {
            Value::Str(text) => text.text.clone(),
            other => other.inspect(),
        };
        let space = Space::from_name(&text)
            .ok_or_else(|| format!("$space: Unknown color space \"{text}\"."))?;
        items.remove(0);
        space
    } else {
        Space::from_name(name).expect("构造器名就是空间名")
    };
    let mut channels = [None; 3];
    for (index, slot) in channels.iter_mut().enumerate() {
        *slot = match items.get(index) {
            None => None,
            Some(Value::Str(text)) if !text.quoted && text.text.eq_ignore_ascii_case("none") => {
                None
            }
            Some(Value::Number(number)) => {
                // `oklab`/`oklch` 的 lightness 用 0..1 的无单位数（`50%` 才是 0..100）。
                Some(
                    if index == 0
                        && matches!(space, Space::Oklab | Space::Oklch)
                        && number.unit().is_empty()
                    {
                        number.value * 100.0
                    } else {
                        number.value
                    },
                )
            }
            Some(other) => {
                return Err(format!("$channels: {} is not a number.", other.inspect()));
            }
        };
    }
    let alpha = match args.alpha.as_ref().or(items.get(3)) {
        None => 1.0,
        Some(Value::Number(number)) if number.unit() == "%" => number.value / 100.0,
        Some(Value::Number(number)) => number.value,
        Some(other) => return Err(format!("$alpha: {} is not a number.", other.inspect())),
    };
    Ok(Value::Color(Color::from_space(space, channels, alpha)))
}

fn hsl(name: &str, positional: &[Value], keywords: &[(String, Value)]) -> ValResult<Value> {
    let channels = color_call_args(positional, keywords)?;
    let args = ColorArgs::from_value(&channels);
    if args.slash_text {
        return Ok(function_string(name, &[channels]));
    }
    if args.all().any(is_special) {
        return Ok(function_string(
            name,
            &args.all().cloned().collect::<Vec<_>>(),
        ));
    }
    args.check_count(["hue", "saturation", "lightness"])?;
    let Value::Number(hue) = &args.items[0] else {
        return Err(format!(
            "$hue: {} is not a number.",
            args.items[0].inspect()
        ));
    };
    let percent = |value: Option<&Value>, label: &str| -> ValResult<f64> {
        match value {
            None => Ok(1.0),
            Some(Value::Number(num)) if num.unit() == "%" => Ok(num.value / 100.0),
            Some(Value::Number(num)) => Ok(num.value / 100.0),
            Some(other) => Err(format!("${label}: {} is not a number.", other.inspect())),
        }
    };
    let s = percent(args.items.get(1), "saturation")?;
    let l = percent(args.items.get(2), "lightness")?;
    let a = match args.alpha.as_ref().or(args.items.get(3)) {
        None => 1.0,
        Some(Value::Number(num)) if num.unit() == "%" => num.value / 100.0,
        Some(Value::Number(num)) => num.value,
        Some(other) => return Err(format!("$alpha: {} is not a number.", other.inspect())),
    };
    let (r, g, b) = hsl_to_rgb(hue.value, s, l);
    let mut color = Color::new(r, g, b, a);
    color.space = Some(Space::Hsl);
    color.hsl = Some((
        hue.value.rem_euclid(360.0),
        (s * 100.0).clamp(0.0, 100.0),
        (l * 100.0).clamp(0.0, 100.0),
    ));
    Ok(Value::Color(color))
}

fn mix(positional: &[Value], keywords: &[(String, Value)]) -> ValResult<Value> {
    let args = args!(&["color1", "color2", "weight"], positional, keywords);
    let c1 = args.color(0)?;
    let c2 = args.color(1)?;
    let weight = match args.opt_number(2)? {
        Some(n) => n.value,
        None => 50.0,
    };
    if !(0.0..=100.0).contains(&weight) {
        return Err(format!(
            "$weight: Expected {} to be within 0 and 100.",
            Number::format_value(weight)
        ));
    }
    let weight_scale = weight / 100.0;
    let normalized = weight_scale * 2.0 - 1.0;
    let alpha_distance = c1.a - c2.a;
    let combined = normalized * alpha_distance;
    let weight1 = if (combined + 1.0).abs() < value::EPSILON {
        (normalized + 1.0) / 2.0
    } else {
        ((normalized + alpha_distance) / (1.0 + combined) + 1.0) / 2.0
    };
    let weight2 = 1.0 - weight1;
    let (r1, g1, b1) = c1.channels_f64();
    let (r2, g2, b2) = c2.channels_f64();
    Ok(Value::Color(Color::new(
        r1 * weight1 + r2 * weight2,
        g1 * weight1 + g2 * weight2,
        b1 * weight1 + b2 * weight2,
        c1.a * weight_scale + c2.a * (1.0 - weight_scale),
    )))
}

fn hsl_adjust(
    params: &'static [&'static str],
    positional: &[Value],
    keywords: &[(String, Value)],
    apply: impl Fn(&mut Hsl, f64),
    amount_name: &'static str,
) -> ValResult<Value> {
    let args = args!(params, positional, keywords);
    let color = args.color(0)?;
    let amount = args.number(1)?.value_in_range(0.0, 100.0, amount_name)?;
    let mut hsl = Hsl::from_color(color);
    apply(&mut hsl, amount);
    Ok(Value::Color(hsl.to_color(color.a)))
}

fn lighten(positional: &[Value], keywords: &[(String, Value)]) -> ValResult<Value> {
    hsl_adjust(
        &["color", "amount"],
        positional,
        keywords,
        |hsl, amount| hsl.l += amount,
        "amount",
    )
}

fn darken(positional: &[Value], keywords: &[(String, Value)]) -> ValResult<Value> {
    hsl_adjust(
        &["color", "amount"],
        positional,
        keywords,
        |hsl, amount| hsl.l -= amount,
        "amount",
    )
}

fn saturate(positional: &[Value], keywords: &[(String, Value)]) -> ValResult<Value> {
    // `saturate(50%)` 是 CSS filter：单实参且不是颜色时原样透传。
    if positional.len() == 1 && keywords.is_empty() && !matches!(positional[0], Value::Color(_)) {
        return Ok(function_string("saturate", positional));
    }
    hsl_adjust(
        &["color", "amount"],
        positional,
        keywords,
        |hsl, amount| hsl.s += amount,
        "amount",
    )
}

fn desaturate(positional: &[Value], keywords: &[(String, Value)]) -> ValResult<Value> {
    hsl_adjust(
        &["color", "amount"],
        positional,
        keywords,
        |hsl, amount| hsl.s -= amount,
        "amount",
    )
}

fn adjust_hue(positional: &[Value], keywords: &[(String, Value)]) -> ValResult<Value> {
    let args = args!(&["color", "degrees"], positional, keywords);
    let color = args.color(0)?;
    let degrees = args.number(1)?.angle_degrees("degrees")?;
    let mut hsl = Hsl::from_color(color);
    hsl.h += degrees;
    Ok(Value::Color(hsl.to_color(color.a)))
}

fn grayscale(positional: &[Value], keywords: &[(String, Value)]) -> ValResult<Value> {
    // `grayscale(50%)` 是 CSS filter。
    if positional.len() == 1 && keywords.is_empty() && !matches!(positional[0], Value::Color(_)) {
        return Ok(function_string("grayscale", positional));
    }
    let args = args!(&["color"], positional, keywords);
    let mut hsl = Hsl::from_color(args.color(0)?);
    hsl.s = 0.0;
    Ok(Value::Color(hsl.to_color(args.color(0)?.a)))
}

fn complement(positional: &[Value], keywords: &[(String, Value)]) -> ValResult<Value> {
    let args = args!(&["color"], positional, keywords);
    let color = args.color(0)?;
    let mut hsl = Hsl::from_color(color);
    hsl.h += 180.0;
    Ok(Value::Color(hsl.to_color(color.a)))
}

fn invert(positional: &[Value], keywords: &[(String, Value)]) -> ValResult<Value> {
    let args = args!(&["color", "weight"], positional, keywords);
    let Some(Value::Color(color)) = args.at(0) else {
        // `invert(0.5)` 之类是 CSS filter：原样透传。
        return Ok(function_string(
            "invert",
            &positional[..positional.len().min(1)],
        ));
    };
    let (r, g, b) = color.channels_f64();
    let inverse = Color::new(255.0 - r, 255.0 - g, 255.0 - b, color.a);
    let weight = match args.opt_number(1)? {
        Some(n) => n.value,
        None => 100.0,
    };
    if (weight - 100.0).abs() < value::EPSILON {
        return Ok(Value::Color(inverse));
    }
    let weight_scale = weight / 100.0;
    let (ir, ig, ib) = inverse.channels_f64();
    Ok(Value::Color(Color::new(
        ir * weight_scale + r * (1.0 - weight_scale),
        ig * weight_scale + g * (1.0 - weight_scale),
        ib * weight_scale + b * (1.0 - weight_scale),
        color.a,
    )))
}

fn opacify(positional: &[Value], keywords: &[(String, Value)]) -> ValResult<Value> {
    let args = args!(&["color", "amount"], positional, keywords);
    let color = args.color(0)?;
    let amount = args.number(1)?.value_in_range(0.0, 1.0, "amount")?;
    Ok(Value::Color(Color {
        a: (color.a + amount).clamp(0.0, 1.0),
        literal: None,
        ..color.clone()
    }))
}

fn transparentize(positional: &[Value], keywords: &[(String, Value)]) -> ValResult<Value> {
    let args = args!(&["color", "amount"], positional, keywords);
    let color = args.color(0)?;
    let amount = args.number(1)?.value_in_range(0.0, 1.0, "amount")?;
    Ok(Value::Color(Color {
        a: (color.a - amount).clamp(0.0, 1.0),
        literal: None,
        ..color.clone()
    }))
}

/// `adjust-color` / `change-color` / `scale-color`：只有 `$color` 可以按位置传。
#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Adjust,
    Change,
    Scale,
}

fn adjust_color(
    positional: &[Value],
    keywords: &[(String, Value)],
    mode: Mode,
) -> ValResult<Value> {
    let name = match mode {
        Mode::Adjust => "adjust-color",
        Mode::Change => "change-color",
        Mode::Scale => "scale-color",
    };
    if positional.len() > 1 {
        return Err(
            "Only one positional argument is allowed. All other arguments must be passed by name."
                .to_string(),
        );
    }
    let color = match positional.first() {
        Some(Value::Color(color)) => color.clone(),
        Some(other) => return Err(format!("$color: {} is not a color.", other.inspect())),
        None => return Err("Missing argument $color.".to_string()),
    };
    let mut rgbs: [Option<f64>; 3] = [None; 3];
    let mut hsls: [Option<f64>; 3] = [None; 3];
    let mut hwbs: [Option<f64>; 2] = [None; 2];
    let mut alpha: Option<f64> = None;
    for (key, value) in keywords {
        if value::names_eq(key, "alpha") {
            let number = value.assert_number("alpha")?;
            alpha = Some(match mode {
                Mode::Scale => number.value_in_range(-100.0, 100.0, "alpha")? / 100.0,
                Mode::Adjust => number.value_in_range(-1.0, 1.0, "alpha")?,
                Mode::Change => number.value_in_range(0.0, 1.0, "alpha")?,
            });
            continue;
        }
        let slot = if value::names_eq(key, "red") {
            &mut rgbs[0]
        } else if value::names_eq(key, "green") {
            &mut rgbs[1]
        } else if value::names_eq(key, "blue") {
            &mut rgbs[2]
        } else if value::names_eq(key, "hue") {
            &mut hsls[0]
        } else if value::names_eq(key, "saturation") {
            &mut hsls[1]
        } else if value::names_eq(key, "lightness") {
            &mut hsls[2]
        } else if value::names_eq(key, "whiteness") {
            &mut hwbs[0]
        } else if value::names_eq(key, "blackness") {
            &mut hwbs[1]
        } else {
            return Err(format!("No argument named ${key}."));
        };
        *slot = Some(component(key, value, mode)?);
    }
    let (rgb_set, hsl_set, hwb_set) = (
        rgbs.iter().any(Option::is_some),
        hsls.iter().any(Option::is_some),
        hwbs.iter().any(Option::is_some),
    );
    if rgb_set && hsl_set {
        return Err("RGB parameters may not be passed along with HSL parameters.".to_string());
    }
    if rgb_set && hwb_set {
        return Err("RGB parameters may not be passed along with HWB parameters.".to_string());
    }
    if hsl_set && hwb_set {
        return Err("HSL parameters may not be passed along with HWB parameters.".to_string());
    }
    let _ = name;
    let (r, g, b) = color.channels_f64();
    let base = [r, g, b];
    let channels = if rgbs.iter().any(Option::is_some) {
        Some(
            rgbs.iter()
                .enumerate()
                .map(|(i, delta)| match (mode, delta) {
                    (Mode::Change, Some(value)) => *value,
                    (Mode::Adjust, Some(delta)) => base[i] + delta,
                    (Mode::Scale, Some(scale)) => scale_channel(base[i], *scale, 255.0, 0.0),
                    _ => base[i],
                })
                .collect::<Vec<_>>(),
        )
    } else if hsls.iter().any(Option::is_some) {
        let mut hsl = Hsl::from_color(&color);
        let cur = [hsl.h, hsl.s, hsl.l];
        for (i, delta) in hsls.iter().enumerate() {
            if let Some(delta) = delta {
                match mode {
                    Mode::Change => {
                        if i == 0 {
                            hsl.h = *delta;
                        } else if i == 1 {
                            hsl.s = *delta;
                        } else {
                            hsl.l = *delta;
                        }
                    }
                    Mode::Adjust => {
                        if i == 0 {
                            hsl.h += *delta;
                        } else if i == 1 {
                            hsl.s += *delta;
                        } else {
                            hsl.l += *delta;
                        }
                    }
                    Mode::Scale => {
                        let scaled = scale_channel(cur[i], *delta, 100.0, 0.0);
                        if i == 0 {
                            hsl.h = scaled;
                        } else if i == 1 {
                            hsl.s = scaled;
                        } else {
                            hsl.l = scaled;
                        }
                    }
                }
            }
        }
        let scaled = hsl.to_color(color.a);
        let (r, g, b) = scaled.channels_f64();
        Some(vec![r, g, b])
    } else {
        None
    };
    let channels = channels.or_else(|| {
        hwb_set.then(|| {
            let mut hwb = Hwb::from_color(&color);
            for (i, delta) in hwbs.iter().enumerate() {
                let Some(delta) = delta else { continue };
                let current = if i == 0 { hwb.w } else { hwb.b };
                let next = match mode {
                    Mode::Change => *delta,
                    Mode::Adjust => current + delta,
                    Mode::Scale => scale_channel(current, *delta, 100.0, 0.0),
                };
                if i == 0 {
                    hwb.w = next;
                } else {
                    hwb.b = next;
                }
            }
            let (r, g, b) = hwb.to_color(color.a).channels_f64();
            vec![r, g, b]
        })
    });
    let (r, g, b) = match channels {
        Some(channels) => (channels[0], channels[1], channels[2]),
        None => (r, g, b),
    };
    let a = match alpha {
        Some(value) => match mode {
            Mode::Change => value,
            Mode::Scale => scale_channel(color.a, value, 1.0, 0.0),
            Mode::Adjust => (color.a + value).clamp(0.0, 1.0),
        },
        None => color.a,
    };
    Ok(Value::Color(Color::new(r, g, b, a)))
}

/// `adjust-color` 的增量语义：`$hue` 是角度，其余是裸数字；`scale-color` 用百分比。
fn component(key: &str, value: &Value, mode: Mode) -> ValResult<f64> {
    let number = value.assert_number(key)?;
    Ok(match mode {
        Mode::Scale => {
            let percent = if number.unit() == "%" {
                number.value
            } else {
                return Err(format!(
                    "${key}: Expected {} to have unit \"%\".",
                    number.format()
                ));
            };
            percent / 100.0
        }
        Mode::Change => number.value,
        Mode::Adjust => {
            // `$hue` 可以带角度单位；saturation/lightness 是百分数但 dart 只取数值。
            if value::names_eq(key, "hue") {
                number.angle_degrees(key)?
            } else {
                number.value
            }
        }
    })
}

fn scale_channel(current: f64, scale: f64, max: f64, min: f64) -> f64 {
    if scale > 0.0 {
        current + (max - current) * scale
    } else {
        current + (current - min) * scale
    }
}

/// HWB 三分量（`h` 度数，`w`/`b` 百分比）。
struct Hwb {
    h: f64,
    w: f64,
    b: f64,
}

impl Hwb {
    fn from_color(color: &Color) -> Self {
        let (r, g, b) = color.channels_f64();
        let (r, g, b) = (r / 255.0, g / 255.0, b / 255.0);
        Self {
            h: Hsl::from_color(color).h,
            w: r.min(g).min(b) * 100.0,
            b: (1.0 - r.max(g).max(b)) * 100.0,
        }
    }

    /// `w + b > 100%` 时按比例归一化（CSS HWB 的规则，dart 同样处理）。
    fn to_color(&self, alpha: f64) -> Color {
        let (mut w, mut b) = (self.w / 100.0, self.b / 100.0);
        if w + b > 1.0 {
            let sum = w + b;
            w /= sum;
            b /= sum;
        }
        let pure = Hsl {
            h: self.h,
            s: 100.0,
            l: 50.0,
        }
        .to_color(1.0);
        let (r, g, blue) = pure.channels_f64();
        let scale = 1.0 - w - b;
        Color::new(
            (r / 255.0 * scale + w) * 255.0,
            (g / 255.0 * scale + w) * 255.0,
            (blue / 255.0 * scale + w) * 255.0,
            alpha,
        )
    }
}

enum Channel {
    Red,
    Green,
    Blue,
    Hue,
    Saturation,
    Lightness,
}

fn channel(positional: &[Value], keywords: &[(String, Value)], which: Channel) -> ValResult<Value> {
    let args = args!(&["color"], positional, keywords);
    let color = args.color(0)?;
    let value = match which {
        Channel::Red | Channel::Green | Channel::Blue => {
            let (r, g, b) = color.channels();
            match which {
                Channel::Red => r as f64,
                Channel::Green => g as f64,
                _ => b as f64,
            }
        }
        _ => {
            let hsl = Hsl::from_color(color);
            match which {
                Channel::Hue => hsl.h,
                Channel::Saturation => hsl.s,
                _ => hsl.l,
            }
        }
    };
    match which {
        Channel::Hue => Ok(Value::Number(Number::with_unit(value, "deg"))),
        Channel::Saturation | Channel::Lightness => {
            Ok(Value::Number(Number::with_unit(value, "%")))
        }
        _ => Ok(Value::Number(Number::unitless(value))),
    }
}

fn alpha(positional: &[Value], keywords: &[(String, Value)]) -> ValResult<Value> {
    // `alpha(opacity=50)` / `opacity(0.5)` 是 CSS：非颜色时原样透传。
    if positional.len() == 1 && keywords.is_empty() && !matches!(positional[0], Value::Color(_)) {
        return Ok(function_string("alpha", positional));
    }
    let args = args!(&["color"], positional, keywords);
    Ok(Value::Number(Number::unitless(args.color(0)?.a)))
}

fn ie_hex_str(positional: &[Value], keywords: &[(String, Value)]) -> ValResult<Value> {
    let args = args!(&["color"], positional, keywords);
    let color = args.color(0)?;
    let (r, g, b) = color.channels();
    let a = (color.a * 255.0).round().clamp(0.0, 255.0) as u8;
    Ok(Value::Str(SassStr::unquoted(format!(
        "#{a:02X}{r:02X}{g:02X}{b:02X}"
    ))))
}

/// HSL 表示：色相是度，饱和度/亮度是 **0..100 的百分数**（与 dart 的 `SassColor` 一致）。
struct Hsl {
    h: f64,
    s: f64,
    l: f64,
}

impl Hsl {
    fn from_color(color: &Color) -> Self {
        // HSL 运算产出的颜色保留原始分量：`darken(darken(#fff, 3%), 5%)` 里第二
        // 步用的是 97% 而不是从 #f7f7f7 反推的 96.86%（dart 实测）。
        if let Some((h, s, l)) = color.hsl {
            return Self { h, s, l };
        }
        let (r, g, b) = color.channels_f64();
        let (h, s, l) = rgb_to_hsl(r / 255.0, g / 255.0, b / 255.0);
        Self {
            h,
            s: s * 100.0,
            l: l * 100.0,
        }
    }

    fn to_color(&self, alpha: f64) -> Color {
        let s = self.s.clamp(0.0, 100.0);
        let l = self.l.clamp(0.0, 100.0);
        let (r, g, b) = hsl_to_rgb(self.h, s / 100.0, l / 100.0);
        let mut color = Color::new(r, g, b, alpha);
        color.hsl = Some((self.h.rem_euclid(360.0), s, l));
        color
    }
}

/// HSL → RGB（`h` 是度，`s`/`l` 是 0..1），与 dart 的 `SassColor.hsl` 同款公式。
fn hsl_to_rgb(hue: f64, s: f64, l: f64) -> (f64, f64, f64) {
    let h = hue.rem_euclid(360.0) / 360.0;
    let s = s.clamp(0.0, 1.0);
    let l = l.clamp(0.0, 1.0);
    let channel1 = if l <= 0.5 {
        l * (s + 1.0)
    } else {
        l + s - l * s
    };
    let channel2 = l * 2.0 - channel1;
    let channel = |hue: f64| {
        let hue = hue.rem_euclid(1.0);
        if hue < 1.0 / 6.0 {
            channel2 + (channel1 - channel2) * hue * 6.0
        } else if hue < 0.5 {
            channel1
        } else if hue < 2.0 / 3.0 {
            channel2 + (channel1 - channel2) * (2.0 / 3.0 - hue) * 6.0
        } else {
            channel2
        }
    };
    (
        channel(h + 1.0 / 3.0) * 255.0,
        channel(h) * 255.0,
        channel(h - 1.0 / 3.0) * 255.0,
    )
}

/// RGB（0..1）→ HSL，`h` 是度，`s`/`l` 是 0..1。公式逐行对齐 dart 的
/// `SassColor.rgbToHsl`（用 `==` 而不是容差比：近白颜色对尾数极敏感）。
fn rgb_to_hsl(r: f64, g: f64, b: f64) -> (f64, f64, f64) {
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let delta = max - min;
    let l = (max + min) / 2.0;
    if delta == 0.0 {
        return (0.0, 0.0, l);
    }
    let s = if l > 0.5 {
        delta / (2.0 - max - min)
    } else {
        delta / (max + min)
    };
    let h = if r == max {
        (g - b) / delta + if g < b { 6.0 } else { 0.0 }
    } else if g == max {
        (b - r) / delta + 2.0
    } else {
        (r - g) / delta + 4.0
    };
    (h * 60.0, s, l)
}
