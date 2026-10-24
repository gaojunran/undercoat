//! Declarations and values: plain/nested/custom properties, interpolated
//! identifiers, value text boundaries.
//!
//! Split out of `scss/mod.rs` (pure code move).

use super::*;

impl<'a> Converter<'a> {
    /// Argument binding: shares the same implementation as `@function`
    /// (`eval::Evaluator::bind`).
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

    /// Declaration list (`@font-face` / `@page` / keyframe blocks): declarations +
    /// variables + allowed mixins/control flow.
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
            return Err(self.err(
                end,
                "SCSS feature not implemented yet (S2): style rules inside a declaration list",
            ));
        }
        Ok(out)
    }

    pub(super) fn var_decl(
        &mut self,
        var: &SassVariableDeclaration<'a>,
    ) -> Result<(), Box<CompileError>> {
        if let Some(namespace) = &var.namespace {
            return Err(self.err(
                namespace.span.start,
                "SCSS feature not implemented yet (S2): namespaced variable declaration",
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
        // `with` configuration only takes effect at the module global level: the
        // configured value becomes the variable's initial value directly (the
        // expression of a `!default` declaration is no longer evaluated); a
        // non-`!default` declaration receiving a configuration is an error in dart.
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

    pub(super) fn decl(
        &mut self,
        decl: &Declaration<'a>,
    ) -> Result<Option<Decl>, Box<CompileError>> {
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

        // dart decides "custom property" by whether the property name in the
        // **source** starts with `--`: in `#{$p}: $v` the name is an interpolation, so
        // `$v` is still evaluated as ordinary SassScript (bootstrap's `rfs()` relies
        // on it). We look for a literal `--` at the start of the source name:
        // `--#{$p}foo: .5` is a custom property too, and its value is handled as
        // "literal text + interpolation" (numbers are not reformatted).
        let custom = self.src()[name_span.start..].starts_with("--");
        let mut custom_strip = None;
        let mut value = if custom {
            // dart reads a custom property value up to `;` (`!important` is part of the
            // raw text); whitespace is collapsed at parse time and newline
            // re-indentation is left to the serializer (only it knows the output
            // nesting level).
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

    /// Value text of an ordinary declaration; returns `None` when the value is
    /// `null` or serializes to empty (the whole declaration is dropped).
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
            // dart-sass: a declaration whose value is null is dropped entirely.
            return Ok(None);
        }
        let text = value.to_css().map_err(|message| {
            self.err(components.first().map_or(at, |v| v.span().start), message)
        })?;
        // A value that serializes to empty (`a: #{$null}`) drops the whole
        // declaration; trailing whitespace is removed as well (dart's `1 #{$null}`
        // outputs `1`). The terminator space of a hexadecimal escape must be kept
        // (`1px\0 `), otherwise `\0` would glue to the following characters into
        // another code point.
        let text = trim_value_end(&text);
        Ok((!text.is_empty()).then(|| text.to_string()))
    }

    /// Prefix the property name inside a nested property (`font: { family: serif }`):
    /// `font-family`.
    fn prefixed_name(&self, name: String) -> String {
        match &self.decl_prefix {
            Some(prefix) => format!("{prefix}-{name}"),
            None => name,
        }
    }

    /// Nested property block: the outer declaration (when it has a value) plus the
    /// child declarations expanded with the prefix.
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

    /// Property name: supports `#{}` interpolation (`border-#{$side}`).
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
                            return Err(self.err(
                                interp.span.start,
                                "SCSS feature not implemented yet (S1): template placeholder",
                            ));
                        }
                    }
                }
                Ok(text)
            }
            other => Err(self.err(
                ident_span(other).start,
                format!(
                    "SCSS feature not implemented yet (S1): {}",
                    snippet(self.src(), ident_span(other).start, ident_span(other).end)
                ),
            )),
        }
    }

    /// End of the declaration "region": from after the value up to `;` (inclusive) or
    /// `}` (exclusive).
    ///
    /// A block comment inside the region belongs to the declaration and dart-sass
    /// drops it entirely; only one after `;` is a statement-level comment.
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

    /// End of the raw custom property value: a top-level `;` or `}` (not inside
    /// strings or brackets).
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

    /// Custom property value: collapsed following dart's parse-time whitespace rules
    /// (a whitespace run collapses to the **last character** of the run, consecutive
    /// newlines collapse into one, indentation after a newline is kept verbatim),
    /// strings are normalized to double quotes, and `#{}` is evaluated. Returns the
    /// collapsed text and the newline re-indentation parameters.
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
                // Escapes are passed through verbatim (including the terminator space
                // of a hexadecimal escape).
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
                // A whitespace run collapses to the **last character** of the run (a run
                // ending in a tab keeps the tab instead of normalizing to a space); the
                // indentation after a newline is kept whole and left to serialization to
                // re-indent (dart relies on it for character shifting). This matches
                // dart's `_interpolatedDeclarationValue`: the current character is only
                // written when a newline was just written or when the next character is
                // not whitespace.
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

    /// Evaluate a string body (including `#{}`): escapes in the value segments are
    /// decoded, interpolations are evaluated and then appended.
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

/// The three branches of dart's `_writeReindentedValue`: no newline / only trailing
/// whitespace / re-indent by indentation.
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

/// Static text of an identifier (interpolations/placeholders return `None`).
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

/// Re-wrap a piece of content back into `@media` with the given prelude.
/// Strip trailing whitespace from the value, but keep the terminator space of a
/// hexadecimal escape.
fn trim_value_end(text: &str) -> &str {
    let trimmed = text.trim_end();
    if trimmed.len() == text.len() {
        return text;
    }
    let bytes = trimmed.as_bytes();
    // Ends like `\<1..6 hex digits>`: dart always appends a space to terminate it.
    let mut i = bytes.len();
    while i > 0 && bytes[i - 1].is_ascii_hexdigit() {
        i -= 1;
    }
    if i > 0 && bytes[i - 1] == b'\\' && bytes.len() - i <= 6 {
        return &text[..trimmed.len() + 1];
    }
    trimmed
}
