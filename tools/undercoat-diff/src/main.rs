//! CSS 语义比较器：判断两份 CSS 在「层叠语义」上是否等价。
//!
//! 用法：
//!   undercoat-diff <a.css> <b.css> [--show N] [--json]
//!
//! 等价 exit 0；不等价 exit 1 并打印前 N 条差异；读文件/解析错误 exit 2。
//!
//! 判等口径（严格优先，只做**可证明**等价的归一化，逐条注明理由）：
//!
//! * 注释：`/* */` 在 CSS 里等价于一个空白分隔符，跳过；`/*! */` 是许可证注释，
//!   会出现在产物里，按出现顺序单独比较。
//! * 规则：按输出顺序比较。顺序影响层叠（同优先级后写覆盖先写），不可忽略。
//! * 选择器：
//!   - 空白游程归一为单空格并去首尾（选择器里的空白只是分隔符）；
//!   - `::before` ≡ `:before`：按 CSS 规范，`before/after/first-line/first-letter`
//!     四个伪元素的单冒号写法是双冒号写法的等价旧式拼写；
//!   - `[type="a"]` ≡ `[type=a]`：属性值本身是合法标识符时，引号只是转义形式，
//!     两种写法的选择器完全等价（保留引号的写法不做反向添加）；
//!   - **不**做大小写折叠：class/id/属性值大小写敏感，折叠会掩盖真实错误。
//! * 声明：
//!   - 属性名折叠为小写（CSS 属性名 ASCII 大小写不敏感），但 `--custom` 自定义
//!     属性例外——其名字大小写**敏感**，折叠会掩盖真实错误；
//!   - 值：空白游程归一、`,` `(` `)` 周围空白归一、数字前导零补齐
//!     （`.5` ≡ `0.5`，同一个数值 token）、`! important` ≡ `!important`
//!     （关键字 ASCII 大小写不敏感）；字符串与 `url(...)` 字面量原样保留；
//!   - **不**做颜色/简写/longhand 归一（例如 `#052c65` 与 `rgb(5,44,101)` 判为不同）。
//! * at-rule：名字折叠小写；prelude 按选择器口径归一（空白/`,`/`()`/前导零）；
//!   有块递归比较块内语句，无块按顺序比较。`@charset`/`@import` 天然顺序敏感。
//!
//! 差异分类见 [`CATEGORIES`]。报告里每条差异都带规则序号与选择器，便于定位。

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fmt::Write as _;
use std::path::PathBuf;

use oxc_css_parser::ast::{Statement, Stylesheet};
use oxc_css_parser::{Allocator, ParserBuilder, Span, Syntax};

/// 报告里可能出现的差异类别（顺序即打印顺序）。
const CATEGORIES: &[&str] = &[
    "parse-error",
    "license-comment",
    "rule-order",
    "missing-rule",
    "extra-rule",
    "selector-mismatch",
    "at-rule-order",
    "at-rule-missing",
    "at-rule-extra",
    "at-rule-block",
    "decl-order",
    "decl-missing",
    "decl-value",
];

// ---------------------------------------------------------------------------
// 模型
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Decl {
    name: String,
    value: String,
}

#[derive(Debug)]
enum Node {
    Rule {
        index: usize,
        selector: String,
        body: Vec<Node>,
    },
    AtRule {
        index: usize,
        head: String,
        body: Option<Vec<Node>>,
    },
    Decl(Decl),
    /// 兜底：CSS 产物里不该出现的语句，按归一化原文比较。
    Raw(String),
}

impl Node {
    /// 用于顺序对齐的键：选择器 / at-rule 头 / 原文。
    fn key(&self) -> String {
        match self {
            Node::Rule { selector, .. } => format!("R\u{1}{selector}"),
            Node::AtRule { head, .. } => format!("A\u{1}{head}"),
            Node::Decl(d) => format!("D\u{1}{}", d.name),
            Node::Raw(s) => format!("X\u{1}{s}"),
        }
    }

    fn index(&self) -> Option<usize> {
        match self {
            Node::Rule { index, .. } | Node::AtRule { index, .. } => Some(*index),
            _ => None,
        }
    }
}

struct Model {
    nodes: Vec<Node>,
    /// `/*! ... */` 许可证注释，按出现顺序。
    licenses: Vec<String>,
}

// ---------------------------------------------------------------------------
// 源文本清洗：剥注释 + 收集许可证注释
// ---------------------------------------------------------------------------

fn clean_source(src: &str) -> (String, Vec<String>) {
    let mut out = String::with_capacity(src.len());
    let mut licenses = Vec::new();
    let mut i = 0;
    while i < src.len() {
        let rest = &src[i..];
        let c = rest.chars().next().expect("non-empty slice");
        if c == '"' || c == '\'' {
            out.push(c);
            i += c.len_utf8();
            let mut escaped = false;
            while i < src.len() {
                let d = src[i..].chars().next().expect("non-empty slice");
                out.push(d);
                i += d.len_utf8();
                if escaped {
                    escaped = false;
                } else if d == '\\' {
                    escaped = true;
                } else if d == c {
                    break;
                }
            }
            continue;
        }
        if c == '/' && rest.starts_with("/*") {
            let start = i;
            let mut end = src.len();
            let mut j = i + 2;
            while j + 1 < src.len() {
                if src.as_bytes()[j] == b'*' && src.as_bytes()[j + 1] == b'/' {
                    end = j + 2;
                    break;
                }
                j += 1;
            }
            let body = &src[start..end];
            if body.starts_with("/*!") {
                licenses.push(body.to_string());
            }
            // 注释在 CSS 里等价于一个空白：替换成空格而不是删除，避免把两个 token
            // 粘连成第三个 token（`a/**/b` 是两个 ident，不是 `ab`）。
            if !out.is_empty() && !out.ends_with(' ') {
                out.push(' ');
            }
            i = end;
            continue;
        }
        out.push(c);
        i += c.len_utf8();
    }
    (out, licenses)
}

// ---------------------------------------------------------------------------
// 归一化
// ---------------------------------------------------------------------------

/// 空白游程折叠为单空格；字符串原样保留；`,` 两侧、`(` 之后、`)` 之前的空白
/// 去掉。这些都是纯分隔空白，不影响 token 序列：
///   - `transparent, red` 与 `transparent,red` 是同一个逗号分隔列表；
///   - `rgba(0, 0, 0, .5)` 与 `rgba(0,0,0,.5)` 是同一个函数调用。
///
/// 注意**不**去掉 `(` 之前和 `)` 之后的空白：在 `:not(a) .b` 里 `)` 后的空格
/// 是后代组合符，去掉会改变选择器语义。
fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    let mut pending_space = false;
    let mut suppress_space = false;
    let trim_spaces = |out: &mut String| {
        while out.ends_with(' ') {
            out.pop();
        }
    };
    while let Some(c) = it.next() {
        if c == '"' || c == '\'' {
            if pending_space && !out.is_empty() {
                out.push(' ');
            }
            pending_space = false;
            suppress_space = false;
            out.push(c);
            let mut escaped = false;
            for d in it.by_ref() {
                out.push(d);
                if escaped {
                    escaped = false;
                } else if d == '\\' {
                    escaped = true;
                } else if d == c {
                    break;
                }
            }
            continue;
        }
        if c.is_whitespace() {
            if !suppress_space {
                pending_space = true;
            }
            continue;
        }
        match c {
            ',' => {
                trim_spaces(&mut out);
                out.push(',');
                pending_space = false;
                suppress_space = true;
            }
            '(' => {
                if pending_space && !out.is_empty() {
                    out.push(' ');
                }
                pending_space = false;
                suppress_space = true;
                out.push('(');
            }
            ')' => {
                trim_spaces(&mut out);
                out.push(')');
                pending_space = false;
                suppress_space = false;
            }
            _ => {
                if pending_space && !out.is_empty() {
                    out.push(' ');
                }
                pending_space = false;
                suppress_space = false;
                out.push(c);
            }
        }
    }
    out
}

/// 数字前导零：`.5` 与 `0.5` 是同一个 `<number>` token，补零不改变值。
/// 只在小数点位于 token 起始位置（前一个字符是分隔符）时补，`1.5` 不动。
fn add_leading_zeros(s: &str) -> String {
    let cs: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len() + 4);
    let mut i = 0;
    while i < cs.len() {
        let c = cs[i];
        if c == '"' || c == '\'' {
            out.push(c);
            i += 1;
            let mut escaped = false;
            while i < cs.len() {
                let d = cs[i];
                out.push(d);
                i += 1;
                if escaped {
                    escaped = false;
                } else if d == '\\' {
                    escaped = true;
                } else if d == c {
                    break;
                }
            }
            continue;
        }
        if c == '.' && i + 1 < cs.len() && cs[i + 1].is_ascii_digit() {
            let boundary = out.chars().last().is_none_or(|p| {
                p.is_whitespace()
                    || matches!(
                        p,
                        '(' | ','
                            | ':'
                            | '/'
                            | '*'
                            | '+'
                            | '-'
                            | '!'
                            | '='
                            | '>'
                            | '<'
                            | '~'
                            | '|'
                            | '^'
                            | '$'
                    )
            });
            if boundary {
                out.push('0');
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

/// `::before` → `:before`（仅规范列出的四个旧式等价伪元素）。
fn strip_legacy_pseudo_elements(s: &str) -> String {
    const LEGACY: &[&str] = &["before", "after", "first-line", "first-letter"];
    let cs: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    let is_name_char = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_';
    while i < cs.len() {
        let c = cs[i];
        if c == '"' || c == '\'' {
            out.push(c);
            i += 1;
            let mut escaped = false;
            while i < cs.len() {
                let d = cs[i];
                out.push(d);
                i += 1;
                if escaped {
                    escaped = false;
                } else if d == '\\' {
                    escaped = true;
                } else if d == c {
                    break;
                }
            }
            continue;
        }
        if c == ':' && i + 2 < cs.len() && cs[i + 1] == ':' {
            let start = i + 2;
            let mut end = start;
            while end < cs.len() && is_name_char(cs[end]) {
                end += 1;
            }
            let name: String = cs[start..end].iter().collect();
            if LEGACY.contains(&name.to_ascii_lowercase().as_str()) {
                out.push(':');
                out.push_str(&name);
                i = end;
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

/// 属性选择器里「合法标识符」的引号可省：`[type="a"]` ≡ `[type=a]`。
/// 只处理最保守的 `-?[A-Za-z_][A-Za-z0-9_-]*`（含 `--x`），带转义/非 ASCII
/// 的保持原样——不拆只是少归一化，不会把不等价判成等价。
fn unquote_attribute_values(s: &str) -> String {
    fn is_ident(s: &str) -> bool {
        let b = s.as_bytes();
        if b.is_empty() {
            return false;
        }
        let mut i = 0;
        if b[0] == b'-' {
            if b.len() == 1 {
                return false;
            }
            if b[1] == b'-' {
                i = 2;
                if i == b.len() {
                    return false;
                }
            } else {
                i = 1;
            }
        }
        let first = b[i];
        if !(first.is_ascii_alphabetic() || first == b'_') {
            return false;
        }
        b[i + 1..]
            .iter()
            .all(|c| c.is_ascii_alphanumeric() || *c == b'-' || *c == b'_')
    }

    let cs: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < cs.len() {
        let c = cs[i];
        if c == '"' || c == '\'' {
            out.push(c);
            i += 1;
            let mut escaped = false;
            while i < cs.len() {
                let d = cs[i];
                out.push(d);
                i += 1;
                if escaped {
                    escaped = false;
                } else if d == '\\' {
                    escaped = true;
                } else if d == c {
                    break;
                }
            }
            continue;
        }
        if c == '=' {
            // `=` 在 CSS 选择器里只出现在属性选择器（`=`, `~=`, `|=`, `^=`, `$=`, `*=`）。
            let mut j = i + 1;
            while j < cs.len() && cs[j].is_whitespace() {
                j += 1;
            }
            if j < cs.len() && (cs[j] == '"' || cs[j] == '\'') {
                let quote = cs[j];
                let mut k = j + 1;
                let mut escaped = false;
                while k < cs.len() {
                    let d = cs[k];
                    if escaped {
                        escaped = false;
                    } else if d == '\\' {
                        escaped = true;
                    } else if d == quote {
                        break;
                    }
                    k += 1;
                }
                let content: String = cs[j + 1..k.min(cs.len())].iter().collect();
                if k < cs.len() && !content.contains('\\') && is_ident(&content) {
                    out.push('=');
                    out.push_str(&content);
                    i = k + 1;
                    continue;
                }
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

/// `! important` / `!IMPORTANT` 都等价于 `!important`（关键字大小写不敏感）。
fn normalize_important(s: &str) -> String {
    let t = s.trim_end();
    if let Some(pos) = t.rfind('!') {
        if t[pos + 1..].trim().eq_ignore_ascii_case("important") {
            let mut out = t[..pos].trim_end().to_string();
            out.push_str("!important");
            return out;
        }
    }
    t.to_string()
}

/// 字符串里的 CSS 转义解码为码点：`"\e644"` 与 `"\u{e644}"`（字面字符）
/// 是**同一个字符串值**——转义只是文本表示，不改变 `content` 等属性的值。
/// 只处理字符串内部；标识符里的转义（`.a\.b`）不解码——那是选择器语法的一部分。
fn decode_string_escapes(s: &str) -> String {
    let cs: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < cs.len() {
        let c = cs[i];
        if c != '"' && c != '\'' {
            out.push(c);
            i += 1;
            continue;
        }
        let quote = c;
        out.push(quote);
        i += 1;
        while i < cs.len() {
            let d = cs[i];
            if d == quote {
                out.push(d);
                i += 1;
                break;
            }
            if d != '\\' {
                out.push(d);
                i += 1;
                continue;
            }
            // 反斜杠：转义序列
            let esc_start = i;
            i += 1;
            if i >= cs.len() {
                out.push('\\');
                break;
            }
            let e = cs[i];
            if e == '\n' || e == '\r' || e == '\u{c}' {
                // 行继续：换行被吃掉
                i += 1;
                if e == '\r' && i < cs.len() && cs[i] == '\n' {
                    i += 1;
                }
                continue;
            }
            if e.is_ascii_hexdigit() {
                let mut value: u32 = 0;
                let mut digits = 0;
                while i < cs.len() && digits < 6 && cs[i].is_ascii_hexdigit() {
                    value = value * 16 + cs[i].to_digit(16).expect("hex digit");
                    i += 1;
                    digits += 1;
                }
                // 十六进制转义后允许（且只允许）一个空白作为终止符
                if i < cs.len() && cs[i].is_whitespace() {
                    i += 1;
                }
                match char::from_u32(value) {
                    Some(ch) if value != 0 => out.push(ch),
                    // 替位/非法码点：保留原转义文本，不做猜测
                    _ => out.extend(cs[esc_start..i].iter()),
                }
                continue;
            }
            out.push(e);
            i += 1;
        }
    }
    out
}

fn norm_selector(s: &str) -> String {
    decode_string_escapes(&strip_legacy_pseudo_elements(&unquote_attribute_values(
        &add_leading_zeros(&collapse_ws(s)),
    )))
}

/// 数学函数 `calc()/min()/max()/clamp()` 函数体内，`*` 与 `/` 两侧的空白可省：
/// CSS 数学函数的语法是 `<calc-value> [ [ '*' | '/' ] <calc-value> ]*`，空白不是
/// token 的一部分，`calc(100% / 2)` 与 `calc(100%/2)` 是同一个表达式。
/// `+` `-` **不**处理：它们必须由空白包围，去掉会把合法表达式变成非法。
fn tighten_calc_ops(s: &str) -> String {
    const FAMILY: &[&str] = &["calc", "min", "max", "clamp", "-webkit-calc"];
    let cs: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < cs.len() {
        if cs[i].is_ascii_alphabetic() || cs[i] == '-' {
            let start = i;
            let mut j = i;
            while j < cs.len() && (cs[j].is_ascii_alphanumeric() || cs[j] == '-' || cs[j] == '_') {
                j += 1;
            }
            let name: String = cs[start..j].iter().collect();
            if j < cs.len() && cs[j] == '(' && FAMILY.contains(&name.to_ascii_lowercase().as_str())
            {
                out.extend(cs[start..=j].iter());
                let mut depth = 1usize;
                let mut k = j + 1;
                while k < cs.len() && depth > 0 {
                    let c = cs[k];
                    if c == '(' {
                        depth += 1;
                    } else if c == ')' {
                        depth -= 1;
                        if depth == 0 {
                            out.push(')');
                            k += 1;
                            break;
                        }
                    }
                    if c.is_whitespace() {
                        let prev = out.chars().last();
                        let mut m = k + 1;
                        while m < cs.len() && cs[m].is_whitespace() {
                            m += 1;
                        }
                        let next = cs.get(m).copied();
                        if matches!(prev, Some('*') | Some('/'))
                            || matches!(next, Some('*') | Some('/'))
                        {
                            k = m;
                            continue;
                        }
                        out.push(' ');
                        k += 1;
                        continue;
                    }
                    out.push(c);
                    k += 1;
                }
                i = k;
                continue;
            }
            out.extend(cs[start..j].iter());
            i = j;
            continue;
        }
        out.push(cs[i]);
        i += 1;
    }
    out
}

/// at-rule prelude 里的 `:` 只出现在 `名字:值`（media feature、supports 声明）
/// 和 `@page :first` 这类伪页选择符里，两侧空白不影响解析，等价。
/// 选择器不能这样做：`a :hover` 里的空格是后代组合符。
fn tighten_prelude_colons(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c == '"' || c == '\'' {
            out.push(c);
            let mut escaped = false;
            for d in it.by_ref() {
                out.push(d);
                if escaped {
                    escaped = false;
                } else if d == '\\' {
                    escaped = true;
                } else if d == c {
                    break;
                }
            }
            continue;
        }
        if c == ':' {
            while out.ends_with(' ') {
                out.pop();
            }
            out.push(':');
            // 吃掉紧跟的空白
            while it.clone().next().is_some_and(|d| d.is_whitespace()) {
                it.next();
            }
            continue;
        }
        out.push(c);
    }
    out
}

fn norm_prelude(s: &str) -> String {
    decode_string_escapes(&tighten_prelude_colons(&add_leading_zeros(&collapse_ws(s))))
}

fn norm_value(s: &str) -> String {
    decode_string_escapes(&normalize_important(&tighten_calc_ops(&add_leading_zeros(
        &collapse_ws(s),
    ))))
}

// ---------------------------------------------------------------------------
// AST → 模型
// ---------------------------------------------------------------------------

fn slice<'a>(src: &'a str, span: &Span) -> &'a str {
    let start = span.start.min(src.len());
    let end = span.end.min(src.len()).max(start);
    &src[start..end]
}

fn build_decl(d: &oxc_css_parser::ast::Declaration, src: &str) -> Decl {
    let raw_name = src
        .get(d.span.start.min(src.len())..d.colon_span.start.min(src.len()))
        .unwrap_or("");
    let mut name = raw_name.trim().to_string();
    // 自定义属性名大小写敏感（`--Foo` 与 `--foo` 是两个属性），不能折叠。
    if !name.starts_with("--") {
        name = name.to_ascii_lowercase();
    }
    let value_end = d.important.as_ref().map_or(d.span.end, |i| i.span.start);
    let raw_value = src
        .get(d.colon_span.end.min(src.len())..value_end.min(src.len()))
        .unwrap_or("");
    let mut value = norm_value(raw_value.trim().trim_end_matches(';'));
    if d.important.is_some() {
        value.push_str(" !important");
    }
    Decl { name, value }
}

fn build_nodes(stmts: &[Statement], src: &str, counter: &mut usize) -> Vec<Node> {
    let mut nodes: Vec<Node> = stmts
        .iter()
        .map(|st| build_node(st, src, counter))
        .collect();
    // 空规则对层叠没有任何作用（没有声明、没有嵌套内容）：去掉是可证明等价的。
    // 它还有一个现实来源：现代 dart-sass 把嵌套规则之后的注释保留在原位置，
    // 输出成 `sel { /* 注释 */ }` 这种只剩注释的包装规则。
    nodes.retain(|n| !matches!(n, Node::Rule { body, .. } if body.is_empty()));
    nodes
}

fn build_node(st: &Statement, src: &str, counter: &mut usize) -> Node {
    match st {
        Statement::QualifiedRule(r) => {
            *counter += 1;
            Node::Rule {
                index: *counter,
                selector: norm_selector(slice(src, &r.selector.span)),
                body: build_nodes(&r.block.statements, src, counter),
            }
        }
        Statement::KeyframeBlock(k) => {
            *counter += 1;
            let selector = src
                .get(k.span.start.min(src.len())..k.block.span.start.min(src.len()))
                .unwrap_or("")
                .trim();
            Node::Rule {
                index: *counter,
                selector: norm_selector(selector),
                body: build_nodes(&k.block.statements, src, counter),
            }
        }
        Statement::UnknownQualifiedRule(r) => {
            *counter += 1;
            Node::Rule {
                index: *counter,
                selector: norm_selector(slice(src, &r.prelude.span)),
                body: build_nodes(&r.block.statements, src, counter),
            }
        }
        Statement::AtRule(a) => {
            *counter += 1;
            let name = a.name.name.to_ascii_lowercase();
            let prelude = a
                .prelude
                .as_ref()
                .map(|p| norm_prelude(slice(src, p.span())))
                .unwrap_or_default();
            let head = if prelude.is_empty() {
                format!("@{name}")
            } else {
                format!("@{name} {prelude}")
            };
            let body = a
                .block
                .as_ref()
                .map(|b| build_nodes(&b.statements, src, counter));
            Node::AtRule {
                index: *counter,
                head,
                body,
            }
        }
        Statement::Declaration(d) => Node::Decl(build_decl(d, src)),
        other => Node::Raw(norm_value(slice(src, other.span()))),
    }
}

fn build_model(src: &str) -> Result<Model, String> {
    let (cleaned, licenses) = clean_source(src);
    let alloc = Allocator::default();
    let mut parser = ParserBuilder::new(&alloc, &cleaned)
        .syntax(Syntax::Css)
        .build();
    let ast: Stylesheet = parser.parse().map_err(|e| format!("{e:?}"))?;
    let mut counter = 0;
    let nodes = build_nodes(&ast.statements, &cleaned, &mut counter);
    Ok(Model { nodes, licenses })
}

// ---------------------------------------------------------------------------
// 比较
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
struct Ctx {
    /// 最近的规则序号；0 表示顶层 / 块外。
    index: usize,
    selector: String,
}

#[derive(Debug, Clone)]
struct Diff {
    category: &'static str,
    index: Option<usize>,
    selector: Option<String>,
    property: Option<String>,
    a: Option<String>,
    b: Option<String>,
    note: Option<&'static str>,
}

#[derive(Debug, Default)]
struct Report {
    diffs: Vec<Diff>,
    counts: BTreeMap<&'static str, usize>,
}

impl Report {
    fn push(&mut self, d: Diff) {
        *self.counts.entry(d.category).or_insert(0) += 1;
        self.diffs.push(d);
    }

    fn is_equal(&self) -> bool {
        self.diffs.is_empty()
    }
}

/// 保序贪心对齐：为 `a` 的每个元素在 `b` 里找**最近的、索引递增的**同键元素。
/// 返回 `(matched_pairs, a_unmatched, b_unmatched)`，三者都用原下标。
fn align(a: &[&Node], b: &[&Node]) -> (Vec<(usize, usize)>, Vec<usize>, Vec<usize>) {
    let mut positions: HashMap<String, VecDeque<usize>> = HashMap::new();
    for (bi, node) in b.iter().enumerate() {
        positions.entry(node.key()).or_default().push_back(bi);
    }
    let mut matched = Vec::new();
    let mut b_used = vec![false; b.len()];
    let mut a_unmatched = Vec::new();
    let mut next_b = 0usize;
    for (ai, node) in a.iter().enumerate() {
        let key = node.key();
        let hit = positions.get_mut(&key).and_then(|q| {
            while q.front().is_some_and(|&bi| bi < next_b) {
                q.pop_front();
            }
            q.pop_front()
        });
        match hit {
            Some(bi) => {
                matched.push((ai, bi));
                b_used[bi] = true;
                next_b = bi + 1;
            }
            None => a_unmatched.push(ai),
        }
    }
    let b_unmatched = (0..b.len()).filter(|bi| !b_used[*bi]).collect();
    (matched, a_unmatched, b_unmatched)
}

fn compare_list(a: &[Node], b: &[Node], ctx: &Ctx, out: &mut Report) {
    let a_items: Vec<&Node> = a.iter().filter(|n| !matches!(n, Node::Decl(_))).collect();
    let b_items: Vec<&Node> = b.iter().filter(|n| !matches!(n, Node::Decl(_))).collect();
    compare_items(&a_items, &b_items, ctx, out);

    let a_decls: Vec<&Decl> = a
        .iter()
        .filter_map(|n| match n {
            Node::Decl(d) => Some(d),
            _ => None,
        })
        .collect();
    let b_decls: Vec<&Decl> = b
        .iter()
        .filter_map(|n| match n {
            Node::Decl(d) => Some(d),
            _ => None,
        })
        .collect();
    compare_decls(&a_decls, &b_decls, ctx, out);
}

fn rule_category(node: &Node, missing: bool) -> (&'static str, &'static str) {
    match node {
        Node::AtRule { .. } => (
            if missing {
                "at-rule-missing"
            } else {
                "at-rule-extra"
            },
            if missing { "a" } else { "b" },
        ),
        _ => (
            if missing {
                "missing-rule"
            } else {
                "extra-rule"
            },
            if missing { "a" } else { "b" },
        ),
    }
}

fn compare_items(a: &[&Node], b: &[&Node], ctx: &Ctx, out: &mut Report) {
    let (matched, a_unmatched, b_unmatched) = align(a, b);

    // 纯换序：两边未匹配集合的键完全一致 → 是顺序问题，不是缺失。
    let mut a_keys: Vec<String> = a_unmatched.iter().map(|&i| a[i].key()).collect();
    let mut b_keys: Vec<String> = b_unmatched.iter().map(|&i| b[i].key()).collect();
    a_keys.sort();
    b_keys.sort();
    if !a_unmatched.is_empty() && a_keys == b_keys {
        for &ai in &a_unmatched {
            let node = a[ai];
            let category = match node {
                Node::AtRule { .. } => "at-rule-order",
                _ => "rule-order",
            };
            out.push(Diff {
                category,
                index: node.index(),
                selector: rule_label(node),
                property: None,
                a: rule_label(node),
                b: None,
                note: None,
            });
        }
        for (ai, bi) in matched {
            recurse_matched(a[ai], b[bi], ctx, out);
        }
        return;
    }

    // 未匹配项按「同一位置」配对：同是规则 → 选择器不同（selector-mismatch），
    // 否则就是多出来/少了一条。用 matched 把未匹配项切段，按段配对，避免跨段错配。
    let mut segments: Vec<(Vec<usize>, Vec<usize>)> = Vec::new();
    let mut prev_a = 0;
    let mut prev_b = 0;
    for &(ai, bi) in &matched {
        let aseg: Vec<usize> = (prev_a..ai).filter(|i| a_unmatched.contains(i)).collect();
        let bseg: Vec<usize> = (prev_b..bi).filter(|i| b_unmatched.contains(i)).collect();
        segments.push((aseg, bseg));
        prev_a = ai + 1;
        prev_b = bi + 1;
    }
    let aseg: Vec<usize> = (prev_a..a.len())
        .filter(|i| a_unmatched.contains(i))
        .collect();
    let bseg: Vec<usize> = (prev_b..b.len())
        .filter(|i| b_unmatched.contains(i))
        .collect();
    segments.push((aseg, bseg));

    for (aseg, bseg) in segments {
        let pairable = matches!(
            (aseg.first().map(|&i| a[i]), bseg.first().map(|&i| b[i])),
            (Some(Node::Rule { .. }), Some(Node::Rule { .. }))
                | (Some(Node::AtRule { .. }), Some(Node::AtRule { .. }))
        );
        if pairable && aseg.len() == bseg.len() {
            for (x, y) in aseg.iter().zip(bseg.iter()) {
                let an = a[*x];
                let bn = b[*y];
                let category = if matches!(an, Node::AtRule { .. }) {
                    "at-rule-missing"
                } else {
                    "selector-mismatch"
                };
                out.push(Diff {
                    category,
                    index: an.index(),
                    selector: rule_label(an),
                    property: None,
                    a: rule_label(an),
                    b: rule_label(bn),
                    note: None,
                });
            }
            continue;
        }
        for ai in aseg {
            let node = a[ai];
            let (category, side) = rule_category(node, true);
            out.push(Diff {
                category,
                index: node.index(),
                selector: rule_label(node),
                property: None,
                a: rule_label(node),
                b: None,
                note: Some(if side == "a" {
                    "in a only"
                } else {
                    "in b only"
                }),
            });
        }
        for bi in bseg {
            let node = b[bi];
            let (category, side) = rule_category(node, false);
            out.push(Diff {
                category,
                index: node.index(),
                selector: rule_label(node),
                property: None,
                a: None,
                b: rule_label(node),
                note: Some(if side == "a" {
                    "in a only"
                } else {
                    "in b only"
                }),
            });
        }
    }
    for &(ai, bi) in &matched {
        recurse_matched(a[ai], b[bi], ctx, out);
    }
}

fn rule_label(node: &Node) -> Option<String> {
    match node {
        Node::Rule { selector, .. } => Some(selector.clone()),
        Node::AtRule { head, .. } => Some(head.clone()),
        _ => None,
    }
}

fn recurse_matched(an: &Node, bn: &Node, ctx: &Ctx, out: &mut Report) {
    match (an, bn) {
        (
            Node::Rule {
                index,
                selector,
                body: abody,
            },
            Node::Rule { body: bbody, .. },
        ) => {
            let inner = Ctx {
                index: *index,
                selector: selector.clone(),
            };
            compare_list(abody, bbody, &inner, out);
        }
        (
            Node::AtRule {
                index,
                head,
                body: abody,
            },
            Node::AtRule { body: bbody, .. },
        ) => match (abody, bbody) {
            (Some(ab), Some(bb)) => {
                let inner = Ctx {
                    index: ctx.index,
                    selector: ctx.selector.clone(),
                };
                compare_list(ab, bb, &inner, out);
            }
            (None, None) => {}
            _ => out.push(Diff {
                category: "at-rule-block",
                index: Some(*index),
                selector: Some(head.clone()),
                property: None,
                a: Some(if abody.is_some() { "block" } else { "no-block" }.to_string()),
                b: Some(if bbody.is_some() { "block" } else { "no-block" }.to_string()),
                note: None,
            }),
        },
        (Node::Raw(a), Node::Raw(b)) => {
            if a != b {
                out.push(Diff {
                    category: "decl-value",
                    index: None,
                    selector: None,
                    property: None,
                    a: Some(a.clone()),
                    b: Some(b.clone()),
                    note: None,
                });
            }
        }
        _ => out.push(Diff {
            category: "selector-mismatch",
            index: an.index(),
            selector: rule_label(an),
            property: None,
            a: rule_label(an),
            b: rule_label(bn),
            note: None,
        }),
    }
}

fn compare_decls(a: &[&Decl], b: &[&Decl], ctx: &Ctx, out: &mut Report) {
    let a_nodes: Vec<Node> = a.iter().map(|d| Node::Decl((*d).clone())).collect();
    let b_nodes: Vec<Node> = b.iter().map(|d| Node::Decl((*d).clone())).collect();
    let a_refs: Vec<&Node> = a_nodes.iter().collect();
    let b_refs: Vec<&Node> = b_nodes.iter().collect();
    let (matched, a_unmatched, mut b_unmatched) = align(&a_refs, &b_refs);

    let mut a_names: Vec<String> = a_unmatched.iter().map(|&i| a[i].name.clone()).collect();
    let mut b_names: Vec<String> = b_unmatched.iter().map(|&i| b[i].name.clone()).collect();
    a_names.sort();
    b_names.sort();
    if !a_unmatched.is_empty() && a_names == b_names {
        for &ai in &a_unmatched {
            out.push(Diff {
                category: "decl-order",
                index: Some(ctx.index),
                selector: selector_or_none(ctx),
                property: Some(a[ai].name.clone()),
                a: None,
                b: None,
                note: None,
            });
        }
        b_unmatched.clear();
        for (ai, bi) in matched {
            diff_decl_values(a[ai], b[bi], ctx, out);
        }
        return;
    }

    for (ai, bi) in matched {
        diff_decl_values(a[ai], b[bi], ctx, out);
    }
    for &ai in &a_unmatched {
        out.push(Diff {
            category: "decl-missing",
            index: Some(ctx.index),
            selector: selector_or_none(ctx),
            property: Some(a[ai].name.clone()),
            a: Some(a[ai].value.clone()),
            b: None,
            note: Some("in a only"),
        });
    }
    for &bi in &b_unmatched {
        out.push(Diff {
            category: "decl-missing",
            index: Some(ctx.index),
            selector: selector_or_none(ctx),
            property: Some(b[bi].name.clone()),
            a: None,
            b: Some(b[bi].value.clone()),
            note: Some("in b only"),
        });
    }
}

fn selector_or_none(ctx: &Ctx) -> Option<String> {
    if ctx.selector.is_empty() {
        None
    } else {
        Some(ctx.selector.clone())
    }
}

fn diff_decl_values(a: &Decl, b: &Decl, ctx: &Ctx, out: &mut Report) {
    if a.value != b.value {
        out.push(Diff {
            category: "decl-value",
            index: Some(ctx.index),
            selector: selector_or_none(ctx),
            property: Some(a.name.clone()),
            a: Some(a.value.clone()),
            b: Some(b.value.clone()),
            note: None,
        });
    }
}

fn compare_models(a: &Model, b: &Model) -> Report {
    let mut report = Report::default();
    // 许可证注释按顺序逐条比较：`/*! ... */` 会进入产物，是可见内容。
    let mut ai = 0usize;
    let mut bi = 0usize;
    while ai < a.licenses.len() || bi < b.licenses.len() {
        match (a.licenses.get(ai), b.licenses.get(bi)) {
            (Some(x), Some(y)) if x == y => {
                ai += 1;
                bi += 1;
            }
            (x, y) => {
                report.push(Diff {
                    category: "license-comment",
                    index: None,
                    selector: None,
                    property: None,
                    a: x.cloned(),
                    b: y.cloned(),
                    note: None,
                });
                if x.is_some() {
                    ai += 1;
                }
                if y.is_some() {
                    bi += 1;
                }
            }
        }
    }
    compare_list(&a.nodes, &b.nodes, &Ctx::default(), &mut report);
    report
}

fn compare_css(a: &str, b: &str) -> Result<Report, String> {
    let ma = build_model(a)?;
    let mb = build_model(b)?;
    Ok(compare_models(&ma, &mb))
}

// ---------------------------------------------------------------------------
// 输出
// ---------------------------------------------------------------------------

fn fmt_diff(i: usize, d: &Diff) -> String {
    let mut s = String::new();
    let index = d
        .index
        .filter(|i| *i > 0)
        .map(|i| format!("rule #{i}"))
        .unwrap_or_else(|| "top-level".to_string());
    let selector = d
        .selector
        .as_ref()
        .map(|s| format!(" `{s}`"))
        .unwrap_or_default();
    let prop = d
        .property
        .as_ref()
        .map(|p| format!(" prop `{p}`"))
        .unwrap_or_default();
    let note = d.note.map(|n| format!(" ({n})")).unwrap_or_default();
    let _ = write!(
        s,
        "{i}. [{cat}] {index}{selector}{prop}{note}",
        cat = d.category
    );
    if let Some(a) = &d.a {
        let _ = write!(s, "\n     a: {a}");
    }
    if let Some(b) = &d.b {
        let _ = write!(s, "\n     b: {b}");
    }
    s
}

fn to_json(equal: bool, a: &str, b: &str, report: &Report) -> serde_json::Value {
    let diffs: Vec<serde_json::Value> = report
        .diffs
        .iter()
        .map(|d| {
            serde_json::json!({
                "category": d.category,
                "index": d.index,
                "selector": d.selector,
                "property": d.property,
                "a": d.a,
                "b": d.b,
                "note": d.note,
            })
        })
        .collect();
    serde_json::json!({
        "equal": equal,
        "a": a,
        "b": b,
        "counts": report.counts,
        "diff_count": report.diffs.len(),
        "diffs": diffs,
    })
}

fn usage() -> ! {
    eprintln!("usage: undercoat-diff <a.css> <b.css> [--show N] [--json]");
    std::process::exit(2);
}

fn main() {
    let mut files: Vec<PathBuf> = Vec::new();
    let mut show = 20usize;
    let mut json = false;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--show" => {
                show = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or_else(|| usage());
            }
            "--json" => json = true,
            "-h" | "--help" => usage(),
            other => files.push(PathBuf::from(other)),
        }
    }
    if files.len() != 2 {
        usage();
    }
    let (pa, pb) = (&files[0], &files[1]);
    let read = |p: &PathBuf| -> String {
        std::fs::read_to_string(p).unwrap_or_else(|e| {
            eprintln!("read {}: {e}", p.display());
            std::process::exit(2);
        })
    };
    let (sa, sb) = (read(pa), read(pb));

    let report = match compare_css(&sa, &sb) {
        Ok(r) => r,
        Err(e) => {
            let which = if build_model(&sa).is_err() { pa } else { pb };
            eprintln!("parse error in {}: {e}", which.display());
            std::process::exit(2);
        }
    };

    if json {
        println!(
            "{}",
            serde_json::to_string(&to_json(
                report.is_equal(),
                &pa.display().to_string(),
                &pb.display().to_string(),
                &report
            ))
            .expect("serialize report")
        );
        std::process::exit(if report.is_equal() { 0 } else { 1 });
    }

    println!(
        "semantic: {}",
        if report.is_equal() {
            "EQUAL"
        } else {
            "DIFFERENT"
        }
    );
    println!("  a: {}", pa.display());
    println!("  b: {}", pb.display());
    if report.is_equal() {
        return;
    }
    let counts: Vec<String> = CATEGORIES
        .iter()
        .filter_map(|c| report.counts.get(c).map(|v| format!("{c}={v}")))
        .collect();
    println!("  counts: {}", counts.join(" "));
    println!("  diffs: {}", report.diffs.len());
    for (i, d) in report.diffs.iter().take(show).enumerate() {
        println!("{}", fmt_diff(i + 1, d));
    }
    if report.diffs.len() > show {
        println!("  ... {} more (use --show N)", report.diffs.len() - show);
    }
    std::process::exit(1);
}

// ---------------------------------------------------------------------------
// 单测
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn diff(a: &str, b: &str) -> Report {
        compare_css(a, b).expect("parse")
    }

    fn categories(r: &Report) -> Vec<&'static str> {
        r.diffs.iter().map(|d| d.category).collect()
    }

    #[test]
    fn whitespace_and_indentation_only() {
        let a = "a {\n  color: red;\n}\n\n.b { color: blue }\n";
        let b = "a{color:red}.b{color:blue}\n";
        let r = diff(a, b);
        assert!(r.is_equal(), "{:?}", r.diffs);
    }

    #[test]
    fn leading_zero_is_equal() {
        let r = diff(
            "a{opacity:.5;margin:-.25em}",
            "a{opacity:0.5;margin:-0.25em}",
        );
        assert!(r.is_equal(), "{:?}", r.diffs);
    }

    #[test]
    fn declaration_order_is_not_equal() {
        let r = diff(
            "a{color:red;background:blue}",
            "a{background:blue;color:red}",
        );
        assert!(!r.is_equal());
        assert!(categories(&r).contains(&"decl-order"), "{:?}", r.diffs);
    }

    #[test]
    fn missing_declaration_is_not_equal() {
        let r = diff("a{color:red;margin:0}", "a{color:red}");
        assert!(!r.is_equal());
        let d = &r.diffs[0];
        assert_eq!(d.category, "decl-missing");
        assert_eq!(d.property.as_deref(), Some("margin"));
        assert_eq!(d.note, Some("in a only"));
    }

    #[test]
    fn pseudo_element_legacy_syntax_is_equal() {
        let r = diff(
            "a::before{content:'x'}b::first-line{color:red}",
            "a:before{content:'x'}b:first-line{color:red}",
        );
        assert!(r.is_equal(), "{:?}", r.diffs);
    }

    #[test]
    fn attribute_selector_quotes_are_equivalent() {
        let r = diff(
            r#"[type="a"]{x:1}[lang|="en"]{y:2}"#,
            "[type=a]{x:1}[lang|=en]{y:2}",
        );
        assert!(r.is_equal(), "{:?}", r.diffs);
    }

    #[test]
    fn class_name_case_matters() {
        let r = diff(".Foo{x:1}", ".foo{x:1}");
        assert!(!r.is_equal());
        assert_eq!(r.diffs[0].category, "selector-mismatch");
    }

    #[test]
    fn colors_are_not_normalized() {
        let r = diff("a{color:#052c65}", "a{color:rgb(5,44,101)}");
        assert!(!r.is_equal());
        assert_eq!(r.diffs[0].category, "decl-value");
    }

    #[test]
    fn normal_comments_are_ignored_but_license_comments_kept() {
        assert!(diff("/* hi */a{x:1}", "a{x:1}").is_equal());
        let r = diff("/*! keep */a{x:1}", "a{x:1}");
        assert!(!r.is_equal());
        assert_eq!(r.diffs[0].category, "license-comment");
    }

    #[test]
    fn at_rule_block_is_compared_recursively() {
        let r = diff(
            "@media (min-width: 768px){a{color:red}}",
            "@media (min-width:768px){a{color:red}}",
        );
        assert!(r.is_equal(), "{:?}", r.diffs);
        let r = diff("@media x{a{color:red}}", "@media x{a{color:blue}}");
        assert_eq!(r.diffs[0].category, "decl-value");
    }

    #[test]
    fn selector_mismatch_is_reported_as_such() {
        let r = diff("a{x:1}", ".b{x:1}");
        assert_eq!(r.diffs[0].category, "selector-mismatch");
        assert_eq!(r.diffs[0].a.as_deref(), Some("a"));
        assert_eq!(r.diffs[0].b.as_deref(), Some(".b"));
    }

    #[test]
    fn custom_property_name_is_case_sensitive() {
        let r = diff(":root{--Foo:1px}", ":root{--foo:1px}");
        assert!(!r.is_equal());
        assert_eq!(r.diffs[0].category, "decl-missing");
    }

    #[test]
    fn calc_operator_spacing_is_equal() {
        let r = diff(
            "a{width:calc((100% - 20rpx) / 2)}",
            "a{width:calc((100% - 20rpx)/2)}",
        );
        assert!(r.is_equal(), "{:?}", r.diffs);
        // `+`/`-` 不归一：`calc(1px + 2px)` 合法，`calc(1px+2px)` 不是同一个表达式
        assert!(!diff("a{width:calc(1px + 2px)}", "a{width:calc(1px+2px)}").is_equal());
    }

    #[test]
    fn empty_rules_are_ignored() {
        // 空规则（包括只剩普通注释的）对层叠没有作用。
        assert!(diff("a{x:1}b{}", "a{x:1}").is_equal());
        assert!(diff("a{x:1}b{/*c*/}", "a{x:1}").is_equal());
        // 但有内容的规则不能因此躲过检查
        assert!(!diff("a{x:1}b{y:2}", "a{x:1}").is_equal());
    }

    #[test]
    fn string_escapes_are_equivalent() {
        let r = diff("a{content:\"\\e644\"}", "a{content:\"\u{e644}\"}");
        assert!(r.is_equal(), "{:?}", r.diffs);
        let r = diff("a{content:\"\\e644\"}", "a{content:\"\\e645\"}");
        assert!(!r.is_equal());
    }
}
