use oxc_css_parser::ast::*;
use oxc_css_parser::pos::Span;

use super::Compiler;
use super::comments::block_comments;
use super::emit::attach_to_feature_values;
use super::imports::{is_css_path, split_import_options, split_quoted_path};
use super::value;
use crate::doc::Comment;

/// `(reference)` 导入在文本里包成的 at-rule 名：块内定义可用，但不输出。
pub(super) const REFERENCE_AT: &str = "-style-ref";

/// less 的单位是 `%|[a-z_]+`，所以 `6px-1px` 在 less 里是 `6px` 减 `1px`；
/// 而 CSS 词法会把 `px-1px` 整段当单位。这里在数字+字母单位与紧随的 `-数字`
/// 之间补一个空格，让后续 AST 与 less 同构（字符串与注释内不动）。
///
/// 没有需要修补的片段时返回原串的拷贝（调用方仍按新串解析/打印，偏移自洽）。
pub(super) fn split_signed_dimensions(src: &str) -> String {
    let bytes = src.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(src.len());
    let mut i = 0usize;
    let mut quote = 0u8;
    while i < bytes.len() {
        let c = bytes[i];
        if quote != 0 {
            out.push(c);
            if c == b'\\' {
                if let Some(&next) = bytes.get(i + 1) {
                    out.push(next);
                    i += 2;
                    continue;
                }
            } else if c == quote {
                quote = 0;
            }
            i += 1;
            continue;
        }
        if c == b'/' && matches!(bytes.get(i + 1), Some(b'/') | Some(b'*')) {
            let line = bytes[i + 1] == b'/';
            out.extend_from_slice(&bytes[i..i + 2]);
            i += 2;
            while i < bytes.len() {
                if line {
                    if bytes[i] == b'\n' {
                        break;
                    }
                } else if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
                    out.extend_from_slice(b"*/");
                    i += 2;
                    break;
                }
                out.push(bytes[i]);
                i += 1;
            }
            continue;
        }
        if c == b'"' || c == b'\'' {
            quote = c;
            out.push(c);
            i += 1;
            continue;
        }
        if c.is_ascii_digit() || (c == b'.' && bytes.get(i + 1).is_some_and(u8::is_ascii_digit)) {
            // 标识符内部（`.bug-100cm-1m`、`a100px-1px`）不是数值上下文
            if i > 0
                && matches!(bytes[i - 1], b if b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'#' | b'\\') || b >= 0x80)
            {
                out.push(c);
                i += 1;
                continue;
            }
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            if bytes.get(i) == Some(&b'.') {
                i += 1;
                while i < bytes.len() && bytes[i].is_ascii_digit() {
                    i += 1;
                }
            }
            let unit_start = i;
            while i < bytes.len() && (bytes[i].is_ascii_alphabetic() || bytes[i] == b'_') {
                i += 1;
            }
            let unit = &src[unit_start..i];
            // `1e-3` 的指数不是单位
            let signed = !unit.is_empty()
                && !unit.eq_ignore_ascii_case("e")
                && bytes.get(i) == Some(&b'-')
                && bytes.get(i + 1).is_some_and(|b| {
                    b.is_ascii_digit()
                        || (*b == b'.' && bytes.get(i + 2).is_some_and(u8::is_ascii_digit))
                });
            out.extend_from_slice(&bytes[start..i]);
            if signed {
                // 两侧都要空格：`6px -1px` 会让解析器把 `-1px` 当成缺失运算符
                out.extend_from_slice(b" - ");
                i += 1;
            }
            continue;
        }
        out.push(c);
        i += 1;
    }
    String::from_utf8(out).expect("utf8")
}

/// 源码里有 `@plugin` 指令（任意层级）：插件可在求值期注入变量，严格报错要关掉。
pub(super) fn has_plugin(stmts: &[Statement]) -> bool {
    stmts.iter().any(|st| match st {
        Statement::AtRule(a) => {
            a.name.name.eq_ignore_ascii_case("plugin")
                || a.block.as_ref().is_some_and(|b| has_plugin(&b.statements))
        }
        Statement::LessMixinDefinition(d) => has_plugin(&d.block.statements),
        Statement::QualifiedRule(r) => has_plugin(&r.block.statements),
        Statement::LessConditionalQualifiedRule(r) => has_plugin(&r.block.statements),
        Statement::LessVariableDeclaration(d) => match &d.value {
            ComponentValue::LessDetachedRuleset(dr) => has_plugin(&dr.block.statements),
            _ => false,
        },
        _ => false,
    })
}

pub(super) fn is_reference(at: &AtRule) -> bool {
    at.name.name == REFERENCE_AT
}

pub(super) fn is_charset(at: &AtRule) -> bool {
    at.name.name.eq_ignore_ascii_case("charset")
}

/// 远程 URL（`https://` / `//host/…`）：没有下载器，按 CSS import 原样保留。
pub(super) fn is_remote(href: &str) -> bool {
    href.contains("://") || href.starts_with("//")
}

/// 只有「原样保留的 CSS import」会被 less 提前到其它节点之前（`importNodeIndex`）；
/// 内联的 `.less` / `(inline)` 导入留在源序里。
pub(super) fn is_hoisted_import(text: &str) -> bool {
    let (options, rest) = split_import_options(text);
    if options.iter().any(|o| o == "inline" || o == "less") {
        return false;
    }
    split_quoted_path(&rest)
        .is_some_and(|(href, _)| is_remote(&href) || is_css_path(&href) || href.ends_with(".wxss"))
}

pub(super) fn is_import(at: &AtRule) -> bool {
    at.name.name.eq_ignore_ascii_case("import")
}

/// 体要继承父选择器的 at-rule（less 里 `isRooted = false` 的那些）。
pub(super) fn at_rule_keeps_parents(name: &str) -> bool {
    let bare = name.trim_start_matches('@');
    let bare = match bare.strip_prefix('-') {
        Some(rest) => rest.split_once('-').map(|(_, n)| n).unwrap_or(bare),
        None => bare,
    };
    matches!(
        bare.to_ascii_lowercase().as_str(),
        "media" | "document" | "supports" | "layer"
    )
}

/// at-rule prelude 归一：括号内 `prop:value` → `prop: value`（less 输出形态）。
pub(super) fn normalize_prelude(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() + 4);
    let chars: Vec<char> = raw.chars().collect();
    for (i, &ch) in chars.iter().enumerate() {
        out.push(ch);
        if ch == ':' {
            let prev = i.checked_sub(1).and_then(|j| chars.get(j));
            let next = chars.get(i + 1);
            if prev != Some(&':') && next != Some(&' ') && next.is_some_and(|c| *c != ':') {
                out.push(' ');
            }
        }
    }
    out
}

/// `~"..."` → 内文：media prelude 里的转义字符串按普通文本输出。
pub(super) fn strip_escapes(raw: &str) -> String {
    if !raw.contains('~') {
        return raw.to_string();
    }
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.char_indices().peekable();
    while let Some((i, ch)) = chars.next() {
        if ch != '~' {
            out.push(ch);
            continue;
        }
        let Some(quote @ ('"' | '\'')) = chars.peek().map(|(_, c)| *c) else {
            out.push('~');
            continue;
        };
        chars.next();
        let start = i + 2;
        let mut end = None;
        for (j, c) in chars.by_ref() {
            if c == quote {
                end = Some(j);
                break;
            }
        }
        match end {
            Some(j) => out.push_str(&raw[start..j]),
            None => out.push_str(&raw[i..]),
        }
        if end.is_none() {
            return out;
        }
    }
    out
}

impl<'a> Compiler<'a> {
    /// @media 的条件文本（求值 + less 的 feature 归一化）。
    ///
    /// 返回 `(文本, 要移到体里的注释)`。less 在读完 at-rule 名后清空 commentStore
    /// （名字前的注释丢掉），之后的注释对 @media/@keyframes 这类列表型 prelude
    /// 会成为体内规则列表的首批节点；其它 at-rule 的 prelude 注释留在原文里。
    pub(super) fn at_prelude(&mut self, at: &AtRule) -> (String, Vec<Comment>) {
        let start = at.name.span().end;
        let end = match &at.block {
            Some(block) => block.span.start,
            None => at.prelude.as_ref().map_or(start, |p| p.span().end),
        };
        if end <= start || end > self.src.len() {
            return (String::new(), Vec::new());
        }
        let span = Span { start, end };
        let full = self.slice(span);
        // `raw` 是 trim 过的：注释在 `raw` 里的下标要加回被裁掉的前导空白才是绝对偏移。
        let base = span.start + full.len() - full.trim_start().len();
        let raw = full.trim().to_string();
        let name = at.name.name.to_ascii_lowercase();
        let bare = name.trim_start_matches('@');
        let bare = match bare.strip_prefix('-') {
            Some(rest) => rest.split_once('-').map(|(_, n)| n).unwrap_or(bare),
            None => bare,
        };
        let listy = matches!(bare, "media" | "keyframes" | "counter-style");
        let is_media = bare == "media";
        let comments = block_comments(&raw);
        if comments.is_empty() {
            return (self.finish_prelude(at, raw, span), Vec::new());
        }
        let mut moved = Vec::new();
        let mut plain = String::with_capacity(raw.len());
        let mut cursor = 0;
        let mut content_seen = false;
        // less 把 prelude 里的注释挂到“下一个值”上：@media 特征里 `: ` 之后；
        // 找不到值时落到体外块的首部。
        let mut pending: Vec<(usize, Comment)> = Vec::new();
        for &(start, end) in &comments {
            let piece = &raw[cursor..start];
            if !piece.trim().is_empty() {
                content_seen = true;
            }
            let text = &raw[start..end];
            let abs = base + start;
            self.mark_comments(Span {
                start: abs,
                end: abs + text.len(),
            });
            plain.push_str(piece);
            cursor = end;
            // 名字后、首个内容前的注释被 less 清掉
            if !content_seen {
                continue;
            }
            let comment = self.comment(abs, abs + text.len());
            match (is_media, listy) {
                (true, _) => {
                    // 注释占位成一个空格，避免去掉后两个 token 粘连成 `and(max-width`
                    plain.push(' ');
                    pending.push((plain.len(), comment));
                }
                (false, true) => moved.push(comment),
                (false, false) => plain.push_str(text),
            }
        }
        let rest = &raw[cursor.min(raw.len())..];
        plain.push_str(rest);
        let plain = if is_media {
            let (text, leftover) = attach_to_feature_values(&plain, &pending);
            moved.extend(leftover);
            text.split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .replace(" ,", ",")
        } else if listy {
            plain
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .replace(" ,", ",")
        } else {
            plain.trim().to_string()
        };
        (self.finish_prelude(at, plain, span), moved)
    }

    pub(super) fn finish_prelude(&self, at: &AtRule, raw: String, span: Span) -> String {
        // prelude 里的 `@var` / `@{var}` 按当前作用域求值（less 在 prelude 层求值）
        let raw = value::substitute(&raw, self.src, &self.env, 0, Some(span));
        // 只有 @media 的 feature 会被 less 归一（`(min-width:100px)` → 加空格）；
        // @supports 的 prelude 由 less 原样透传。
        if at.name.name.eq_ignore_ascii_case("media") {
            normalize_prelude(&strip_escapes(&raw))
        } else {
            raw
        }
    }
}
