use oxc_css_parser::ast::*;
use oxc_css_parser::pos::Span;

use super::Compiler;
use super::Layer;
use crate::doc::{Comment, Item};

/// Block spans of mixin definitions (including those nested in other mixin bodies), used to remove inner comments from the root level.
pub(super) fn collect_definition_spans(stmts: &[Statement], out: &mut Vec<Span>) {
    for st in stmts {
        match st {
            Statement::LessMixinDefinition(d) => {
                out.push(d.block.span);
                collect_definition_spans(&d.block.statements, out);
            }
            // Comments inside a detached ruleset body are emitted only when it is called (dropped for each)
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

/// Removes block comments from the text (for `@import` preludes: less moves comments outside the statement).
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

/// Block comment spans in the text (relative offsets).
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

/// Span of a rule header (selector/at-rule name up to `{`): comments here are dropped by less.
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
        // detached ruleset declaration: comments in the body belong to the expansion site, not the outer block
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
    /// Block comments in a mixin body (excluding nested blocks); re-emitted on every expansion (comments are ordinary nodes in less).
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

    /// Comments owned by this block: block comments inside `block_span` but not inside any child block.
    ///
    /// They are marked as emitted when collected, so the root-level flush skips them (avoiding duplicates).
    pub(super) fn own_comments(
        &mut self,
        block_span: Span,
        stmts: &'a [Statement<'a>],
    ) -> Vec<Comment> {
        let children: Vec<Span> = stmts.iter().filter_map(child_block_span).collect();
        // Comments in a rule header (selector/at-rule name up to `{`): less clears the
        // commentStore in `selectors()` / `atrule()` and drops them.
        let headers: Vec<Span> = stmts.iter().filter_map(child_header_span).collect();
        // Comments in a whole declaration (including variable declarations) belong to that statement: emitted in place in the value, dropped in the name part
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

    /// Block comment immediately following a value node (within the same statement): less treats comments as the last node of a value list.
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

    /// Marks block comments inside the span as emitted (dropped/inlined when they belong to no position).
    pub(super) fn mark_comments(&mut self, span: Span) {
        for (i, &(start, _)) in self.comments.iter().enumerate() {
            if start >= span.start && start < span.end {
                self.comment_done[i] = true;
            }
        }
    }

    /// Takes every unemitted block comment with start < limit (preserving source order).
    pub(super) fn take_comments(&mut self, limit: usize) -> Vec<Comment> {
        let mut out = Vec::new();
        while self.comment_idx < self.comments.len() {
            let i = self.comment_idx;
            let (start, end) = self.comments[i];
            if start >= limit {
                break;
            }
            self.comment_idx += 1;
            // Comments inside definition bodies are emitted only at the expansion site (they must not surface from the root level)
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

    /// Writes out every unemitted block comment with start < limit (top level / between rules).
    pub(super) fn flush_comments(&mut self, limit: usize, layer: &mut Layer) {
        for comment in self.take_comments(limit) {
            layer.items.push(Item::Comment(comment));
        }
    }

    /// IR form of a block comment: the text with `/*` and `*/` removed.
    ///
    /// The less side writes it back verbatim per [`Style::verbatim_comments`](crate::doc::Style), so `text`
    /// is the raw source bytes (including internal newlines); only the dart side treats it as inner text to reflow.
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
