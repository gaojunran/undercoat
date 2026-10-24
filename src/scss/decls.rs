//! 声明与值：普通/嵌套/自定义属性、插值标识符、值文本边界。
//!
//! 由 `scss/mod.rs` 拆分而来（纯搬运）。

use super::*;

impl<'a> Converter<'a> {
    /// 实参绑定：与 `@function` 共用同一套（`eval::Evaluator::bind`）。
    pub(super) fn bind(
        &mut self,
        params: &[Param<'a>],
        rest: Option<&str>,
        inv: Invocation,
        at: usize,
    ) -> Result<Option<RestCheck>, Box<CompileError>> {
        let src = self.src();
        self.eval
            .bind(src, params, rest, inv, at)
            .map_err(|e| self.eval_err(e))
    }

    /// 声明列表（`@font-face` / `@page` / keyframe 块）：声明 + 变量 + 允许的 mixin/控制流。
    pub(super) fn decl_list(
        &mut self,
        stmts: &'a [Statement<'a>],
        end: usize,
    ) -> Result<Vec<DeclItem>, Box<CompileError>> {
        let mut out = Vec::new();
        let mut nested = Vec::new();
        self.decl_context = true;
        let scope = self.push_scope(false);
        let result = self.stmts(
            stmts,
            end,
            None,
            Span { start: 0, end: 0 },
            &mut out,
            &mut nested,
        );
        self.pop_scope(scope);
        self.decl_context = false;
        result?;
        if !nested.is_empty() {
            return Err(self.err(end, "SCSS 特性尚未实现（S2）: 声明列表内的规则"));
        }
        Ok(out)
    }

    pub(super) fn var_decl(&mut self, var: &SassVariableDeclaration<'a>) -> Result<(), Box<CompileError>> {
        if let Some(namespace) = &var.namespace {
            return Err(self.err(
                namespace.span.start,
                "SCSS 特性尚未实现（S2）: 命名空间变量声明",
            ));
        }
        let has = |name: &str| {
            var.flags
                .iter()
                .any(|f| f.keyword.name.eq_ignore_ascii_case(name))
        };
        let name = var.name.name.name;
        let default = has("default");
        let global = has("global");
        // `with` 配置只在模块全局层生效：配置值直接成为变量的初值（`!default`
        // 声明的表达式不再求值）；非 `!default` 声明收到配置就是 dart 的报错。
        if self.eval.at_module_global() {
            if !default {
                if let Some(at) = self.eval.config_conflict(name) {
                    return Err(self.err(
                        at,
                        "This variable was not declared with !default in the @used module.",
                    ));
                }
            } else if let Some(value) = self.eval.take_config(name) {
                self.eval.define(name, value, false, global);
                return Ok(());
            }
        }
        let value = self
            .eval
            .eval_value(self.src(), std::slice::from_ref(&var.value))
            .map_err(|e| self.eval_err(e))?;
        self.eval.define(name, value, default, global);
        Ok(())
    }

    pub(super) fn decl(&mut self, decl: &Declaration<'a>) -> Result<Option<Decl>, Box<CompileError>> {
        let name_span = *decl.name.span();
        let mut name = self.decl_name(&decl.name)?;
        if let Some(prefix) = decl.name_prefix {
            name.insert(0, prefix);
        }
        if let Some(suffix) = decl.name_suffix {
            name.push(suffix);
        }

        let value_end = decl
            .important
            .as_ref()
            .map_or(decl.span.end, |important| important.span.start);

        // dart 的「自定义属性」判定看**源码**里属性名是否以 `--` 开头：`#{$p}: $v`
        // 里属性名是插值，所以 `$v` 仍按普通 SassScript 求值（bootstrap 的 `rfs()` 依赖它）。
        // 看源码里属性名的开头是否有字面 `--`：`--#{$p}foo: .5` 也是自定义属性，
        // 值按「字面文本 + 插值」处理（数字不会被重新格式化）。
        let custom = self.src()[name_span.start..].starts_with("--");
        let mut custom_strip = None;
        let mut value = if custom {
            // dart 的自定义属性值一直读到 `;`（`!important` 也在原文里），空白在
            // 解析期折叠，换行的重排交给序列化器（那里才知道输出层级）。
            let end = match &decl.important {
                Some(important) => self
                    .custom_raw_end(decl.colon_span.end)
                    .min(important.span.end),
                None => self.custom_raw_end(decl.colon_span.end),
            };
            let column = self.merged.position(name_span.start).2 as usize;
            let (text, strip) = self.custom_value(decl.colon_span.end, end, column)?;
            custom_strip = strip;
            text
        } else {
            match self.decl_value_text(&decl.value, decl.span.start)? {
                Some(text) => text,
                None => return Ok(None),
            }
        };

        if decl.important.is_some() && !custom {
            value.push_str(" !important");
        }
        Ok(Some(Decl {
            name: self.prefixed_name(name),
            name_span,
            value,
            value_span: Span {
                start: decl.colon_span.end,
                end: value_end,
            },
            custom,
            custom_strip,
        }))
    }

    /// 普通声明的值文本；`null` 或序列化后为空时返回 `None`（整条丢弃）。
    fn decl_value_text(
        &mut self,
        components: &[ComponentValue<'a>],
        at: usize,
    ) -> Result<Option<String>, Box<CompileError>> {
        let value = self
            .eval
            .eval_value(self.src(), components)
            .map_err(|e| self.eval_err(e))?;
        if value.is_null() {
            // dart-sass: null 值的声明整条丢弃。
            return Ok(None);
        }
        let text = value.to_css().map_err(|message| {
            self.err(components.first().map_or(at, |v| v.span().start), message)
        })?;
        // 值序列化为空（`a: #{$null}`）时整条声明丢弃；尾部空白也去掉
        // （dart 的 `1 #{$null}` 输出 `1`）。十六进制转义的终止空格要保留
        // （`1px\0 `），否则 `\0` 会和后面的字符黏连成另一个码位。
        let text = trim_value_end(&text);
        Ok((!text.is_empty()).then(|| text.to_string()))
    }

    /// 嵌套属性（`font: { family: serif }`）里的属性名带上前缀：`font-family`。
    fn prefixed_name(&self, name: String) -> String {
        match &self.decl_prefix {
            Some(prefix) => format!("{prefix}-{name}"),
            None => name,
        }
    }

    /// 嵌套属性块：外层声明（有值时）+ 按前缀展开的子声明。
    pub(super) fn nested_decl(
        &mut self,
        decl: &'a Declaration<'a>,
        cut: usize,
        parent: Option<&SelectorSet>,
        parent_span: Span,
        decls: &mut Vec<DeclItem>,
        nested: &mut Vec<Item>,
    ) -> Result<(), Box<CompileError>> {
        if parent.is_none() && !self.decl_context {
            return Err(self.err(
                decl.span.start,
                "Declarations may only be used within style rules.",
            ));
        }
        let name_span = *decl.name.span();
        let name = self.decl_name(&decl.name)?;
        let full = self.prefixed_name(name);
        if cut > 0 {
            if let Some(value) = self.decl_value_text(&decl.value[..cut], decl.span.start)? {
                let custom = self.src()[name_span.start..].starts_with("--");
                decls.push(DeclItem::Decl(Decl {
                    name: full.clone(),
                    name_span,
                    value,
                    value_span: Span {
                        start: decl.colon_span.end,
                        end: decl.value[cut - 1].span().end,
                    },
                    custom,
                    custom_strip: None,
                }));
            }
        }
        let Some(ComponentValue::SassNestingDeclaration(block)) = decl.value.get(cut) else {
            return Ok(());
        };
        let prev = self.decl_prefix.replace(full);
        let scope = self.push_scope(false);
        let result = self.stmts(
            &block.block.statements,
            block.block.span.end,
            parent,
            parent_span,
            decls,
            nested,
        );
        self.pop_scope(scope);
        self.decl_prefix = prev;
        result
    }

    /// 属性名：支持 `#{}` 插值（`border-#{$side}`）。
    fn decl_name(&mut self, name: &InterpolableIdent<'a>) -> Result<String, Box<CompileError>> {
        match name {
            InterpolableIdent::Literal(ident) => Ok(ident.name.to_string()),
            InterpolableIdent::SassInterpolated(interp) => {
                let mut text = String::new();
                for element in &interp.elements {
                    match element {
                        SassInterpolatedIdentElement::Static(part) => text.push_str(part.value),
                        SassInterpolatedIdentElement::Expression(comp) => {
                            let value = self
                                .eval
                                .eval_component(self.src(), comp)
                                .map_err(|e| self.eval_err(e))?;
                            text.push_str(
                                &value
                                    .interpolation()
                                    .map_err(|message| self.err(comp.span().start, message))?,
                            );
                        }
                        SassInterpolatedIdentElement::Placeholder(_) => {
                            return Err(
                                self.err(interp.span.start, "SCSS 特性尚未实现（S1）: 模板占位符")
                            );
                        }
                    }
                }
                Ok(text)
            }
            other => Err(self.err(
                ident_span(other).start,
                format!(
                    "SCSS 特性尚未实现（S1）: {}",
                    snippet(self.src(), ident_span(other).start, ident_span(other).end)
                ),
            )),
        }
    }

    /// 声明的「区间」终点：值之后到 `;`（含）或 `}`（不含）。
    ///
    /// 落在区间内的块注释属于声明内部，dart-sass 整段丢弃；只有 `;` 之后的
    /// 才是语句级注释。
    pub(super) fn decl_region_end(&self, from: usize) -> Span {
        let bytes = self.src().as_bytes();
        let mut i = from;
        while i < bytes.len() {
            match bytes[i] {
                b';' => {
                    return Span {
                        start: from,
                        end: i + 1,
                    };
                }
                b'}' => {
                    return Span {
                        start: from,
                        end: i,
                    };
                }
                b'/' if bytes.get(i + 1) == Some(&b'*') => {
                    i += 2;
                    while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                        i += 1;
                    }
                    i = (i + 2).min(bytes.len());
                }
                b'/' if bytes.get(i + 1) == Some(&b'/') => {
                    while i < bytes.len() && bytes[i] != b'\n' {
                        i += 1;
                    }
                }
                b if b.is_ascii_whitespace() => i += 1,
                _ => {
                    return Span {
                        start: from,
                        end: i,
                    };
                }
            }
        }
        Span {
            start: from,
            end: bytes.len(),
        }
    }

    /// 自定义属性值的原文终点：顶层 `;` 或 `}`（字符串/括号内不算）。
    fn custom_raw_end(&self, start: usize) -> usize {
        let bytes = self.src().as_bytes();
        let mut depth = 0usize;
        let mut quote: Option<u8> = None;
        let mut i = start;
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
                b'(' | b'[' | b'{' => depth += 1,
                b')' | b']' => depth = depth.saturating_sub(1),
                b'}' => {
                    if depth == 0 {
                        return i;
                    }
                    depth -= 1;
                }
                b';' if depth == 0 => return i,
                _ => {}
            }
            i += 1;
        }
        bytes.len()
    }

    /// 自定义属性值：按 dart 解析期的空白规则折叠（空白游程压成游程的**最后一个
    /// 字符**、连续换行压成一个、换行后的缩进原样保留），字符串归一双引号，`#{}`
    /// 求值。返回折叠后的文本与换行重排参数。
    fn custom_value(
        &mut self,
        from: usize,
        end: usize,
        column: usize,
    ) -> Result<(String, Option<CustomStrip>), Box<CompileError>> {
        let raw = self.src().get(from..end).unwrap_or("").to_string();
        let chars: Vec<(usize, char)> = raw.char_indices().collect();
        let mut out = String::with_capacity(raw.len());
        let mut wrote_newline = false;
        let mut i = 0;
        while i < chars.len() {
            let (byte, ch) = chars[i];
            match ch {
                // 转义原样带过（含十六进制转义的终止空格）。
                '\\' => {
                    out.push(ch);
                    if let Some(&(_, next)) = chars.get(i + 1) {
                        out.push(next);
                        i += 2;
                    } else {
                        i += 1;
                    }
                    wrote_newline = false;
                }
                '\'' | '"' => {
                    let quote = ch;
                    i += 1;
                    let content_start = chars.get(i).map_or(raw.len(), |&(byte, _)| byte);
                    while i < chars.len() {
                        if chars[i].1 == '\\' {
                            i += 2;
                            continue;
                        }
                        if chars[i].1 == quote {
                            break;
                        }
                        i += 1;
                    }
                    let content_end = chars.get(i).map_or(raw.len(), |&(byte, _)| byte);
                    let text = self
                        .interpolated_string(&raw[content_start..content_end], from + content_start)
                        .map_err(|e| self.eval_err(e))?;
                    out.push_str(&value::quote_string(&text));
                    i += 1;
                    wrote_newline = false;
                }
                '#' if chars.get(i + 1).map(|&(_, c)| c) == Some('{') => {
                    let j = interpolation_end(&chars, i);
                    let tail = chars.get(j + 1).map_or(raw.len(), |&(byte, _)| byte);
                    let interpolated = self
                        .eval
                        .interpolate(&raw[byte..tail], from + byte)
                        .map_err(|e| self.eval_err(e))?;
                    out.push_str(&interpolated);
                    wrote_newline = false;
                    i = j + 1;
                }
                // 空白游程压成**游程的最后一个字符**（tab 结尾就留 tab，不是统一变空格）；
                // 换行后的缩进整段保留，交给序列化期重排（dart 靠它做字符平移）。
                // 对齐 dart 的 `_interpolatedDeclarationValue`：只有「刚写过换行」或
                // 「下一个字符不是空白」时才写出当前字符。
                ' ' | '\t' => {
                    let next_ws = chars
                        .get(i + 1)
                        .is_some_and(|&(_, c)| is_space_or_newline(c));
                    if wrote_newline || !next_ws {
                        out.push(ch);
                    }
                    i += 1;
                }
                '\n' | '\r' | '\x0c' => {
                    if !out.ends_with('\n') {
                        out.push('\n');
                    }
                    wrote_newline = true;
                    i += 1;
                }
                _ => {
                    out.push(ch);
                    wrote_newline = false;
                    i += 1;
                }
            }
        }
        let strip = custom_strip(&out, column);
        Ok((out, strip))
    }

    /// 字符串体（含 `#{}`）求值：值段解转义，插值段求值后拼接。
    fn interpolated_string(&mut self, inner: &str, offset: usize) -> Result<String, EvalError> {
        let mut out = String::new();
        let mut pos = 0;
        while let Some(rel) = inner[pos..].find("#{") {
            let at = pos + rel;
            out.push_str(&value::decode_css_escapes(&inner[pos..at]));
            let end = matching_brace(inner, at);
            let text = self.eval.interpolate(&inner[at..=end], offset + at)?;
            out.push_str(&text);
            pos = end + 1;
        }
        out.push_str(&value::decode_css_escapes(&inner[pos..]));
        Ok(out)
    }
}

/// dart 的 `_writeReindentedValue` 三个分支：无换行 / 只有尾部空白 / 按缩进重排。
fn custom_strip(text: &str, column: usize) -> Option<CustomStrip> {
    if !text.contains('\n') {
        return None;
    }
    let min = text
        .split('\n')
        .skip(1)
        .filter_map(|line| {
            let lead = line.len() - line.trim_start_matches([' ', '\t']).len();
            (!line.trim().is_empty()).then_some(lead)
        })
        .min();
    Some(match min {
        None => CustomStrip::Trailing,
        Some(min) => CustomStrip::Indent(min.min(column)),
    })
}

/// 标识符的静态文本（插值/占位符返回 `None`）。
pub(super) fn ident_name<'a>(ident: &'a InterpolableIdent<'a>) -> Option<&'a str> {
    match ident {
        InterpolableIdent::Literal(i) => Some(i.name),
        _ => None,
    }
}

fn ident_span(ident: &InterpolableIdent) -> Span {
    match ident {
        InterpolableIdent::Literal(i) => i.span,
        InterpolableIdent::SassInterpolated(s) => s.span,
        InterpolableIdent::LessInterpolated(s) => s.span,
        InterpolableIdent::Placeholder(p) => p.span,
    }
}

/// 用给定 prelude 把一段内容重新包回 `@media`。
/// 去掉值尾部的空白，但保留十六进制转义的终止空格。
fn trim_value_end(text: &str) -> &str {
    let trimmed = text.trim_end();
    if trimmed.len() == text.len() {
        return text;
    }
    let bytes = trimmed.as_bytes();
    // 结尾形如 `\<1..6 位十六进制>`：dart 一定补一个空格断句。
    let mut i = bytes.len();
    while i > 0 && bytes[i - 1].is_ascii_hexdigit() {
        i -= 1;
    }
    if i > 0 && bytes[i - 1] == b'\\' && bytes.len() - i <= 6 {
        return &text[..trimmed.len() + 1];
    }
    trimmed
}
