use oxc_css_parser::ast::*;
use oxc_css_parser::pos::Span;

use super::eval::interp_tokens;
use super::extend::{self, Need, Sel};
use super::value;
use super::Compiler;

/// 把选择器文本拆成 less 的 mixin 元素 token（简单选择器，忽略组合子与伪类细节）。
pub(super) fn selector_tokens(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if matches!(c, '.' | '#' | '&' | '*' | ',') || c.is_alphanumeric() || c == '_' || c == '-' {
            let start = i;
            i += 1;
            while i < bytes.len() {
                let c = bytes[i] as char;
                if c == '\\' {
                    // `.\31 23` 这类转义整体保留
                    i += 2;
                } else if c.is_alphanumeric() || c == '_' || c == '-' {
                    i += 1;
                } else {
                    break;
                }
            }
            // 转义/多字节字符可能让 `i` 落在字符中间：退到最近的 char 边界
            let end = (start..=i.min(text.len()))
                .rev()
                .find(|&k| text.is_char_boundary(k))
                .unwrap_or(start);
            let token = &text[start..end];
            match token {
                "," | "&" => {}
                _ => tokens.push(token.to_string()),
            }
            continue;
        }
        i += 1;
    }
    tokens
}

/// 作用域 token：`&`-only 选择器自身没有 token，但仍需占一层作用域（内部定义不外泄）。
pub(super) fn scope_tokens(own: Vec<String>) -> Vec<String> {
    if own.is_empty() {
        vec!["&".to_string()]
    } else {
        own
    }
}

/// 规则的所有选择器原文（每个选择器整段保留，插值后才切 token）。
pub(super) fn rule_selector_raw_lists(sel: &SelectorList, src: &str) -> Vec<Vec<String>> {
    sel.selectors
        .iter()
        .filter_map(|s| src.get(s.span.start..s.span.end))
        .map(|t| vec![t.trim().to_string()])
        .filter(|t| !t[0].is_empty())
        .collect()
}

/// 规则自身的选择器 token（插值后，多个选择器取第一个）。
pub(super) fn rule_own_tokens<'a>(
    sel: &SelectorList,
    src: &'a str,
    scope: &dyn value::Scope<'a>,
) -> Vec<String> {
    sel.selectors
        .first()
        .and_then(|s| src.get(s.span.start..s.span.end))
        .map(|t| interp_tokens(&[t.trim().to_string()], src, scope))
        .unwrap_or_default()
}

/// 选择器是否只有一个 `&`（less `isJustParentSelector`：折进父规则）。
pub(super) fn is_just_parent(sel: &SelectorList, src: &str) -> bool {
    sel.selectors.len() == 1
        && sel
            .selectors
            .first()
            .and_then(|s| src.get(s.span.start..s.span.end))
            .map(|t| t.trim() == "&")
            .unwrap_or(false)
}

/// 选择器列表展开：插值得到的逗号串要拆成多个选择器（less `SelectorList` 逐个换行）。
pub(super) fn split_selectors(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut depth = 0usize;
    let mut quote: Option<char> = None;
    for (i, ch) in text.char_indices() {
        match quote {
            Some(q) if ch == q => quote = None,
            Some(_) => {}
            None => match ch {
                '"' | '\'' => quote = Some(ch),
                '(' | '[' => depth += 1,
                ')' | ']' => depth = depth.saturating_sub(1),
                ',' if depth == 0 => {
                    out.push(text[start..i].trim().to_string());
                    start = i + 1;
                }
                _ => {}
            },
        }
    }
    out.push(text[start..].trim().to_string());
    out.retain(|s| !s.is_empty());
    out
}

pub(super) fn stmt_start(st: &Statement) -> Option<usize> {
    match st {
        Statement::Declaration(d) => Some(d.span.start),
        Statement::QualifiedRule(r) => Some(r.span.start),
        Statement::AtRule(a) => Some(a.span.start),
        Statement::KeyframeBlock(k) => Some(k.span.start),
        Statement::LessVariableDeclaration(d) => Some(d.span.start),
        Statement::LessExtendRule(r) => Some(r.span.start),
        Statement::LessMixinCall(c) => Some(c.span.start),
        Statement::LessVariableCall(c) => Some(c.span.start),
        Statement::LessFunctionCall(f) => Some(f.span.start),
        Statement::LessConditionalQualifiedRule(r) => Some(r.span.start),
        _ => None,
    }
}

/// 父 × 子 组合：已由 `extend::combine` 完成（按 less `joinSelector`）。
/// 插值带出的逗号仍要拆成多条选择器。
pub(super) fn split_interpolated(sel: Sel) -> Vec<Sel> {
    let text = extend::render(&sel);
    match split_selectors(&text) {
        parts if parts.len() > 1 => parts.iter().map(|p| extend::parse(p)).collect(),
        _ => vec![sel],
    }
}

/// 组合子文本（与 less `Combinator.value` 一致）。
///
/// less 的 slashed combinator 是任意的 `/name/`（`/shadow/`），只能从源文本取。
pub(super) fn combinator_text(c: &Combinator, src: &str) -> String {
    if matches!(c.kind, CombinatorKind::Slashed) {
        return src.get(c.span.start..c.span.end).unwrap_or("/").to_string();
    }
    match c.kind {
        CombinatorKind::Descendant => " ".to_string(),
        CombinatorKind::NextSibling => "+".to_string(),
        CombinatorKind::Child => ">".to_string(),
        CombinatorKind::LaterSibling => "~".to_string(),
        CombinatorKind::Column => "||".to_string(),
        CombinatorKind::Deep => "/deep/".to_string(),
        CombinatorKind::Slashed => "/".to_string(),
        CombinatorKind::ShadowChild => "^".to_string(),
        CombinatorKind::ShadowDescendant => "^^".to_string(),
    }
}

/// 下一个元素该带的组合子（复合选择器内部元素无组合子）。
pub(super) fn element_combinator(
    pending: &mut Option<String>,
    first: &mut bool,
    selector_empty: bool,
) -> String {
    if !*first {
        return String::new();
    }
    *first = false;
    if selector_empty {
        pending.take().unwrap_or_default()
    } else {
        pending.take().unwrap_or_else(|| " ".to_string())
    }
}

impl<'a> Compiler<'a> {
    /// 选择器列表 → 展平后的元素序列（每条路径同时带出它的 `:extend(...)`）。
    pub(super) fn selector_sels(&mut self, sel: &SelectorList) -> Vec<(Sel, Vec<Need>, Span)> {
        let mut out = Vec::with_capacity(sel.selectors.len());
        for complex in &sel.selectors {
            let complex_span = complex.span;
            let mut elements: Sel = Vec::new();
            let mut needs: Vec<Need> = Vec::new();
            // 组合子等到下一个复合选择器落地（与 less 的 Element.combinator 同构）
            let mut pending: Option<String> = None;
            for child in &complex.children {
                match child {
                    ComplexSelectorChild::Combinator(c) => {
                        pending = Some(combinator_text(c, self.src))
                    }
                    ComplexSelectorChild::CompoundSelector(compound) => {
                        let mut first = true;
                        for simple in &compound.children {
                            if let SimpleSelector::PseudoClass(p) = simple
                                && let Some(arg) = &p.arg
                                && let PseudoClassSelectorArgKind::LessExtendList(list) = &arg.kind
                            {
                                needs.extend(self.needs_of(list));
                                continue;
                            }
                            if let SimpleSelector::Nesting(n) = simple {
                                // `&-suffix` 是「`&` 元素 + 后缀元素」，不能并成一个
                                let combinator = element_combinator(
                                    &mut pending,
                                    &mut first,
                                    elements.is_empty(),
                                );
                                elements.push(extend::Element {
                                    combinator,
                                    value: "&".to_string(),
                                });
                                if let Some(suffix) = &n.suffix {
                                    let suffix_span = *suffix.span();
                                    let text =
                                        self.interpolate_at(self.slice(suffix_span), suffix_span);
                                    if !text.is_empty() {
                                        let combinator = element_combinator(
                                            &mut pending,
                                            &mut first,
                                            elements.is_empty(),
                                        );
                                        elements.push(extend::Element {
                                            combinator,
                                            value: text,
                                        });
                                    }
                                }
                                continue;
                            }
                            let span = *simple.span();
                            let text = self.interpolate_at(self.slice(span), span);
                            // less 的 `Element` 对字符串值做 trim（转义序列末尾的空格会被去掉）
                            let text = text.trim();
                            if text.is_empty() {
                                continue;
                            }
                            let combinator =
                                element_combinator(&mut pending, &mut first, elements.is_empty());
                            elements.push(extend::Element {
                                combinator,
                                value: text.to_string(),
                            });
                        }
                    }
                }
            }
            out.push((elements, needs, complex_span));
        }
        out
    }

    /// `:extend(...)` 参数列表 → 匹配需求。
    pub(super) fn needs_of(&self, list: &LessExtendList<'_>) -> Vec<Need> {
        list.elements
            .iter()
            .map(|e| {
                let raw = self.slice(e.selector.span);
                let text = self.interpolate_at(raw, e.selector.span);
                Need {
                    target: extend::parse(&text),
                    all: e.all.is_some(),
                }
            })
            .collect()
    }
}
