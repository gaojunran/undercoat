use std::collections::HashMap;

use oxc_css_parser::ast::*;
use oxc_css_parser::pos::Span;

use super::comments::strip_block_comments;
use super::env::Inject;
use super::eval::Out;
use super::extend::{self, Need, Sel};
use super::imports::{is_css_path, split_import_options, split_quoted_path};
use super::prelude::{
    at_rule_keeps_parents, is_hoisted_import, is_import, is_reference, is_remote,
    normalize_prelude, strip_escapes,
};
use super::selector::{
    is_just_parent, scope_tokens, selector_tokens, split_interpolated, split_selectors, stmt_start,
};
use super::value::{self, PropSource, eval};
use super::{Child, Compiler, Head, Layer, strip_embedded_important};
use crate::doc::{AtRule as OutAtRule, Body, Comment, Decl, DeclItem, Item, Keyframe, Raw};
use crate::{CompileError, Options};

/// An emittable declaration: carries the mixin injection scope and the forced `!important` flag.
#[derive(Clone)]
pub(super) struct Emit<'a> {
    pub(super) decl: &'a Declaration<'a>,
    pub(super) inject: Option<Inject<'a>>,
    pub(super) important: bool,
}

/// A child node deferred to pass 4 (a normal child rule or a child rule inside a mixin body).
pub(super) struct ChildItem<'a> {
    /// Start of the statement that triggered it (for comment attribution)
    pub(super) at: usize,
    pub(super) child: Child<'a>,
    pub(super) inject: Option<Inject<'a>>,
    pub(super) important: bool,
}

/// A nested media deferred inside a media block.
pub(super) struct Deferred<'a> {
    pub(super) at: &'a AtRule<'a>,
    pub(super) sels: Vec<Sel>,
    pub(super) spans: Vec<Span>,
    pub(super) level: usize,
    /// Full condition of the outer media (for merging)
    pub(super) outer: String,
    /// Scope still needed after bubbling (the parameter frame of `@ruleset()` is still alive when output is deferred)
    pub(super) inject: Option<Inject<'a>>,
}

/// Output sink and scope for `collect_block`.
pub(super) struct Sink<'a, 'c> {
    pub(super) pieces: &'c mut Vec<Piece<'a>>,
    pub(super) children: &'c mut Vec<ChildItem<'a>>,
    /// Local scope of a `&`-folded block (`None` for ordinary blocks)
    pub(super) inject: Option<Inject<'a>>,
    /// This block's `&:extend(...)` / `:extend(...)`
    pub(super) needs: &'c mut Vec<Need>,
}

pub(super) enum Piece<'a> {
    /// Block comment (IR form: the text with `/*` and `*/` removed)
    Comment(Comment),
    Decl(Emit<'a>),
    /// A single declaration after property merging (`+:` / `+_:`), occupying the position of the first declaration with the same name
    Merged(Vec<Emit<'a>>),
    /// A blockless at-rule inside a rule body (`@apply …;`): less treats it as a node of this level
    AtRule(Span),
}

/// Merge group splitting: `+:` starts a new group (comma), and items within a group are joined by spaces. Returns index groups.
pub(super) fn merge_groups(decls: &[&Declaration]) -> Vec<Vec<usize>> {
    let mut groups: Vec<Vec<usize>> = Vec::new();
    let mut space: Vec<usize> = Vec::new();
    for (i, d) in decls.iter().enumerate() {
        let comma = matches!(
            d.less_property_merge.as_ref().map(|m| &m.kind),
            Some(LessPropertyMergeKind::Comma)
        );
        if comma && !space.is_empty() {
            groups.push(std::mem::take(&mut space));
        }
        space.push(i);
    }
    groups.push(space);
    groups
}

/// Spans of child blocks (emitted recursively), used to attribute comments to the innermost block.
/// Whether the node tree contains a namespace lookup (decides whether an operation goes through text evaluation).
pub(super) fn has_lookup(v: &ComponentValue) -> bool {
    match v {
        ComponentValue::LessNamespaceValue(_) => true,
        ComponentValue::LessBinaryOperation(op) => has_lookup(&op.left) || has_lookup(&op.right),
        ComponentValue::LessParenthesizedOperation(p) => has_lookup(&p.operation),
        _ => false,
    }
}

/// Character set of less `anonymousValue()`: on a hit the whole value is emitted as source text (no parsing, no evaluation).
pub(super) fn anonymous_value(raw: &str) -> bool {
    !raw.chars().any(|c| {
        matches!(
            c,
            '.' | '#'
                | '@'
                | '$'
                | '+'
                | '/'
                | '\''
                | '"'
                | '*'
                | '`'
                | '('
                | ';'
                | '{'
                | '}'
                | '-'
        )
    })
}

/// Comments in an `@media` prelude attach to the next value: insert ` /*…*/` after the following `:`.
/// Returns the leftover comments for which no `:` was found (the caller moves them into the body).
pub(super) fn attach_to_feature_values(
    text: &str,
    pending: &[(usize, Comment)],
) -> (String, Vec<Comment>) {
    let mut out = text.to_string();
    let mut shift = 0isize;
    let mut leftover = Vec::new();
    for (pos, comment) in pending {
        let from = (*pos as isize + shift).max(0) as usize;
        match out.get(from..).and_then(|rest| rest.find(':')) {
            Some(rel) => {
                let at = from + rel + 1;
                let inserted = format!(" /*{}*/ ", comment.text);
                out.insert_str(at, &inserted);
                shift += inserted.len() as isize;
            }
            None => leftover.push(comment.clone()),
        }
    }
    (out, leftover)
}

/// The wrapper at-rule for a `(reference)` import body.
/// The text after `@name` in a blockless at-rule (the prelude).
pub(super) fn blockless_rest<'s>(at: &AtRule, src: &'s str) -> &'s str {
    let start = at.name.span().end;
    let end = at.span.end.max(start).min(src.len());
    src.get(start..end).unwrap_or("").trim()
}

/// Names of `$prop` references in a value (`$color` → `color`).
pub(super) fn collect_prop_vars<'a>(cv: &ComponentValue<'a>, out: &mut Vec<String>) {
    match cv {
        ComponentValue::LessPropertyVariable(p) => out.push(p.name.name.to_string()),
        ComponentValue::LessBinaryOperation(op) => {
            collect_prop_vars(&op.left, out);
            collect_prop_vars(&op.right, out);
        }
        ComponentValue::LessParenthesizedOperation(p) => collect_prop_vars(&p.operation, out),
        ComponentValue::Function(f) => {
            for a in &f.args {
                collect_prop_vars(a, out);
            }
        }
        _ => {}
    }
}

impl<'a> Compiler<'a> {
    /// A bare statement of a blockless at-rule (`@nope;` / `@nope [foo];`): less treats it as a variable call,
    /// reports an error at the root level, and produces no CSS (it used to emit a raw `@nope;` line).
    pub(super) fn bare_at_rule(&mut self, at: &'a AtRule, wrapped: bool) {
        let rest = blockless_rest(at, self.src);
        if rest.is_empty() {
            self.env.miss(
                "Missing '[...]' lookup in variable call".to_string(),
                at.span,
            );
            return;
        }
        if wrapped {
            return;
        }
        let name = at.name.name;
        let message = match self.var_ruleset(name, 0) {
            // The variable is a detached ruleset: lookup yields a declaration, which is not allowed at the root level
            Some(_) => {
                "Properties must be inside selector blocks. They cannot be in the root".to_string()
            }
            None if self.env.get(name).is_none() => format!("variable @{name} is undefined"),
            None => format!("Could not evaluate variable call @{name}"),
        };
        self.env.miss(message, at.span);
    }

    /// Whether a root-level `@import` is hoisted as-is: the prelude must be interpolated in the child's injection scope before the test
    /// (`@import "@{x}"` inside a mixin such as `.add_an_import("file.css")`).
    pub(super) fn hoists(&self, at: &AtRule, inject: &Option<Inject<'a>>) -> bool {
        if !is_import(at) {
            return false;
        }
        let Some(raw) = at
            .prelude
            .as_ref()
            .and_then(|p| self.src.get(p.span().start..p.span().end))
        else {
            return false;
        };
        let text = match inject {
            Some(inject) => {
                let env = self.env.scoped(inject);
                value::interpolate(raw, self.src, &env, 0, None)
            }
            None => self.interpolate(raw),
        };
        is_hoisted_import(&text)
    }

    /// Emits an at-rule child (with the mixin injection scope and `!important` state).
    pub(super) fn emit_child_at(
        &mut self,
        at: &'a AtRule,
        child: &ChildItem<'a>,
        level: usize,
        parents: &[Sel],
        parent_spans: &[Span],
        layer: &mut Layer,
    ) -> Result<(), Box<CompileError>> {
        let saved = self.force_important;
        self.force_important |= child.important;
        self.env.push_scope(&child.inject);
        let result = self.emit_at_rule(
            at,
            parents,
            parent_spans,
            level,
            child.inject.clone(),
            layer,
        );
        self.env.pop_scope(&child.inject);
        self.force_important = saved;
        result
    }

    /// Expands a child rule (selector interpolation must be evaluated in the caller's scope).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn emit_child_rule(
        &mut self,
        sel: &SelectorList,
        block: &'a SimpleBlock<'a>,
        inject: Option<Inject<'a>>,
        important: bool,
        parents: &[Sel],
        parent_spans: &[Span],
        level: usize,
        layer: &mut Layer,
    ) -> Result<(), Box<CompileError>> {
        let saved = self.force_important;
        let saved_path = self.path.len();
        self.force_important |= important;
        // Block comments inside selectors: less clears the commentStore in `selectors()` and drops them
        self.mark_comments(sel.span);
        self.env.push_scope(&inject);
        let own = self.selector_sels(sel);
        let mut combined: Vec<Sel> = Vec::new();
        let mut combined_spans: Vec<Span> = Vec::new();
        let mut extends: Vec<Vec<Need>> = Vec::new();
        for (child, needs, child_span) in &own {
            // Commas introduced by interpolation are split at the text level first, then combined with parent selectors (less interpolation is text-level substitution):
            // `.d@{.a,.b,.c}&:hover` → `.d.a`, `.b`, `.c&:hover`, each later joined with the parent selectors.
            let text = extend::render(child);
            let parts = split_selectors(&text);
            let pieces = match parts.len() {
                0 => vec![child.clone()],
                1 => vec![child.clone()],
                _ => parts.iter().map(|p| extend::parse(p)).collect(),
            };
            for piece in pieces {
                // For mapping, the source span comes from the outermost selector of the path: a nested rule's first mapping lands on the parent rule's line
                // (matching less's `Selector.elements[0].span`).
                let mut push = |product: Sel, span: Span| {
                    for sel in split_interpolated(product) {
                        combined.push(sel);
                        combined_spans.push(span);
                        extends.push(needs.clone());
                    }
                };
                let products = extend::combine(parents, &piece);
                // Mapping takes source spans along the parent path (in the same order as combine's parent loop);
                // with a single file or no parent selectors the child selector span is kept, matching the previous map bytes.
                let spans: Vec<Span> = {
                    let root_span = |idx: usize| match self.multi_source {
                        true if !parents.is_empty() && products.len() == parents.len() => {
                            parent_spans.get(idx).copied().unwrap_or(*child_span)
                        }
                        true => parent_spans.first().copied().unwrap_or(*child_span),
                        false => *child_span,
                    };
                    (0..products.len()).map(root_span).collect()
                };
                for (product, span) in products.into_iter().zip(spans) {
                    push(product, span);
                }
            }
        }
        let tokens = own
            .first()
            .map(|(sel, _, _)| selector_tokens(&extend::render(sel)))
            .map(scope_tokens)
            .unwrap_or_default();
        self.path_linkable
            .extend(std::iter::repeat_n(true, tokens.len()));
        self.path.extend(tokens);
        self.rule_bodies.push(block);
        self.emit_block(
            &block.statements,
            Head {
                sels: &combined,
                spans: &combined_spans,
                extends,
                root: false,
            },
            level,
            false,
            block.span,
            layer,
        )?;
        self.rule_bodies.pop();
        self.path.truncate(saved_path);
        self.path_linkable.truncate(saved_path);
        self.env.pop_scope(&inject);
        self.force_important = saved;
        Ok(())
    }

    /// Collects one block level: pass 1 registers variables, pass 2 collects declarations and child rules (in source order).
    pub(super) fn collect_block(
        &mut self,
        stmts: &'a [Statement<'a>],
        block_span: Span,
        wrapped: bool,
        sink: &mut Sink<'a, '_>,
    ) {
        let Sink {
            pieces,
            children,
            inject,
            needs,
        } = sink;
        for st in stmts {
            match st {
                Statement::LessVariableDeclaration(d) => {
                    self.note_var(d);
                }
                // Variables in a (reference) body remain visible as usual
                Statement::AtRule(a) if is_reference(a) => {
                    for inner in a.block.iter().flat_map(|b| b.statements.iter()) {
                        if let Statement::LessVariableDeclaration(d) = inner {
                            self.note_var(d);
                        }
                    }
                }
                _ => {}
            }
        }
        // less's `Variable.eval` evaluates every variable declaration's value (even when unreferenced),
        // and undefined references in the value are reported too; by then the variables of the same scope are all collected (lazy evaluation still holds).
        for st in stmts {
            if let Statement::LessVariableDeclaration(d) = st {
                self.eval_var_value(d);
            }
        }
        let own = if wrapped {
            self.own_comments(block_span, stmts)
        } else {
            Vec::new()
        };
        let mut next = 0;
        for st in stmts {
            let start = stmt_start(st).unwrap_or(usize::MAX);
            while next < own.len() && own[next].span.start < start {
                pieces.push(Piece::Comment(own[next].clone()));
                next += 1;
            }
            match st {
                Statement::Declaration(d) => {
                    // less reports "properties in root" for root-level declarations; this stays lenient, but
                    // undefined interpolation in the property name is still reported in less's evaluation order.
                    if !wrapped {
                        let span = *d.name.span();
                        self.interpolate_at(self.slice(span), span);
                    }
                    pieces.push(Piece::Decl(Emit {
                        decl: d,
                        inject: inject.clone(),
                        important: false,
                    }))
                }
                Statement::LessMixinCall(call) => {
                    let mut out = Out {
                        pieces,
                        children,
                        needs,
                    };
                    self.expand_mixin(call, start, false, &mut out, 0);
                }
                // `@dr();`: detached ruleset call
                Statement::LessVariableCall(call) => {
                    let mut out = Out {
                        pieces,
                        children,
                        needs,
                    };
                    self.expand_ruleset_call(call, start, &mut out);
                }
                // `each(list, ruleset)` is a statement-level function: it must expand into rules
                Statement::LessFunctionCall(f)
                    if value::function_name(f).eq_ignore_ascii_case("each") =>
                {
                    let mut sink = Sink {
                        pieces,
                        children,
                        inject: inject.clone(),
                        needs,
                    };
                    self.expand_each(f, &mut sink);
                }
                // A root-level bare function call (`e('text');`): less emits it as an ordinary node
                Statement::LessFunctionCall(f) if !wrapped => children.push(ChildItem {
                    at: start,
                    child: Child::Other(f),
                    inject: inject.clone(),
                    important: false,
                }),
                Statement::QualifiedRule(rule) => {
                    // A nested rule with a single `&` is folded into the parent rule (less `isJustParentSelector`)
                    if is_just_parent(&rule.selector, self.src) {
                        let mut out = Out {
                            pieces,
                            children,
                            needs,
                        };
                        self.inline_ruleset(&rule.block, &mut out);
                    } else {
                        children.push(ChildItem {
                            at: start,
                            child: Child::Rule(rule),
                            inject: inject.clone(),
                            important: false,
                        });
                    }
                }
                Statement::LessConditionalQualifiedRule(rule) => {
                    if !self.rule_guard_passes(&rule.guard) {
                        continue;
                    }
                    if is_just_parent(&rule.selector, self.src) {
                        let mut out = Out {
                            pieces,
                            children,
                            needs,
                        };
                        self.inline_ruleset(&rule.block, &mut out);
                    } else {
                        children.push(ChildItem {
                            at: start,
                            child: Child::CondRule(rule),
                            inject: inject.clone(),
                            important: false,
                        });
                    }
                }
                Statement::KeyframeBlock(k) => children.push(ChildItem {
                    at: start,
                    child: Child::Keyframe(k),
                    inject: inject.clone(),
                    important: false,
                }),
                // (reference) import body: rules are registered invisible and emitted/dropped at the end depending on extend hits
                Statement::AtRule(at) if is_reference(at) => {
                    if let Some(block) = &at.block {
                        children.push(ChildItem {
                            at: start,
                            child: Child::Ref(block),
                            inject: inject.clone(),
                            important: false,
                        });
                    }
                }
                // `@plugin` only loads plugins during parsing and produces no output (consumed by less's plugin manager)
                Statement::AtRule(at) if at.name.name.eq_ignore_ascii_case("plugin") => {}
                // Bare identifier/variable statement: less evaluates it as a variable call, reports an error at the root level, and produces no CSS
                Statement::AtRule(at)
                    if at.block.is_none()
                        && (blockless_rest(at, self.src).is_empty()
                            || !wrapped && blockless_rest(at, self.src).starts_with('[')) =>
                {
                    self.bare_at_rule(at, wrapped);
                }
                // A blockless at-rule inside a rule body (`@apply x;`): less treats it as a node of this level and keeps it in the block
                Statement::AtRule(at) if wrapped && at.block.is_none() => {
                    pieces.push(Piece::AtRule(Span {
                        start: at.span.start,
                        end: at.span.end,
                    }))
                }
                Statement::AtRule(at) => children.push(ChildItem {
                    at: start,
                    child: Child::AtRule(at),
                    inject: inject.clone(),
                    important: false,
                }),
                Statement::LessExtendRule(rule) => needs.extend(self.needs_of(&rule.extend)),
                _ => {}
            }
        }
        while next < own.len() {
            pieces.push(Piece::Comment(own[next].clone()));
            next += 1;
        }
    }

    /// Turns a piece into a node in the declaration body (shared by `Raw` placement and compact output).
    pub(super) fn piece_item(&mut self, piece: &Piece<'a>) -> DeclItem {
        match piece {
            Piece::Comment(c) => DeclItem::Comment(c.clone()),
            Piece::Decl(e) => {
                self.env.push_scope(&e.inject);
                let decl = self.emit_declaration(e.decl, e.important);
                self.env.pop_scope(&e.inject);
                DeclItem::Decl(decl)
            }
            Piece::Merged(decls) => DeclItem::Decl(self.emit_merged(decls)),
            Piece::AtRule(span) => DeclItem::AtRule(self.blockless_at(*span)),
        }
    }

    /// The declaration sequence of a rule body.
    ///
    /// A `(reference)` body is the only exception: declaration nodes have `isVisible()` undefined, so less's
    /// `ruleset.genCSS` inserts no newlines (`a: 1;b: 2;`), while `Body::Decls` always prints one per line;
    /// the whole section therefore becomes a `Raw` (declarations are still evaluated, only per-item mapping is lost).
    pub(super) fn piece_body(&mut self, pieces: &[Piece<'a>]) -> Body {
        let items = pieces.iter().map(|piece| self.piece_item(piece)).collect();
        if !self.hidden {
            return Body::Decls(items);
        }
        let mut text = String::new();
        let mut marks = Vec::new();
        for item in &items {
            match item {
                DeclItem::Comment(c) => text.push_str(&format!("/*{}*/", c.text)),
                DeclItem::Decl(decl) => {
                    marks.push((text.len(), decl.name_span));
                    text.push_str(&super::decl_text(item));
                }
                DeclItem::AtRule(_) => text.push_str(&super::decl_text(item)),
            }
        }
        Body::Rules(vec![Item::Raw(Raw { text, marks })])
    }

    /// A blockless at-rule inside a rule body (`@apply x;`): less echoes it verbatim, only splitting out the name and prelude.
    fn blockless_at(&self, span: Span) -> OutAtRule {
        let text = self.interpolate_at(self.slice(span).trim(), span);
        let body = text.strip_prefix('@').unwrap_or(&text);
        let (name, prelude) = match body.find(char::is_whitespace) {
            Some(i) => (
                body[..i].to_string(),
                body[i..]
                    .trim_start_matches(char::is_whitespace)
                    .to_string(),
            ),
            None => (body.to_string(), String::new()),
        };
        OutAtRule {
            name_span: Span {
                start: span.start + 1,
                end: span.start + 1 + name.len(),
            },
            name,
            prelude,
            body: None,
            group_end: false,
            hoist: false,
        }
    }

    /// A single declaration after merging same-named properties: `+:` separates with commas and `+_:` with spaces (less `_mergeRules`).
    pub(super) fn emit_merged(&mut self, items: &[Emit<'a>]) -> Decl {
        let refs: Vec<&Declaration<'a>> = items.iter().map(|e| e.decl).collect();
        self.env.push_scope(&items[0].inject);
        let name = self.prop_name_at(refs[0]);
        self.env.pop_scope(&items[0].inject);
        let mut value = String::new();
        let mut embedded = false;
        for (gi, group) in merge_groups(&refs).iter().enumerate() {
            if gi > 0 {
                value.push_str(", ");
            }
            for (di, &idx) in group.iter().enumerate() {
                let e = &items[idx];
                if di > 0 {
                    value.push(' ');
                }
                self.env.push_scope(&e.inject);
                embedded |= self.emit_value_list(&e.decl.value, &mut value);
                self.env.pop_scope(&e.inject);
            }
        }
        // less `_mergeRules`: `!important` is hoisted to the end of the merged declaration (once only)
        if embedded
            || items
                .iter()
                .any(|e| e.decl.important.is_some() || e.important)
            || self.force_important
        {
            value.push_str(" !important");
        }
        Decl {
            name,
            name_span: *refs[0].name.span(),
            value_span: refs[0].value.first().map_or(refs[0].span, |v| *v.span()),
            value,
            custom: false,
            custom_strip: None,
        }
    }

    /// A single declaration: the property name carries a source anchor and the value is one text block (tokens in the value are no longer mapped individually).
    pub(super) fn emit_declaration(
        &mut self,
        d: &'a Declaration<'a>,
        force_important: bool,
    ) -> Decl {
        let name = self.prop_name_at(d);
        let name_span = *d.name.span();
        let value_span = d.value.first().map_or(d.span, |v| *v.span());
        let forced = force_important || self.force_important;
        let region = self.decl_value_region(d);
        // Comments between the property name and the first value token: less clears the commentStore after `ruleProperty()` and drops them
        let value_start = d.value.first().map_or(d.span.end, |v| v.span().start);
        self.mark_comments(Span {
            start: d.span.start,
            end: value_start,
        });
        // Comments in the value are emitted in place by `emit_values_in` (or dropped as less does) and are all marked as handled
        if let Some((start, end, _)) = region {
            self.mark_comments(Span { start, end });
        }
        let mut value = String::new();
        // less `anonymousValue()`: when the value has none of `. # @ $ + / ' " * ` ( ; { } -`, the whole text is emitted as-is
        if let Some((start, end, true)) = region {
            let raw = self.src[start..end].trim_start();
            if anonymous_value(raw) {
                // A declaration re-parsed by `$prop`: `!important` lost its leading space (less's in-place mutation)
                let text = match self.reparsed.contains(&d.span.start) {
                    true => raw.trim_end().strip_suffix("!important").map_or_else(
                        || raw.to_string(),
                        |body| format!("{}!important", body.trim_end()),
                    ),
                    false => raw.to_string(),
                };
                value.push_str(&text);
                if forced && !text.trim_end().ends_with("!important") {
                    value.push_str(" !important");
                }
                return Decl {
                    name,
                    name_span,
                    value,
                    value_span,
                    custom: false,
                    custom_strip: None,
                };
            }
        }
        let embedded = self.emit_values_in(&d.value, region, &mut value);
        let important = d.important.is_some() || forced || embedded;
        if important {
            // A declaration re-parsed by `$prop` lost its leading space (less's in-place mutation)
            if self.reparsed.contains(&d.span.start) {
                value.push_str("!important");
            } else {
                value.push_str(" !important");
            }
        }
        Decl {
            name,
            name_span,
            value,
            value_span,
            custom: false,
            custom_strip: None,
        }
    }

    /// The declaration's value region: after `:` up to the statement terminator (`;` / `}` / `{` / EOF).
    /// Returns `(start, terminator position, whether it ends with `;`)`.
    pub(super) fn decl_value_region(&self, d: &Declaration) -> Option<(usize, usize, bool)> {
        self.region_from(d.colon_span.end)
    }

    /// Scans from `start` to this statement's terminator (skipping quoted/comment/bracketed content).
    pub(super) fn region_from(&self, start: usize) -> Option<(usize, usize, bool)> {
        if start > self.src.len() {
            return None;
        }
        let bytes = self.src.as_bytes();
        let mut i = start;
        let mut depth = 0usize;
        let mut quote = 0u8;
        while i < bytes.len() {
            let c = bytes[i];
            if quote != 0 {
                if c == b'\\' {
                    i += 2;
                    continue;
                }
                if c == quote {
                    quote = 0;
                }
                i += 1;
                continue;
            }
            match c {
                b'"' | b'\'' => quote = c,
                b'/' if bytes.get(i + 1) == Some(&b'*') => {
                    i = self.src[i + 2..]
                        .find("*/")
                        .map_or(bytes.len(), |j| i + 2 + j + 2);
                    continue;
                }
                b'/' if bytes.get(i + 1) == Some(&b'/') => {
                    i = self.src[i..].find('\n').map_or(bytes.len(), |j| i + j);
                    continue;
                }
                b'(' | b'[' => depth += 1,
                b')' | b']' => depth = depth.saturating_sub(1),
                b';' | b'}' | b'{' if depth == 0 => {
                    return Some((start, i, c == b';'));
                }
                _ => {}
            }
            i += 1;
        }
        Some((start, bytes.len(), false))
    }

    /// Value list: evaluated segment by segment; whitespace between segments is normalized to a single space as less `Expression` does,
    /// commas are normalized to `, `, and the source text is kept around `/` (less's `Operation` preserves isSpaced).
    ///
    /// When a block comment sits between segments it stays in place (less treats comments as nodes of the value list).
    ///
    /// Returns whether `!important` was found in the value (coming from a variable value) and strips it from the text —
    /// less hoists the whole declaration's `!important` to the end and emits it once.
    pub(super) fn emit_value_list(
        &mut self,
        values: &'a [ComponentValue<'a>],
        out: &mut String,
    ) -> bool {
        self.emit_values_in(values, None, out)
    }

    /// `emit_value_list` with a value region: there may still be comments between the last item and the terminator.
    pub(super) fn emit_values_in(
        &mut self,
        values: &'a [ComponentValue<'a>],
        region: Option<(usize, usize, bool)>,
        out: &mut String,
    ) -> bool {
        let mut prev_end: Option<usize> = None;
        let mut after_solidus = false;
        let mut embedded = false;
        for v in values {
            let span = *v.span();
            let comma =
                matches!(v, ComponentValue::Delimiter(d) if matches!(d.kind, DelimiterKind::Comma));
            let solidus = matches!(
                v,
                ComponentValue::Delimiter(d) if matches!(d.kind, DelimiterKind::Solidus)
            );
            if let Some(end) = prev_end {
                if end <= span.start {
                    let raw = self.slice(Span {
                        start: end,
                        end: span.start,
                    });
                    let gap = if raw.contains("/*") {
                        // less treats comments as nodes of the value list: keep one space on each side
                        let body = raw.split_whitespace().collect::<Vec<_>>().join(" ");
                        if comma {
                            String::new()
                        } else if solidus || after_solidus {
                            raw.to_string()
                        } else {
                            format!(" {body} ")
                        }
                    } else if comma {
                        String::new()
                    } else if solidus || after_solidus {
                        raw.to_string()
                    } else {
                        " ".to_string()
                    };
                    out.push_str(&gap);
                }
            }
            // `10px / 2 * 2`: oxc splits `/` off as a delimiter, and the right segment is part of a division chain in less
            // (emitted verbatim when math is off), so substitute the source text instead of evaluating.
            let text = match after_solidus {
                true if matches!(v, ComponentValue::LessBinaryOperation(_)) => {
                    let raw = self.slice(span).trim().to_string();
                    value::substitute(&raw, self.src, &self.env, 0, Some(span))
                }
                _ => self.eval_text(v),
            };
            let text = match strip_embedded_important(&text) {
                Some(stripped) => {
                    embedded = true;
                    stripped
                }
                None => text,
            };
            out.push_str(&text);
            after_solidus = solidus;
            prev_end = Some(span.end);
        }
        // From the last item to the terminator: ordinary whitespace is dropped (less's `Expression` leaves no trailing whitespace),
        // but block comments stay in place.
        if let (Some(end), Some((_, stop, semi))) = (prev_end, region)
            && end <= stop
            && stop <= self.src.len()
        {
            let tail = &self.src[end..stop];
            if tail.contains("/*") {
                let comment_at = tail.find("/*").unwrap_or(0);
                // less's `Expression` leaves one space between a value node and a comment
                out.push(' ');
                out.push_str(tail[comment_at..].trim_end());
            } else if semi && !embedded && tail.trim().is_empty() {
                out.push_str(tail);
            }
        }
        embedded
    }

    /// Evaluates a single segment (variables are looked up in the Scope; text variables are returned directly).
    pub(super) fn eval_text(&self, v: &'a ComponentValue<'a>) -> String {
        match v {
            // `@p[text]` / `#ns[$prop]`: namespace lookup happens only when the value is taken (lazy resolution)
            ComponentValue::LessNamespaceValue(ns) => match self.namespace_text(ns, 0) {
                Ok(text) => text,
                Err(message) => {
                    self.env.miss(message, ns.span);
                    String::new()
                }
            },
            // The lookup result takes part in an operation (`@sizes[base] * 2`)
            ComponentValue::LessBinaryOperation(op)
                if has_lookup(&op.left) || has_lookup(&op.right) =>
            {
                let left = self.eval_text(&op.left);
                let right = self.eval_text(&op.right);
                value::arithmetic(&op.op.kind, &left, &right)
                    .unwrap_or_else(|| format!("{left} {right}"))
            }
            _ => eval(v, self.src, &self.env, 0),
        }
    }

    /// Used when the property name is actually emitted: undefined variables are reported as less does.
    pub(super) fn prop_name_at(&self, d: &Declaration) -> String {
        let span = *d.name.span();
        let name = self.interpolate_at(self.slice(span), span);
        self.prop_name_with(d, name)
    }

    /// Merges same-named `+:` / `+_:` declarations into the first position (less `_mergeRules`).
    pub(super) fn merge_pieces(&self, pieces: Vec<Piece<'a>>) -> Vec<Piece<'a>> {
        let mut out: Vec<Piece<'a>> = Vec::with_capacity(pieces.len());
        let mut slots: HashMap<String, usize> = HashMap::new();
        for piece in pieces {
            match piece {
                Piece::Decl(e) if e.decl.less_property_merge.is_some() => {
                    let name = self.prop_name(e.decl);
                    match slots.get(&name) {
                        Some(&i) => {
                            if let Piece::Merged(items) = &mut out[i] {
                                items.push(e);
                            }
                        }
                        None => {
                            slots.insert(name, out.len());
                            out.push(Piece::Merged(vec![e]));
                        }
                    }
                }
                other => out.push(other),
            }
        }
        out
    }

    /// less's `ruleset.parseValue` re-parses declarations referenced by `$prop` in place, and after re-parsing
    /// the declaration's `important` field is `!important` (no leading space) — so output has no space.
    pub(super) fn mark_reparsed_props(&mut self, pieces: &[Piece<'a>]) {
        let mut names: Vec<String> = Vec::new();
        for piece in pieces {
            if let Piece::Decl(e) = piece {
                for v in &e.decl.value {
                    collect_prop_vars(v, &mut names);
                }
            }
        }
        for name in names {
            for src in self.env.get_prop(&name).unwrap_or_default() {
                match src {
                    PropSource::Decl(d) => {
                        if d.important.is_some() {
                            self.reparsed.insert(d.span.start);
                        }
                    }
                    PropSource::Merged(groups) => {
                        for d in groups.iter().flatten() {
                            if d.important.is_some() {
                                self.reparsed.insert(d.span.start);
                            }
                        }
                    }
                }
            }
        }
    }

    /// This level's property table: all same-named entries are kept (`$prop` takes the last one or the merged result).
    pub(super) fn collect_props(
        &self,
        pieces: &[Piece<'a>],
    ) -> HashMap<String, Vec<PropSource<'a>>> {
        let mut map: HashMap<String, Vec<PropSource<'a>>> = HashMap::new();
        for piece in pieces {
            match piece {
                Piece::Decl(e) => {
                    let name = self.prop_name(e.decl);
                    map.entry(name).or_default().push(PropSource::Decl(e.decl));
                }
                Piece::Merged(items) => {
                    let name = self.prop_name(items[0].decl);
                    let refs: Vec<&Declaration<'a>> = items.iter().map(|e| e.decl).collect();
                    let groups = merge_groups(&refs)
                        .into_iter()
                        .map(|g| g.into_iter().map(|i| refs[i]).collect())
                        .collect();
                    map.entry(name)
                        .or_default()
                        .push(PropSource::Merged(groups));
                }
                Piece::Comment(_) | Piece::AtRule(_) => {}
            }
        }
        map
    }

    pub(super) fn emit_at_rule(
        &mut self,
        at: &'a AtRule,
        parents: &[Sel],
        parent_spans: &[Span],
        level: usize,
        inject: Option<Inject<'a>>,
        layer: &mut Layer,
    ) -> Result<(), Box<CompileError>> {
        let name = at.name.name.to_string();
        // A single file keeps the existing map: selectors passed through an at-rule are not mapped separately.
        let parent_spans = if self.multi_source { parent_spans } else { &[] };
        if name == "import" {
            return self.emit_import(at, layer);
        }
        // less emits only the first `@charset` (later ones are dropped)
        if name.eq_ignore_ascii_case("charset") {
            if self.charset {
                return Ok(());
            }
            self.charset = true;
        }
        // less `JoinSelectorVisitor.visitAtRule`: the body of a declarative at-rule (@font-face/@keyframes/@page…)
        // is a root ruleset, so parent selectors do not propagate into it (only @document/@supports inherit them).
        let (parents, parent_spans) = if at_rule_keeps_parents(&name) {
            (parents, parent_spans)
        } else {
            (&[][..], &[][..])
        };
        // An @media inside a media block: conditions are merged and it bubbles to the same level (less's media bubble)
        if name.eq_ignore_ascii_case("media")
            && let Some((outer, outer_level)) = self.media.last().cloned().flatten()
        {
            self.deferred
                .last_mut()
                .expect("media defer stack")
                .push(Deferred {
                    at,
                    sels: parents.to_vec(),
                    spans: parent_spans.to_vec(),
                    level: outer_level,
                    outer,
                    inject,
                });
            return Ok(());
        }
        self.emit_at_rule_inner(at, parents, parent_spans, level, None, layer)
    }

    /// A percentage/keyword block inside an `@keyframes` body (oxc classifies it as a standalone statement, not a rule).
    pub(super) fn emit_keyframe(
        &mut self,
        k: &'a KeyframeBlock<'a>,
        level: usize,
        layer: &mut Layer,
    ) -> Result<(), Box<CompileError>> {
        let start = layer.items.len();
        let raw = self
            .slice(Span {
                start: k.span.start,
                end: k.block.span.start,
            })
            .trim()
            .to_string();
        let text = self.interpolate_at(
            &raw,
            Span {
                start: k.span.start,
                end: k.block.span.start,
            },
        );
        let sels: Vec<Sel> = split_selectors(&text)
            .iter()
            .map(|s| {
                vec![extend::Element {
                    combinator: String::new(),
                    value: s.clone(),
                }]
            })
            .collect();
        self.emit_block(
            &k.block.statements,
            Head {
                sels: &sels,
                spans: &[],
                extends: vec![Vec::new(); sels.len()],
                root: false,
            },
            level,
            false,
            k.block.span,
            layer,
        )?;
        // Blocks inside an `@keyframes` body are keyframes, not style rules: there are no blank lines between them.
        let frames = layer.items.split_off(start);
        for frame in frames {
            layer.items.push(match frame {
                Item::Rule(rule) => Item::Keyframe(Keyframe {
                    selector: rule.selector,
                    body: rule.body,
                }),
                other => other,
            });
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn emit_at_rule_inner(
        &mut self,
        at: &'a AtRule,
        parents: &[Sel],
        parent_spans: &[Span],
        level: usize,
        outer: Option<String>,
        layer: &mut Layer,
    ) -> Result<(), Box<CompileError>> {
        let name = at.name.name.to_string();
        let (prelude, moved) = self.at_prelude(at);
        let is_media = name.eq_ignore_ascii_case("media");
        let hoist = outer.is_some();
        let prelude = match outer {
            Some(outer) if !prelude.is_empty() => {
                if outer.is_empty() {
                    prelude
                } else {
                    format!("{outer} and {prelude}")
                }
            }
            other => other.unwrap_or(prelude),
        };
        // Reserve a slot first: the shell's index in `items` is the start of the `(reference)` pruning range.
        let index = layer.items.len();
        layer.items.push(Item::AtRule(OutAtRule {
            name,
            name_span: *at.name.span(),
            prelude: prelude.clone(),
            body: None,
            group_end: false,
            hoist,
        }));
        match &at.block {
            Some(block) => {
                let scope = self.extends.push_scope(Some(self.scope));
                let saved = self.scope;
                self.scope = scope;
                self.media.push(is_media.then(|| (prelude.clone(), level)));
                self.deferred.push(Vec::new());
                let has_body = !block.statements.is_empty();
                let mut body = layer.child(index);
                // Prelude comments in an empty block: identifier-like at-rules such as @keyframes keep the block alive through comments,
                // while an empty @media block is dropped entirely (less `to-css-visitor` prune).
                if has_body || !moved.is_empty() && !is_media {
                    body.items.extend(moved.into_iter().map(Item::Comment));
                }
                self.emit_block(
                    &block.statements,
                    Head {
                        sels: parents,
                        spans: parent_spans,
                        extends: vec![Vec::new(); parents.len()],
                        root: false,
                    },
                    level + 1,
                    true,
                    block.span,
                    &mut body,
                )?;
                let deferred = self.deferred.pop().unwrap_or_default();
                self.media.pop();
                self.scope = saved;
                let body = body.into_body(level + 1);
                // Empty blocks are not emitted (less drops empty rulesets/empty media blocks)
                if body.is_empty() {
                    layer.items.pop();
                } else if let Item::AtRule(shell) = &mut layer.items[index] {
                    shell.body = Some(body);
                    // The shell's range ends here: a bubbled nested media lies outside the shell and is not part of its content
                    if self.hidden {
                        self.ref_ranges.push((layer.path.clone(), index, index + 1));
                    }
                }
                // Nested media from the block now follow it (less's output order)
                for item in deferred {
                    self.env.push_scope(&item.inject);
                    self.emit_at_rule_inner(
                        item.at,
                        &item.sels,
                        &item.spans,
                        item.level,
                        Some(item.outer),
                        layer,
                    )?;
                    self.env.pop_scope(&item.inject);
                }
            }
            None => layer.items.extend(moved.into_iter().map(Item::Comment)),
        }
        Ok(())
    }

    /// Inlines `@import "x.less";`.
    pub(super) fn emit_import(
        &mut self,
        at: &'a AtRule,
        layer: &mut Layer,
    ) -> Result<(), Box<CompileError>> {
        let Some(prelude) = &at.prelude else {
            return Ok(());
        };
        let raw = self.slice(*prelude.span()).trim().to_string();
        // Comments are not part of the path/media condition: less moves them outside the statement (still emitted as block comments)
        let raw = strip_block_comments(&raw);
        let raw = self.interpolate_at(&raw, *prelude.span());
        let (options, _) = split_import_options(&raw);
        let optional = options.iter().any(|o| o == "optional");
        let inline = options.iter().any(|o| o == "inline");
        let forced_less = options.iter().any(|o| o == "less");
        // Variables in the media condition are evaluated at the current position (less evaluates at the prelude level)
        let raw = value::substitute(&raw, self.src, &self.env, 0, Some(*prelude.span()));
        let (_, rest) = split_import_options(&raw);
        let Some((href, media)) = split_quoted_path(&rest) else {
            // No path → emit as-is
            self.push_at(layer, at, raw.trim());
            return Ok(());
        };
        let media = media.trim().to_string();

        // Remote / .css / .wxss / `?css` (not marked (less)) → kept as a CSS import (not inlined)
        if !forced_less
            && !inline
            && (is_remote(&href) || is_css_path(&href) || href.ends_with(".wxss"))
        {
            // The media condition is normalized as less does (`(min-width:100px)` → `(min-width: 100px)`)
            let text = match raw.ends_with(&media) && !media.is_empty() {
                true => format!(
                    "{} {}",
                    raw[..raw.len() - media.len()].trim_end(),
                    normalize_prelude(&strip_escapes(&media))
                ),
                false => raw.clone(),
            };
            self.push_at(layer, at, &text);
            return Ok(());
        }

        // Canonicalize the path: emit-time inlining shares the same import-once dedup key as text inlining
        let Some(resolved) = self
            .resolve_import(&href)
            .map(|p| p.canonicalize().unwrap_or(p))
        else {
            if optional {
                return Ok(());
            }
            return Err(CompileError::new(format!(
                "@import not found: {href} (from {})",
                self.file_path.display()
            ))
            .into());
        };
        if inline && !forced_less {
            // less's (inline): written out verbatim without parsing; wrapped entirely in @media when a media condition is present
            let text = std::fs::read_to_string(&resolved)
                .map_err(|e| Box::new(CompileError::new(e.to_string())))?;
            self.deps.push(resolved.to_string_lossy().to_string());
            let text = if media.is_empty() {
                text
            } else {
                format!(
                    "@media {} {{\n  {text}\n}}",
                    normalize_prelude(&strip_escapes(&media))
                )
            };
            self.push_raw(layer, text);
            return Ok(());
        }
        if self.seen.contains(&resolved) {
            return Ok(());
        }
        self.seen.push(resolved.clone());
        self.deps.push(resolved.to_string_lossy().to_string());

        let source = std::fs::read_to_string(&resolved).map_err(|e| {
            Box::new(CompileError::new(format!(
                "failed to read {}: {e}",
                resolved.display()
            )))
        })?;

        // First version: @import is inlined with an independent compiler instance (variable scopes do not cross files, and neither does the map).
        // A less import is actually a text-level merge (shared scope); this is a known deviation to be fixed later.
        let sub_opts = Options {
            paths: self.options.paths.clone(),
            less: crate::LessOptions {
                global_vars: self.options.less.global_vars.clone(),
            },
            source_map: false,
            project_root: None,
        };
        let sub = crate::less::compile(&resolved, &source, &sub_opts)?;
        // The sub-compilation output has its own trailing newline; `doc::print` adds the newlines between `Raw` items.
        let css = sub.css.strip_suffix('\n').unwrap_or(&sub.css);
        self.push_raw(layer, css.to_string());
        self.deps.extend(sub.deps);
        Ok(())
    }

    /// Echoes an `@import` kept as CSS (not inlined).
    ///
    /// less echoes the evaluated prelude (path + media condition), so this is placed as an `AtRule`: the name maps to
    /// `@` and the prelude comes out verbatim (matching the previous bytes).
    fn push_at(&self, layer: &mut Layer, at: &AtRule, prelude: &str) {
        layer.items.push(Item::AtRule(OutAtRule {
            name: at.name.name.to_string(),
            name_span: *at.name.span(),
            prelude: prelude.to_string(),
            body: None,
            group_end: false,
            hoist: false,
        }));
    }

    /// Places a whole text block as one `Raw` child item (`(inline)` file content, sub-compilation output).
    ///
    /// Empty text is not placed: the IR newline between items would create a phantom blank line.
    fn push_raw(&self, layer: &mut Layer, text: String) {
        if text.is_empty() {
            return;
        }
        layer.items.push(Item::Raw(Raw {
            text,
            marks: Vec::new(),
        }));
    }
}
