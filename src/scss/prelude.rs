//! at-rule 前导：插值/变量替换、media 合并、`@supports` 条件结构化与打印。
//!
//! 由 `scss/mod.rs` 拆分而来（纯搬运）。

use super::*;

impl<'a> Converter<'a> {
    /// `@media` prelude 里的值：媒体特性 `name: value` 的值当 SassScript 求值
    /// （dart 的 `MediaQuery.value` 就是表达式），求不出来时才退回裸 `$var` 代入。
    pub(super) fn substitute_vars(&mut self, text: &str, base: usize) -> Result<String, Box<CompileError>> {
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
                    let ch = text[i..].chars().next().expect("非空");
                    out.push(ch);
                    i += ch.len_utf8();
                }
            }
        }
        Ok(out)
    }

    /// 一个媒体特性（`min-width: $bp - 1px`）：值含 `$` 时整体当 SassScript 求值。
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

    /// 媒体特性的值求值：只接受能完整解析成 SassScript 的表达式。
    fn media_value(&mut self, value: &str, at: usize) -> Option<String> {
        let trimmed = value.trim();
        let start = at + leading_ws(value);
        let evaluated = self.eval.eval_text(trimmed, start).ok()?;
        evaluated.to_css().ok()
    }

    /// prelude 文本里的裸 `$var` 替换（`media_feature` 求值失败时的兜底）。
    fn substitute_vars_fallback(&mut self, text: &str, base: usize) -> Result<String, Box<CompileError>> {
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

    /// 语句头到块首（无块则到语句尾）的原文 prelude。
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

/// `#{` 在 `text` 的 `start` 处，返回到配对 `}` 的字节下标。
pub(super) fn matching_brace(text: &str, start: usize) -> usize {
    crate::text::matching(text, start + 1).map_or_else(|| text.len() - 1, |end| end - 1)
}

/// `#{` 在 `chars` 的 `start` 处，返回到配对的 `}` 的下标。
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

/// 嵌套 `@media` 的 prelude 合并（dart `_mergeMediaQueries`）：
/// 交叉相乘每条查询；媒体类型相同则保留、不同则丢弃该组合；带 `not` / `only`
/// 修饰的查询不参与合并（dart 保持嵌套）。返回 `None` 表示整体不可合并。
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

/// 单条查询合并：`(a) and (b)`，媒体类型取相同的那个。
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

/// 拆一条媒体查询成「类型」与「条件」列表（顶层 `and` 分隔）。
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

/// `@import` 里的 `url(...)` 按 dart 的 `_visitImport` 重新序列化：参数是带引号的字符串时
/// 统一用双引号（字符串里含 `"` 时保留单引号），括号内空白折叠（`url( 'x' )` → `url("x")`）。
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
                    // 含双引号或转义时保持原样，避免换引号后语义变化。
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
        let ch = text[i..].chars().next().expect("字符边界");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// at-rule prelude 规范化：折叠空白；`(prop:value)` 补冒号后空格；逗号后空格；括号内不留空格。
/// 转义序列按 dart 的写法重新序列化（`\0` → `\0 `、`\25` → `\%`），终止空格不删。
pub(super) fn normalize_prelude(text: &str) -> String {
    // 转义序列的终止空白先换成哨兵：`a\61  b` 里第一个空格属于转义、第二个是真空格，
    // 直接折叠空白会把两者并成一个（dart 输出 `aa b`，折叠后只剩 `aab` 的输入）。
    let guarded = guard_escape_spaces(text);
    let collapsed = value::normalize_raw(&guarded).replace(ESCAPE_SPACE, " ");
    let chars: Vec<char> = collapsed.chars().collect();
    let mut out = String::with_capacity(collapsed.len());
    let mut parens = 0usize;
    // 转义写法的终止空格（`0\0 )`）不能被括号/逗号处的去空格逻辑吃掉。
    let mut protect = 0usize;
    // 标识符起点：只有这里出现数字才需要 `\31 ` 这种写法（`x\31 0` → `x10`）。
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
                // 转义后面的一个空白属于转义本身（`\61 b` → `ab`）。
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
        // url(...) 内部原样保留（`https://` 的冒号不能动）。
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

/// 转义终止空白的哨兵（U+0001，不会被当成空白折叠）。
const ESCAPE_SPACE: char = '\u{1}';

/// 把「CSS 转义自带的那个终止空白」替换成哨兵，其余文本原样（引号内不动）。
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

/// 去尾随空格，但保留「转义终止空格」（`protect` 之前的字节）。
fn trim_protected(out: &mut String, protect: usize) {
    while out.len() > protect && out.ends_with(' ') {
        out.pop();
    }
}

/// 单个字符按 dart 的标识符转义规则写回：控制字符/标识符起点的数字用 `\hex `（带终止空格），
/// 其余可打印字符用 `\c`，字母数字原样。
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

/// `@supports` 的条件是**结构化**的（dart 的 `SupportsCondition`），不是字符串：
/// 冗余括号在解析期就消失，序列化时按节点类型决定要不要补括号。
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
    /// `selector(:has(a))` / `foo(bar)`：函数形态原样保留实参。
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
    /// `#{}` 插值出来的裸条件。
    Raw(String),
}

enum SuppFail {
    /// 句法错误：位置（相对 prelude 的字节偏移）+ 文案。
    At(usize, String),
    /// 本实现还认不出的形态：退回纯文本规范化（不静默改写）。
    Unsupported,
}

type SuppResult<T> = Result<T, SuppFail>;

/// 把 `@supports` 的 prelude 解析成条件结构，再按 dart 的规则打印。
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

    /// 标识符（`and`/`or`/`not` 之类的关键字用）；转义序列原样吞掉。
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

    /// `[not] cond (and|or cond)*`：dart 要求同级运算符**同种**，混用报 `Expected "and".`
    fn condition(&mut self) -> SuppResult<Supports> {
        // 顶层的 `not` 是前缀（`@supports not (a: b)`），不是标识符。
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
        // `selector(...)` / `foo(bar)`：标识符 + 括号。裸标识符只能是插值出来的条件。
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
            // 裸标识符后面只能是 `and`/`or` 或结束，否则本实现不认（退回原文）。
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
        // `((cond))`：括号只是分组，直接消失（dart 在解析期就丢掉）。
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

    /// 当前所在的 `(` 组的匹配 `)` 位置（组内已按 `(`/`)`、引号、转义平衡）。
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

    /// 声明里的名字/值都按 SassScript 求值（dart 的 `evaluateToCss`）；认不出时保留原文。
    fn evaluate(&mut self, text: &str, offset: usize) -> SuppResult<String> {
        if text.is_empty() {
            return Ok(String::new());
        }
        match self.eval.eval_text(text, self.base + offset) {
            Ok(value) => Ok(value.to_css().unwrap_or_else(|_| text.to_string())),
            // 求值期的真实错误（未定义变量/非法运算）要报出去；解析不了的 CSS 值按原文保留
            // （等价于 dart 的 `SupportsAnything` 兜底）。
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

/// `open` 处是 `(` 时，返回配对 `)` 的位置（跳过引号与转义）。
fn paren_end(text: &str, open: usize) -> Option<usize> {
    crate::text::matching(text, open).map(|end| end - 1)
}

/// 组内第一个**顶层** `:`（跳过引号、括号、转义）。
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
                // `::` 之类不是声明分隔符
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

/// dart 的序列化规则：整条件直接打印；作为 `and`/`or`/`not` 的操作数时，
/// **Negation 恒定加括号**，**Operation 只在运算符不同（或处于 `not` 下）时加括号**，
/// 声明/函数/插值不加。
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
