use oxc_css_parser::ast::*;
use oxc_css_parser::pos::Span;

use super::Compiler;
use super::Layer;
use crate::doc::{Comment, Item};

/// mixin 定义（含嵌套在别的 mixin 体里的）的块区间，用于把内部注释从根层摘除。
pub(super) fn collect_definition_spans(stmts: &[Statement], out: &mut Vec<Span>) {
    for st in stmts {
        match st {
            Statement::LessMixinDefinition(d) => {
                out.push(d.block.span);
                collect_definition_spans(&d.block.statements, out);
            }
            // detached ruleset 体内注释只在被调用时输出（each 则丢掉）
            Statement::LessVariableDeclaration(d) => {
                if let ComponentValue::LessDetachedRuleset(dr) = &d.value {
                    out.push(dr.block.span);
                    collect_definition_spans(&dr.block.statements, out);
                }
            }
            Statement::QualifiedRule(r) => collect_definition_spans(&r.block.statements, out),
            Statement::LessConditionalQualifiedRule(r) => {
                collect_definition_spans(&r.block.statements, out);
            }
            Statement::AtRule(a) => {
                if let Some(block) = &a.block {
                    collect_definition_spans(&block.statements, out);
                }
            }
            _ => {}
        }
    }
}

/// 去掉文本里的块注释（用于 `@import` prelude：注释会被 less 移到语句外）。
pub(super) fn strip_block_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    for (start, end) in block_comments(text) {
        out.push_str(&text[cursor..start]);
        cursor = end;
    }
    out.push_str(&text[cursor.min(text.len())..]);
    out.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace(" ,", ",")
}

/// 文本里的块注释区间（相对偏移）。
pub(super) fn block_comments(text: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'/' && bytes[i + 1] == b'*' {
            match text[i + 2..].find("*/") {
                Some(j) => {
                    out.push((i, i + 2 + j + 2));
                    i += 2 + j + 2;
                }
                None => break,
            }
        } else {
            i += 1;
        }
    }
    out
}

/// 规则头部（选择器/at-rule 名到 `{`）的区间：这里的注释被 less 丢掉。
pub(super) fn child_header_span(st: &Statement) -> Option<Span> {
    match st {
        Statement::QualifiedRule(r) => Some(Span {
            start: r.span.start,
            end: r.block.span.start,
        }),
        Statement::LessConditionalQualifiedRule(r) => Some(Span {
            start: r.span.start,
            end: r.block.span.start,
        }),
        Statement::AtRule(a) => Some(Span {
            start: a.name.span().start,
            end: a.block.as_ref().map_or(a.span.end, |b| b.span.start),
        }),
        _ => None,
    }
}

pub(super) fn child_block_span(st: &Statement) -> Option<Span> {
    match st {
        Statement::QualifiedRule(r) => Some(r.block.span),
        Statement::LessConditionalQualifiedRule(r) => Some(r.block.span),
        Statement::LessMixinDefinition(d) => Some(d.block.span),
        // detached ruleset 声明：体内的注释属于展开处，不属于外层块
        Statement::LessVariableDeclaration(d) => match &d.value {
            ComponentValue::LessDetachedRuleset(dr) => Some(dr.block.span),
            _ => None,
        },
        Statement::AtRule(a) => a.block.as_ref().map(|b| b.span),
        Statement::KeyframeBlock(k) => Some(k.block.span),
        _ => None,
    }
}

impl<'a> Compiler<'a> {
    /// mixin 体内的块注释（不含子块内部）；每次展开都重新输出（less 里注释是普通节点）。
    pub(super) fn mixin_comments(&mut self, block: &'a SimpleBlock<'a>) -> Vec<Comment> {
        let children: Vec<Span> = block
            .statements
            .iter()
            .filter_map(child_block_span)
            .collect();
        let mut out = Vec::new();
        for (i, &(start, end)) in self.comments.iter().enumerate() {
            if start < block.span.start
                || start >= block.span.end
                || children.iter().any(|c| c.start <= start && start < c.end)
            {
                continue;
            }
            self.comment_done[i] = true;
            out.push(self.comment(start, end));
        }
        out
    }

    /// 本块归属的注释：落在 `block_span` 内、但不属于任何子块内部的块注释。
    ///
    /// 收集时即标记为已输出，根层 flush 会跳过它们（避免重复）。
    pub(super) fn own_comments(
        &mut self,
        block_span: Span,
        stmts: &'a [Statement<'a>],
    ) -> Vec<Comment> {
        let children: Vec<Span> = stmts.iter().filter_map(child_block_span).collect();
        // 规则头部（选择器/at-rule 名到 `{`）里的注释：less 在 `selectors()` / `atrule()`
        // 里清空 commentStore，直接丢掉。
        let headers: Vec<Span> = stmts.iter().filter_map(child_header_span).collect();
        // 声明（含变量声明）整段里的注释属于该语句：值里的原位输出，名字段里的丢掉
        let values: Vec<Span> = stmts
            .iter()
            .filter_map(|st| {
                let (start, from) = match st {
                    Statement::Declaration(d) => (d.span.start, d.colon_span.end),
                    Statement::LessVariableDeclaration(d) => (d.span.start, d.colon_span.end),
                    _ => return None,
                };
                let (_, end, _) = self.region_from(from)?;
                Some(Span { start, end })
            })
            .collect();
        let mut out = Vec::new();
        for (i, &(start, end)) in self.comments.iter().enumerate() {
            if self.comment_done[i]
                || start < block_span.start
                || start >= block_span.end
                || children.iter().any(|c| c.start <= start && start < c.end)
                || headers.iter().any(|h| h.start <= start && start < h.end)
                || values.iter().any(|v| v.start <= start && start < v.end)
            {
                continue;
            }
            self.comment_done[i] = true;
            out.push(self.comment(start, end));
        }
        out
    }

    /// 值节点之后紧跟的块注释（同一语句内）：less 把注释当值列表的最后一个节点。
    pub(super) fn trailing_comment(&self, end: usize) -> Option<String> {
        let rest = self.src.get(end..)?;
        let start = rest
            .char_indices()
            .find(|(_, c)| !matches!(c, ' ' | '\t' | '\r' | '\n'))
            .map(|(i, _)| i)?;
        let tail = &rest[start..];
        let close = tail.strip_prefix("/*")?.find("*/")? + 4;
        Some(tail[..close].to_string())
    }

    /// 把区间内的块注释标为已输出（不归属任何位置时的丢弃/内联）。
    pub(super) fn mark_comments(&mut self, span: Span) {
        for (i, &(start, _)) in self.comments.iter().enumerate() {
            if start >= span.start && start < span.end {
                self.comment_done[i] = true;
            }
        }
    }

    /// 取走所有 start < limit 的未输出块注释（保持源序）。
    pub(super) fn take_comments(&mut self, limit: usize) -> Vec<Comment> {
        let mut out = Vec::new();
        while self.comment_idx < self.comments.len() {
            let i = self.comment_idx;
            let (start, end) = self.comments[i];
            if start >= limit {
                break;
            }
            self.comment_idx += 1;
            // 定义体里的注释只在展开处输出（不能从根层冒出来）
            if self.comment_done[i]
                || self
                    .def_spans
                    .iter()
                    .any(|s| s.start <= start && start < s.end)
            {
                continue;
            }
            self.comment_done[i] = true;
            out.push(self.comment(start, end));
        }
        out
    }

    /// 直接写出所有 start < limit 的未输出块注释（顶层/规则间）。
    pub(super) fn flush_comments(&mut self, limit: usize, layer: &mut Layer) {
        for comment in self.take_comments(limit) {
            layer.items.push(Item::Comment(comment));
        }
    }

    /// 一条块注释的 IR 形态：文本去掉 `/*` `*/`。
    ///
    /// less 侧按 [`Style::verbatim_comments`](crate::doc::Style) 原样回写，所以 `text`
    /// 就是源码字节（含内部换行）；dart 侧才把它当待重排的内部文本。
    pub(super) fn comment(&self, start: usize, end: usize) -> Comment {
        let column = self.src[..start]
            .rfind('\n')
            .map_or(start, |i| start - i - 1);
        Comment {
            text: self.src[start + 2..end.saturating_sub(2)].to_string(),
            span: Span { start, end },
            column,
        }
    }
}
