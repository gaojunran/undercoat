//! 选择器解析与规范化：父选择器展开、`&` 替换、伪类参数、`nth`/`an+b` 归一化。
//!
//! 由 `scss/mod.rs` 拆分而来（纯搬运）。

use super::*;

impl<'a> Converter<'a> {
    /// 声明值里 `SassNestingDeclaration`（`font: { ... }`）的位置。
    pub(super) fn nesting_cut(&self, decl: &Declaration<'a>) -> Option<usize> {
        decl.value
            .iter()
            .position(|c| matches!(c, ComponentValue::SassNestingDeclaration(_)))
    }

    /// 选择器列表：插值 → 规范化 → 按逗号拆开 → 相对父选择器解析。
    /// `X > { ... }` 的尾随组合符：oxc 的选择器 span 不含它，得看原文。
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
            // `X > { ... }`：尾随组合符在 oxc 里不在选择器 span 内，从原文补上。
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
                // 裸 `@at-root` 内部：没有外层嵌套，但 `&` 仍指原来的选择器。
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
                // dart-sass `resolveParentSelectors`：`flattenVertically` 合并各子选择器
                // 的展开结果（按序号交叉取值），每条产物带自己的 `lineBreak`——
                // 含 `&` 的取父选择器的，纯后代拼接的取子选择器自己的。
                let lists: Vec<Vec<Sel>> = parts
                    .into_iter()
                    .map(|(part, line_break)| resolve_selector(&part, parents, line_break))
                    .collect();
                let resolved = flatten_vertically(lists);
                // 兜底：产物里绝不允许残留未替换的 `&`（那会直接变成无效 CSS）。
                if let Some(bad) = resolved.iter().find(|sel| has_parent_selector(&sel.text)) {
                    return Err(self.err(
                        list.span.start,
                        format!("SCSS 内部错误：选择器里残留未替换的 `&`（{}）", bad.text),
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
            // 关键帧选择器列表固定用 `, ` 连接（dart 的 `_commaSeparator`）。
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

/// 选择器是否含父选择器 `&`（忽略字符串/属性值里的 `&`）。
/// 选择器里是否有 `&`（含函数式伪类参数里的：`:not(&)`）。
fn has_parent_selector(text: &str) -> bool {
    find_parent_in(text, true).is_some()
}

/// 顶层（不在括号里）是否有 `&`：只用于「笛卡尔积」展开。
fn find_parent(text: &str) -> Option<(usize, usize)> {
    find_parent_in(text, false)
}

/// 第一个 `&` 的位置与紧随的后缀长度（`&-s` 的 `-s`）。
/// `deep == false` 时跳过括号内（那些是伪类参数里的 `&`，另走列表替换）。
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

/// 子选择器相对父选择器解析。
///
/// 跨行位（dart `ComplexSelector.lineBreak`）实测规则：
/// - 纯后代拼接：`父子任一跨行` 即换行；
/// - 含 `&`：只继承父选择器的跨行（`&.b,\n&.c` 输出在一行）。
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
    // 函数式伪类参数里的 `&` 用**整份父列表**代入（`:not(.a, .b)`），其余位置才做笛卡尔积。
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

/// 把 `&`（含 `&-suffix`）逐个替换成父选择器（`& + &` 展开成笛卡尔积）。
/// 结果附带「首个 `&` 用的是哪个父选择器」的下标（跨行位取自它）。
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

/// 伪类参数里的 `&`：dart 的 `SelectorList.resolveParentSelectors` 语义——
/// 参数按选择器列表拆开，每个复杂选择器里的多个 `&` 做笛卡尔积，
/// 最后 `flattenVertically`（按下标交叉）合并；`&` 只替换成单个父选择器。
fn expand_pseudo_args(text: &str, parents: &[Sel]) -> String {
    let mut out = text.to_string();
    // 从最内层往外套（`:not(:is(&))` 先解 `:is`）。
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

/// 最内层「参数里有 `&`」的函数式伪类：返回 `(` 的下标与伪类名。
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

/// `(` 前面是否是 `:name`（函数式伪类），返回伪类名。
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

/// `:nth-child(2n of &)`：`of` 之后才是选择器列表，前缀只打印一次。
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

/// 顶层 ` of `（两侧带空白）的位置。
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

/// 把一段选择器文本（伪类参数）解析成选择器列表，`&` 逐父展开。
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

/// 多个 `&` 的笛卡尔积；返回值附带「首个 `&` 用了哪个父选择器」。
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

/// dart 的 `flattenVertically`：按下标交叉合并多份列表。
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

/// 选择器列表按 dart 的换行位打印（用于伪类参数内部）。
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

/// 块内是否含规则类语句（决定 at-rule 体按规则还是声明列表处理）。
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

/// 声明类 at-rule 所处的静态容器。dart 在 **解析期** 就按容器拒绝某些声明（实测：
/// `@if false` 分支里的 `@mixin` 也报错），所以这里是独立于求值的静态遍历。
#[derive(Clone, Copy, Default)]
pub(super) struct Nesting {
    /// mixin 体（dart `_inMixin`）。
    pub(super) mixin: bool,
    /// `@include ... { }` 的 content 块（dart `_inContentBlock`）。
    pub(super) content_block: bool,
    /// 控制指令体内：`@if` / `@else` / `@each` / `@for` / `@while`。
    pub(super) control: bool,
    /// function 体（dart `_inFunction`）：只允许下表的 at-rule。
    pub(super) function: bool,
}

impl Nesting {
    /// function 体白名单（dart 的 function 子解析器）。
    pub(super) fn function_allows(name: &str) -> bool {
        matches!(
            name,
            "debug" | "warn" | "error" | "each" | "for" | "while" | "if" | "return"
        )
    }
}

/// 选择器规范化：折叠空白、组合符两侧一个空格、逗号后一个空格、括号内去空白。
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
                // 属性选择器里的字符串：合法标识符去引号，否则按引号规则重写。
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
                // 引号串是属性值：它后面的空白是后代选择器分隔符，不能被吞掉。
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

/// `:nth-child()` / `:nth-last-child()` 的 An+B 参数重排。
///
/// dart-sass 1.35.2 只对这两个伪类做结构化 An+B 序列化（去空白、`n`/`odd`/`even`
/// 小写、保留 A/B 原数字拼写与符号）；`:nth-of-type()` / `:nth-last-of-type()`
/// 实测**不**归一（空白按普通选择器折叠），所以不在此列。
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

/// An+B 的序列化：去掉全部空白、`n`/`odd`/`even` 小写，`of <selector>` 用单空格。
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

/// 在深度 0 处切出 `of` 子句（`2n+1 of .a`）。
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
