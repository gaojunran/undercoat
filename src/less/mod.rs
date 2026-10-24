//! The less compiler: scopes, rule expansion, @import, printing.
//!
//! Semantics follow less@4.1.3:
//! - Variables are lazy (later definitions in the same scope win; values are evaluated on use)
//! - Declarations and nested rules are separated (the parent rule's declarations are collected and merged first, then child rules are expanded)
//! - Selector combination order: the child selector is on the outside and the parent selector on the inside (measured with less)
//! - `&` text replacement; `@import` inlining

pub(crate) mod comments;
pub(crate) mod emit;
pub(crate) mod env;
pub(crate) mod eval;
pub(crate) mod extend;
pub(crate) mod file_fn;
pub(crate) mod guard;
pub(crate) mod imports;
pub(crate) mod prelude;
pub(crate) mod selector;
pub(crate) mod value;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use oxc_css_parser::ast::*;
use oxc_css_parser::pos::Span;
use oxc_css_parser::{Allocator, ParserBuilder, Syntax};

use crate::doc::{self, Body, DeclItem, Item, Style};
use crate::merge::Merged;
use crate::{CompileError, Options, Output};

use comments::collect_definition_spans;
use emit::{ChildItem, Deferred, Piece, Sink};
pub(crate) use env::RuleSet;
use env::{Defs, Env, Inject, Roots, Vars};
use eval::collect_mixins;
use extend::{Need, Sel};
use imports::expand_imports;
use prelude::{has_plugin, is_charset, is_import};
use value::VarRef;

/// Strips a trailing `!important` from the value text (coming from a variable value) and returns the remaining text.
pub(super) fn strip_embedded_important(text: &str) -> Option<String> {
    let trimmed = text.trim_end();
    let cut = trimmed.len().checked_sub("!important".len())?;
    if !trimmed[cut..].eq_ignore_ascii_case("!important") {
        return None;
    }
    Some(trimmed[..cut].trim_end().to_string())
}

pub(crate) struct Compiler<'a> {
    env: Env<'a>,
    /// Merged text plus file/segment tables (handed to `doc::print` at the end).
    merged: &'a Merged,
    src: &'a str,
    /// More than one source file takes part in compiling (`@import` inlining); a single file keeps the existing map bytes.
    multi_source: bool,
    file_path: PathBuf,
    /// Paths relative to the project root (for error messages/deps).
    deps: Vec<String>,
    /// Absolute paths of files inlined by @import (deduplicated).
    seen: Vec<PathBuf>,
    /// All mixin definitions (with and without parentheses); several may share a name (less matches them all).
    mixins: Defs<'a>,
    /// The current rule selector token chain (used for mixin resolution).
    path: Vec<String>,
    /// Same length as `path`: whether this level can be entered with zero arguments as a namespace chain element.
    path_linkable: Vec<bool>,
    /// Mixin bodies currently being expanded and their argument signatures (to prevent self-recursion without a terminating condition).
    active: Vec<(*const SimpleBlock<'a>, String)>,
    /// Rule bodies currently being emitted (less's `isRecursive`: a same-named rule does not match itself).
    rule_bodies: Vec<*const SimpleBlock<'a>>,
    /// Whether the current level is inside a rule expanded from an `!important` call (child rules inherit it).
    force_important: bool,
    /// Block comment spans (`//` line comments are discarded by less and not collected).
    comments: Vec<(usize, usize)>,
    /// Comments already emitted with a block (so the root-level flush does not emit them again)
    comment_done: Vec<bool>,
    comment_idx: usize,
    options: &'a Options,
    /// The extend evaluator (selector rewriting is deferred to the end)
    extends: extend::Engine,
    /// The current media/at-rule scope
    scope: usize,
    /// Nested media deferred inside a media block (less merges conditions and bubbles them to the same level)
    deferred: Vec<Vec<Deferred<'a>>>,
    /// Full condition and level of the @media containing the current block (`None` outside media blocks)
    media: Vec<Option<(String, usize)>>,
    /// Whether @charset has been emitted (less keeps only the first one)
    charset: bool,
    /// Body spans of mixins / detached rulesets (comments inside them do not belong to the root level)
    def_spans: Vec<Span>,
    /// Declarations referenced by `$prop` (less re-parses such a declaration on reference, and `!important` loses its leading space)
    reparsed: std::collections::HashSet<usize>,
    /// Whether output is currently inside a `(reference)` import body (rules are invisible by default and the whole block may be dropped)
    hidden: bool,
    /// Output ranges of `(reference)` blocks: the whole range is pruned when the block has no visible rule
    ref_ranges: Vec<(Vec<usize>, usize, usize)>,
}

pub(crate) fn compile(
    path: &Path,
    source: &str,
    opts: &Options,
) -> Result<Output, Box<CompileError>> {
    // less treats `@import` as text-level inlining: the imported file shares the host's scope.
    // The entry file is registered by its canonical path too, otherwise a circular import `a -> b -> a` would inline the entry again
    let mut seen = vec![path.canonicalize().unwrap_or_else(|_| path.to_path_buf())];
    let mut deps = Vec::new();
    let mut merged = Merged::new();
    let part = expand_imports(
        path,
        source,
        opts,
        &mut seen,
        &mut Vec::new(),
        &mut deps,
        false,
        &mut merged,
    );
    merged.append_part(part);
    let source = merged.text.as_str();
    let alloc = Allocator::default();
    let mut parser = ParserBuilder::new(&alloc, source)
        .syntax(Syntax::Less)
        .comments()
        .build();
    let ast = parser.parse::<Stylesheet>().map_err(|e| {
        // Parsing happens on the merged text; offsets are mapped through the segment table back to the failing file and the original position
        let (file, local) = merged
            .locate_exact(e.span.start)
            .unwrap_or((0, e.span.start));
        let (path, line, column) = match merged.position_in(file, local) {
            Some((path, line, column)) => (Some(path), Some(line), Some(column)),
            None => (None, None, None),
        };
        Box::new(CompileError {
            message: format!("less parse failed: {}", e.kind),
            path,
            line,
            column,
        })
    })?;

    let comments = parser
        .comments()
        .iter()
        .filter(|c| matches!(c.kind, oxc_css_parser::token::CommentKind::Block))
        .map(|c| (c.span.start, c.span.end))
        .collect::<Vec<_>>();

    let mut compiler = Compiler::new(path, &merged, opts);
    compiler.env.plugins = has_plugin(&ast.statements);
    compiler.comments = comments;
    compiler.comment_done = vec![false; compiler.comments.len()];
    compiler.seen = seen;
    compiler.deps = deps;
    let mut frames: Vec<Vars<'_>> = Vec::new();
    let mut conds: Vec<(&LessConditions<'_>, Inject<'_>)> = Vec::new();
    collect_mixins(
        &ast.statements,
        source,
        &mut frames,
        &mut Vec::new(),
        &mut Vec::new(),
        &mut conds,
        &mut compiler.mixins,
    );
    // Block comments inside mixin / detached ruleset bodies do not belong to the root level; they are emitted one by one during expansion
    collect_definition_spans(&ast.statements, &mut compiler.def_spans);
    let mut root_layer = Layer::default();
    compiler.emit_block(
        &ast.statements,
        Head {
            sels: &[],
            spans: &[],
            extends: Vec::new(),
            root: true,
        },
        0,
        false,
        Span {
            start: 0,
            end: source.len(),
        },
        &mut root_layer,
    )?;
    compiler.flush_comments(source.len(), &mut root_layer);
    debug_assert!(
        root_layer.decls.is_empty(),
        "the root layer produces no bare declarations"
    );
    compiler.finish(root_layer.items)
}

impl<'a> Compiler<'a> {
    pub(super) fn new(path: &Path, merged: &'a Merged, opts: &'a Options) -> Self {
        let multi_source = merged.files.len() > 1;
        // globalVars (a pair of options from useCompilerPlugins) become the outermost text variables
        let mut root = HashMap::new();
        for (k, v) in &opts.less.global_vars {
            root.insert(k.clone(), VarRef::Text(v.clone()));
        }
        let mut env = Env::new(root);
        // File functions are relative to the directory of the entry less file (less's `entryPath`)
        env.roots = Some(Rc::new(Roots {
            dir: path.parent().unwrap_or(Path::new(".")).to_path_buf(),
            paths: opts.paths.clone(),
        }));
        Self {
            env,
            merged,
            src: merged.text.as_str(),
            multi_source,
            file_path: path.to_path_buf(),
            deps: Vec::new(),
            seen: vec![path.to_path_buf()],
            mixins: Defs::default(),
            path: Vec::new(),
            path_linkable: Vec::new(),
            active: Vec::new(),
            rule_bodies: Vec::new(),
            force_important: false,
            comments: Vec::new(),
            comment_done: Vec::new(),
            comment_idx: 0,
            options: opts,
            extends: extend::Engine::new(),
            scope: 0,
            deferred: Vec::new(),
            media: Vec::new(),
            charset: false,
            def_spans: Vec::new(),
            reparsed: std::collections::HashSet::new(),
            hidden: false,
            ref_ranges: Vec::new(),
        }
    }

    /// Slice of the merged text (spans come from an AST parsed over the merged text).
    pub(super) fn slice(&self, span: Span) -> &str {
        self.src.get(span.start..span.end).unwrap_or("")
    }

    /// Finishing: extend rewrites selectors → `(reference)` pruning → hand over to `doc::print`.
    pub(super) fn finish(mut self, mut items: Vec<Item>) -> Result<Output, Box<CompileError>> {
        self.extends.run();
        rewrite(&mut items, &self.extends);
        // Pruning must come before dropping placeholders: range indices are from tree construction (removing items would invalidate them).
        prune(&mut items, &mut Vec::new(), &mut self.ref_ranges);
        drop_placeholders(&mut items);
        if let Some((message, span)) = self.env.misses.borrow().first().cloned() {
            let (path, line, column) = match self
                .merged
                .locate_exact(span.start)
                .and_then(|(file, local)| self.merged.position_in(file, local))
            {
                Some((path, line, column)) => (Some(path), Some(line), Some(column)),
                None => (None, None, None),
            };
            return Err(Box::new(CompileError {
                message,
                path,
                line,
                column,
            }));
        }
        let entry = self.file_path.to_string_lossy();
        // `sourcesContent` is written only with multiple sources (lessc's `outputSourceFiles` follows inlining).
        let style = Style {
            sources_content: self.multi_source,
            ..Style::LESS
        };
        let (css, map) = doc::print(&items, &style, &entry, self.merged, self.options);
        Ok(Output {
            css,
            map,
            deps: self.deps,
        })
    }

    /// Block expansion: variable collection → declaration emission → child rule recursion.
    ///
    /// `block_span` is this block's `{…}` span, used to attribute block comments to a specific level (in less a comment is
    /// a node of the containing rule array). Comments in a block include "comments attached to child rules" but exclude those inside child blocks.
    pub(super) fn emit_block(
        &mut self,
        stmts: &'a [Statement<'a>],
        head: Head<'_>,
        level: usize,
        bare: bool,
        block_span: Span,
        layer: &mut Layer,
    ) -> Result<(), Box<CompileError>> {
        let saved_hidden = self.hidden;
        self.env.push();
        // Whether this block emits a block with selectors (decides comment attribution: comments in a block go to this block, root-level ones wait for pass 4)
        let wrapped = !head.sels.is_empty() || bare;
        let mut pieces: Vec<Piece<'a>> = Vec::new();
        let mut children: Vec<ChildItem<'a>> = Vec::new();
        let mut needs: Vec<Need> = Vec::new();
        let mut sink = Sink {
            pieces: &mut pieces,
            children: &mut children,
            inject: None,
            needs: &mut needs,
        };
        self.collect_block(stmts, block_span, wrapped, &mut sink);
        // The rule must be registered before child rules (less collects extends in tree order)
        let rule = self.register_rule(&head, &needs);
        // Property merging (`+:` / `+_:`) and the property table for `$prop`
        let pieces = self.merge_pieces(pieces);
        self.env.set_props(self.collect_props(&pieces));
        self.mark_reparsed_props(&pieces);
        let pieces = self.dedupe_pieces(pieces);

        // pass 3: emit this level's rules (empty blocks are not emitted; with only comments a selector block is still wrapped)
        if !pieces.is_empty() && head.sels.is_empty() && bare {
            // @font-face / @page: the block holds bare declarations with no selector wrapper
            for piece in &pieces {
                // Comments at the top level of a `(reference)` body are masked nodes and are dropped by less
                if self.hidden && matches!(piece, Piece::Comment(_)) {
                    continue;
                }
                let item = self.piece_item(piece);
                layer.decls.push(item);
            }
        } else if !pieces.is_empty() && !head.sels.is_empty() {
            // One `(String, Span)` per selector line: the line text is byte-for-byte the `a,\nb` form,
            // and `doc::print` adds the indentation and newline of continuation lines according to `Style`.
            let selector = head
                .sels
                .iter()
                .enumerate()
                .map(|(i, sel)| {
                    let mut text = extend::render(sel);
                    if i + 1 < head.sels.len() {
                        text.push(',');
                    }
                    let span = head
                        .spans
                        .get(i)
                        .copied()
                        .unwrap_or(Span { start: 0, end: 0 });
                    (text, span)
                })
                .collect();
            let body = self.piece_body(&pieces);
            layer.items.push(Item::Rule(crate::doc::Rule {
                selector,
                body,
                group_end: false,
                extend_id: rule,
                placeholder: false,
            }));
        }

        // pass 4: child rules. At the root block `@charset` / `@import` are hoisted first (less `ruleset.genCSS` reordering).
        if head.root {
            for child in &children {
                if let Child::AtRule(at) = child.child
                    && is_charset(at)
                {
                    self.emit_child_at(at, child, level, head.sels, head.spans, layer)?;
                }
            }
            // less hoists root-level `@import` before all other nodes (`importNodeIndex`),
            // but leaves comments preceding it in place.
            let front_limit = children
                .iter()
                .find_map(|c| match c.child {
                    Child::AtRule(at) if is_import(at) || is_charset(at) => None,
                    _ => Some(c.at),
                })
                .unwrap_or(usize::MAX);
            self.flush_comments(front_limit, layer);
            for child in &children {
                if let Child::AtRule(at) = child.child
                    && self.hoists(at, &child.inject)
                {
                    self.emit_child_at(at, child, level, head.sels, head.spans, layer)?;
                }
            }
        }
        for child in &children {
            if head.root
                && let Child::AtRule(at) = child.child
                && (is_charset(at) || self.hoists(at, &child.inject))
            {
                continue;
            }
            self.flush_comments(child.at, layer);
            match child.child {
                Child::Rule(rule) => self.emit_child_rule(
                    &rule.selector,
                    &rule.block,
                    child.inject.clone(),
                    child.important,
                    head.sels,
                    head.spans,
                    level,
                    layer,
                )?,
                Child::CondRule(rule) => self.emit_child_rule(
                    &rule.selector,
                    &rule.block,
                    child.inject.clone(),
                    child.important,
                    head.sels,
                    head.spans,
                    level,
                    layer,
                )?,
                Child::AtRule(at) => {
                    self.emit_child_at(at, child, level, head.sels, head.spans, layer)?
                }
                Child::Ref(block) => {
                    self.emit_reference(block, head.sels, head.spans, level, layer)?
                }
                Child::Keyframe(k) => self.emit_keyframe(k, level, layer)?,
                Child::Other(f) => self.emit_other(f, layer),
            }
        }

        self.env.pop();
        self.hidden = saved_hidden;
        Ok(())
    }

    /// Output of a `(reference)` import body: content expands as usual (rules are registered invisible);
    /// rules hit by an extend are emitted at the end under the extender's selector, and the rest of the block is removed.
    pub(super) fn emit_reference(
        &mut self,
        block: &'a SimpleBlock<'a>,
        parents: &[Sel],
        parent_spans: &[Span],
        level: usize,
        layer: &mut Layer,
    ) -> Result<(), Box<CompileError>> {
        let start = layer.items.len();
        let saved = self.hidden;
        self.hidden = true;
        // Comments at the top level of the import body are "masked nodes": less drops them and they must not leak from the root level
        self.own_comments(block.span, &block.statements);
        let result = self.emit_block(
            &block.statements,
            Head {
                sels: parents,
                spans: parent_spans,
                extends: vec![Vec::new(); parents.len()],
                root: false,
            },
            level,
            false,
            block.span,
            layer,
        );
        self.hidden = saved;
        self.ref_ranges
            .push((layer.path.clone(), start, layer.items.len()));
        result
    }

    /// Root-level bare function call: evaluated and emitted as a whole line (`e('…');` emits the inner text).
    /// The inner text is arbitrary CSS text that cannot be decomposed into IR nodes, so it becomes a `Raw` (mapping to the whole call).
    pub(super) fn emit_other(&mut self, f: &'a Function<'a>, layer: &mut Layer) {
        let text = value::eval_function_stmt(f, self.src, &self.env, 0);
        layer.items.push(Item::Raw(crate::doc::Raw {
            text,
            marks: vec![(0, f.span)],
        }));
    }

    /// `@{name}` / `${prop}` interpolation substitution.
    pub(super) fn interpolate(&self, text: &str) -> String {
        value::interpolate(text, self.src, &self.env, 0, None)
    }

    /// Interpolation with a source span: undefined variables must be reported as less does (at property names/selectors/extends and other real evaluation sites).
    pub(super) fn interpolate_at(&self, text: &str, span: Span) -> String {
        value::interpolate(text, self.src, &self.env, 0, Some(span))
    }

    /// Property name (interpolated, including the IE `*` hack prefix/suffix).
    /// Silent version: called during merging/deduplication/`$prop` comparison, when the injection scope may not be in place yet.
    pub(super) fn prop_name(&self, d: &Declaration) -> String {
        let name = self.interpolate(self.slice(*d.name.span()));
        self.prop_name_with(d, name)
    }

    pub(super) fn prop_name_with(&self, d: &Declaration, name: String) -> String {
        match (d.name_prefix, d.name_suffix) {
            (None, None) => name,
            (prefix, suffix) => format!(
                "{}{name}{}",
                prefix.map(String::from).unwrap_or_default(),
                suffix.map(String::from).unwrap_or_default()
            ),
        }
    }
}

/// The header of an emitted rule (selector list + options).
pub(super) struct Head<'s> {
    sels: &'s [Sel],
    /// Same length as `sels`: source span of each selector (empty: at-rule passthrough/generated selectors)
    spans: &'s [Span],
    /// The `:extend(...)` attached to each selector (same length as `sels`)
    extends: Vec<Vec<Need>>,
    /// The outermost block: `@charset` / `@import` are hoisted first (less `ruleset.genCSS` reordering)
    root: bool,
}

/// Output of one level: child `items` + bare `decls` (of an at-rule body).
///
/// `path` is this level's position in the whole tree (indices of ancestor `items`); `(reference)` pruning uses it
/// to return to the corresponding child range. `decls` is always empty for levels that are not at-rule bodies.
#[derive(Default)]
pub(super) struct Layer {
    path: Vec<usize>,
    items: Vec<Item>,
    decls: Vec<DeclItem>,
}

impl Layer {
    /// The body level of the `index`-th child item.
    fn child(&self, index: usize) -> Layer {
        let mut path = self.path.clone();
        path.push(index);
        Layer {
            path,
            items: Vec::new(),
            decls: Vec::new(),
        }
    }

    /// Body: normally `Decls`; when an at-rule body has both declarations and child rules (`@media screen { color: red;
    /// .a {…} }`), the single shape of `Body` cannot express "declarations first, then child rules", so the whole declarations section
    /// is joined into one `Raw` with the current level's indentation and placed before the child rules (verified byte-for-byte against lessc 4.1.3).
    fn into_body(self, level: usize) -> Body {
        if self.items.is_empty() {
            return Body::Decls(self.decls);
        }
        if self.decls.is_empty() {
            return Body::Rules(self.items);
        }
        let pad = format!("\n{}", "  ".repeat(level));
        let text = self
            .decls
            .iter()
            .map(decl_text)
            .collect::<Vec<_>>()
            .join(&pad);
        let mut items = self.items;
        items.insert(
            0,
            Item::Raw(crate::doc::Raw {
                text,
                marks: Vec::new(),
            }),
        );
        Body::Rules(items)
    }
}

enum Child<'a> {
    Rule(&'a QualifiedRule<'a>),
    /// A rule with a guard (`.a when (...)`)
    CondRule(&'a LessConditionalQualifiedRule<'a>),
    AtRule(&'a AtRule<'a>),
    /// A `0%` / `from` block inside an `@keyframes` body
    Keyframe(&'a KeyframeBlock<'a>),
    /// A root-level bare function call (`e('text');`)
    Other(&'a Function<'a>),
    /// A `(reference)` import body: content expands as usual but is not emitted by default (only emitted when hit by an extend)
    Ref(&'a SimpleBlock<'a>),
}

/// extend write-back: replaces the selectors with the IR selector lines for the visible ones (each line carries its own source anchor).
///
/// Invisible rules are only marked `placeholder` (`doc::print` skips them); no text pruning afterwards.
fn rewrite(items: &mut [Item], engine: &extend::Engine) {
    for item in items.iter_mut() {
        match item {
            Item::Rule(rule) => {
                let Some(id) = rule.extend_id else { continue };
                let sels = engine.visible_selectors(id);
                if sels.is_empty() {
                    rule.placeholder = true;
                    continue;
                }
                let last = sels.len() - 1;
                rule.selector = sels
                    .iter()
                    .enumerate()
                    .map(|(i, (sel, span))| {
                        let mut text = extend::render(sel);
                        if i < last {
                            text.push(',');
                        }
                        (text, *span)
                    })
                    .collect();
            }
            Item::AtRule(at) => {
                if let Some(Body::Rules(nested)) = at.body.as_mut() {
                    rewrite(nested, engine);
                }
            }
            Item::Keyframe(_) | Item::Comment(_) | Item::Raw(_) => {}
        }
    }
}

/// Drops placeholder rules (before printing, so empty `@media` blocks disappear too).
fn drop_placeholders(items: &mut Vec<Item>) {
    items.retain(|item| !matches!(item, Item::Rule(rule) if rule.placeholder));
    for item in items.iter_mut() {
        if let Item::AtRule(at) = item
            && let Some(Body::Rules(nested)) = at.body.as_mut()
        {
            drop_placeholders(nested);
        }
    }
}

/// `(reference)` pruning: removes the whole range when it contains no visible rule.
///
/// Ranges are recorded as "level path + child index" at tree construction (equivalent to the output byte ranges used before),
/// and a nested `@media` is a sibling bubbled outside the shell, so it is not miscounted as part of the shell.
fn prune(
    items: &mut Vec<Item>,
    path: &mut Vec<usize>,
    ranges: &mut Vec<(Vec<usize>, usize, usize)>,
) {
    for (i, item) in items.iter_mut().enumerate() {
        if let Item::AtRule(at) = item
            && let Some(Body::Rules(nested)) = at.body.as_mut()
        {
            path.push(i);
            prune(nested, path, ranges);
            path.pop();
        }
    }
    let mut mine: Vec<(usize, usize)> = ranges
        .iter()
        .filter(|(p, _, _)| p.as_slice() == path.as_slice())
        .map(|&(_, start, end)| (start, end))
        .collect();
    // Remove from the back so earlier indices stay valid.
    mine.sort_unstable_by_key(|(start, _)| std::cmp::Reverse(*start));
    for (start, end) in mine {
        if start < end && end <= items.len() && !items[start..end].iter().any(has_visible_rule) {
            items.drain(start..end);
        }
    }
}

/// Retention test for `(reference)` pruning: whether the range has a rule that produces bytes.
///
/// Comments, `Raw` (root-level function calls/bare comments), bodyless at-rules such as `@import`, and
/// hidden at-rules containing only declarations (`@font-face` inside `(reference)`) do not count — matching the previous visible-rule test.
fn has_visible_rule(item: &Item) -> bool {
    match item {
        Item::Rule(rule) => !rule.placeholder,
        Item::Keyframe(_) => true,
        Item::AtRule(at) => match at.body.as_ref() {
            Some(Body::Rules(nested)) => nested.iter().any(has_visible_rule),
            Some(Body::Decls(_)) | None => false,
        },
        Item::Comment(_) | Item::Raw(_) => false,
    }
}

/// Serialized text of one declaration (for `Raw` placement, matching the form produced by `doc::print`).
fn decl_text(item: &DeclItem) -> String {
    match item {
        DeclItem::Decl(decl) => match decl.custom {
            true => format!("{}:{};", decl.name, decl.value),
            false => format!("{}: {};", decl.name, decl.value),
        },
        DeclItem::Comment(comment) => format!("/*{}*/", comment.text),
        DeclItem::AtRule(at) => match at.prelude.is_empty() {
            true => format!("@{};", at.name),
            false => format!("@{} {};", at.name, at.prelude),
        },
    }
}
