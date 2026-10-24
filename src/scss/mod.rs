//! SCSS compile entry: assembly (`@import` / `@use`) -> parse (oxc `Syntax::Scss`) ->
//! evaluation -> serialization.
//!
//! The `.sass` indented syntax is folded into equivalent SCSS text at load time by
//! [`indented::to_scss`], so the merged text here is always in SCSS form (the parser is
//! fixed to `Syntax::Scss`).
//!
//! S1 covers variables / scope / nesting / interpolation / `@import` / `@use` (minimal)
//! and the SassScript expression evaluation that drives them; mixins / functions / control
//! flow still report an explicit error instead of silently producing broken output.

pub(crate) mod builtins;
mod color_space;
pub(crate) mod eval;
pub(crate) mod extend;
pub(crate) mod imports;
// Test hook (called directly by `tests/sass_conversion_ast.rs`), not a product API.
#[doc(hidden)]
pub mod indented;
pub(crate) mod modules;
pub(crate) mod value;

mod decls;
mod emit;
mod flow;
mod module_use;
mod prelude;
mod selector;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use oxc_css_parser::ast::*;
use oxc_css_parser::pos::Span;
use oxc_css_parser::{Allocator, ParserBuilder, Syntax};

use crate::doc::{
    AtRule as OutAtRule, Body, Comment, CustomStrip, Decl, DeclItem, Item, Keyframe, Rule, Style,
    print, selector_lines,
};
use crate::merge::Merged;
use crate::{CompileError, Options, Output};
use eval::{EvalError, Evaluator, Invocation, RestCheck};
use imports::{Loaded, Loader, ModuleInfo};

use self::decls::*;
use self::emit::*;
use self::prelude::*;
use self::selector::*;

/// Mixin recursion limit (dart-sass itself overflows the stack at around 300 levels;
/// here it is an explicit error instead of a crash).
const MAX_MIXIN_DEPTH: usize = 256;

/// Compile SCSS / plain CSS. `path` is only used for error reporting, dependency
/// resolution and the sourcemap's `sources`.
pub(crate) fn compile(
    path: &Path,
    source: &str,
    opts: &Options,
) -> Result<Output, Box<CompileError>> {
    // ParserBuilder strips the BOM itself, but it is stripped here first so that spans
    // line up with this function's src.
    let source = source.strip_prefix('\u{feff}').unwrap_or(source);
    let Loaded {
        merged,
        deps,
        modules,
        scopes,
        scope_of,
    } = Loader::new(path, source, opts).run()?;
    let src = merged.text.as_str();
    let module_file: HashMap<usize, u32> = modules
        .values()
        .map(|info| (info.scope, info.file))
        .chain(std::iter::once((0, 0)))
        .collect();

    // The merged text is always in SCSS form: `.sass` segments have already been
    // rewritten when `imports::Loader` loaded them.
    let alloc = Allocator::default();
    let mut parser = ParserBuilder::new(&alloc, src)
        .syntax(Syntax::Scss)
        .comments()
        .build();
    let ast = parser
        .parse::<Stylesheet>()
        .map_err(|e| merged.error_at(e.span.start, format!("SCSS parse failure: {}", e.kind)))?;
    // Container check of declaration-like at-rules (dart does it at parse time,
    // independent of the execution path).
    validate_structure(&merged, &ast.statements)?;

    let comments: Vec<CommentRec> = parser
        .comments()
        .iter()
        .filter(|c| c.kind == oxc_css_parser::token::CommentKind::Block)
        .map(|c| CommentRec {
            text: c.content.to_string(),
            span: c.span,
        })
        .collect();

    let mut eval = Evaluator::new(scopes);
    eval.set_source(&merged.text);
    let mut converter = Converter {
        empty_tail: false,
        had_child: false,
        pending_hoist: Vec::new(),
        merged: &merged,
        opts,
        eval,
        modules: &modules,
        scope_of: &scope_of,
        module_file: &module_file,
        comments,
        cursor: 0,
        mixins: (0..scopes).map(|_| vec![HashMap::new()]).collect(),
        content: Vec::new(),
        content_blocks: Vec::new(),
        mixin_depth: 0,
        control_depth: 0,
        in_mixin: false,
        decl_context: false,
        decl_prefix: None,
        block_depth: 0,
        media_stack: Vec::new(),
        extensions: extend::Engine::new(),
    };
    let mut items = hoist_imports(converter.stylesheet(&ast.statements)?);
    if let Some(err) = converter.check_forward_conflicts() {
        return Err(err);
    }
    if let Some(at) = converter.eval.unresolved_config() {
        return Err(converter
            .merged
            .error_at(
                at,
                "This variable was not declared with !default in the @used module.",
            )
            .into());
    }
    converter.finish_extensions(&mut items)?;
    drop(converter);

    let (css, map) = print(&items, &Style::SASS, &path.to_string_lossy(), &merged, opts);
    Ok(Output { css, map, deps })
}

/// Move root-level CSS `@import`s after the "leading comment/import section" (observed
/// with dart-sass).
///
/// Only imports that appear after other statements are moved; the move keeps their
/// relative order, and leading comments are not crossed (`/* c */ @import` keeps the
/// comment first).
fn hoist_imports(items: Vec<Item>) -> Vec<Item> {
    let is_import = |item: &Item| matches!(item, Item::AtRule(at) if at.body.is_none() && at.name.eq_ignore_ascii_case("import"));
    let prefix = items
        .iter()
        .take_while(|item| matches!(item, Item::Comment(_)) || is_import(item))
        .count();
    let mut keep = Vec::with_capacity(items.len());
    let mut hoisted = Vec::new();
    for (i, item) in items.into_iter().enumerate() {
        if i >= prefix && is_import(&item) {
            hoisted.push(item);
        } else {
            keep.push(item);
        }
    }
    keep.splice(prefix..prefix, hoisted);
    keep
}

/// One selector list: output text + the individual selectors after splitting (used by
/// nested parsing).
struct SelectorSet {
    parts: Vec<Sel>,
}

/// One complex selector plus dart-sass `ComplexSelector.lineBreak` (serialization breaks
/// the line before it).
struct Sel {
    text: String,
    line_break: bool,
}

impl SelectorSet {
    fn is_empty(&self) -> bool {
        self.parts.is_empty()
    }

    /// Serialize a selector list; `keep` filters out placeholder selectors (`%p`).
    fn render(&self, keep: impl Fn(&Sel) -> bool) -> String {
        let mut out = String::new();
        for part in self.parts.iter().filter(|p| keep(p)) {
            if !out.is_empty() {
                out.push(',');
                out.push(if part.line_break { '\n' } else { ' ' });
            }
            out.push_str(&part.text);
        }
        out
    }
}

struct CommentRec {
    text: String,
    span: Span,
}

/// One parameter (`@mixin` / `@function` share it; defined in `eval`).
type Param<'a> = eval::Param<'a>;

/// A mixin definition. `depth` is the frame count at the definition site: a call truncates
/// the frame stack to it (lexical closure), so that the mixin body cannot see the caller's
/// locals but can see the definition-time environment.
#[derive(Clone)]
struct MixinDef<'a> {
    params: Vec<Param<'a>>,
    rest: Option<&'a str>,
    body: &'a [Statement<'a>],
    body_span: Span,
    scope: usize,
    depth: usize,
    has_content: bool,
}

/// The content block of `@include m { ... }`: evaluated in the caller's environment
/// (lexical closure).
/// `tail` is the part taken off the caller's frame stack at `@include` (only present when
/// `def.scope == the caller's scope`); every `@content` switches back to the caller's
/// environment and writes the changes back here afterwards.
#[derive(Clone)]
struct ContentBlock<'a> {
    params: Vec<Param<'a>>,
    rest: Option<&'a str>,
    stmts: &'a [Statement<'a>],
    span: Span,
    scope: usize,
    head_len: usize,
    tail: Vec<eval::Frame<'a>>,
    mixins_tail: Vec<HashMap<String, MixinDef<'a>>>,
    cursor: usize,
    /// The content block "in effect" when this block was created (dart's `_contentBlock`
    /// capture): `@content` inside the block points at it, not at the block itself.
    outer: Option<usize>,
}

struct Converter<'a> {
    merged: &'a Merged,
    opts: &'a Options,
    eval: Evaluator<'a>,
    modules: &'a HashMap<PathBuf, ModuleInfo>,
    /// File id -> scope id (a module loaded by `@use` has its own scope).
    scope_of: &'a HashMap<u32, usize>,
    /// Scope -> the module file owning it (all other files are inlined by `@import`).
    module_file: &'a HashMap<usize, u32>,
    comments: Vec<CommentRec>,
    cursor: usize,
    /// One mixin frame per scope (pushed/popped in sync with the variable frames).
    mixins: Vec<Vec<HashMap<String, MixinDef<'a>>>>,
    /// Slot for the content block of each mixin call (`None` = that mixin received no
    /// content block).
    /// The top of the stack is the "currently effective" content block, consistent with
    /// dart's `_contentBlock`.
    content: Vec<Option<usize>>,
    /// The content block bodies themselves (referenced by index, since repeating
    /// `@content` must execute the same block several times).
    content_blocks: Vec<ContentBlock<'a>>,
    /// Current mixin call nesting depth (the recursion limit).
    mixin_depth: usize,
    /// Current control directive (`@if`) nesting depth: a mixin definition is not allowed
    /// inside one.
    control_depth: usize,
    in_mixin: bool,
    /// Declaration-list context (`@font-face` etc.): bare declarations are allowed.
    decl_context: bool,
    /// Property-name prefix of a nested property block (`font: { family: serif }`).
    decl_prefix: Option<String>,
    /// Current nesting depth (1 for the root stylesheet): `@use` is only allowed at 1.
    block_depth: usize,
    /// Stack of outer `@media` preludes (nested media must merge into a single query).
    media_stack: Vec<String>,
    /// `@extend` records: all of them are applied to the already emitted rules after the
    /// whole compilation finishes.
    extensions: extend::Engine,
    /// Whether the last **nested rule** in the block produced a node (an empty rule counts
    /// as a child node and "eats" the blank-line marker: dart's `isGroupEnd` falls on the
    /// last child node, and empty rules disappear with serialization).
    empty_tail: bool,
    /// Whether a nested rule has appeared in the current block (decides whether
    /// `empty_tail` looks at the child nodes or at itself).
    had_child: bool,
    /// Hoisted items "at the same position" in the current block (media merged across
    /// levels).
    /// dart closes the current rule before emitting such nodes, so they are accumulated
    /// until after the declarations.
    pending_hoist: Vec<Vec<Item>>,
}

impl<'a> Converter<'a> {
    fn src(&self) -> &'a str {
        &self.merged.text
    }

    /// Scope of the file containing the statement (files inlined by `@import` share the
    /// importer's scope).
    fn scope_at(&self, at: usize) -> usize {
        *self.scope_of.get(&self.file_at(at)).unwrap_or(&0)
    }

    /// Merged offset -> file id; on a miss it falls back to the last segment (the scss
    /// semantics for out-of-range locations).
    fn file_at(&self, at: usize) -> u32 {
        self.merged.locate_exact(at).map_or_else(
            || self.merged.segs.last().map_or(0, |seg| seg.file),
            |(file, _)| file,
        )
    }

    fn err(&self, at: usize, message: impl Into<String>) -> Box<CompileError> {
        self.merged.error_at(at, message).into()
    }

    fn eval_err(&self, e: EvalError) -> Box<CompileError> {
        self.merged.error_at(e.at, e.message).into()
    }

    /// `@at-root <selector> { ... }`: the selector is resolved against the root and the
    /// product is lifted out of the current rule (`@at-root a#{&}` is used to generate
    /// `a.badge` for `.badge`).
    fn at_root_rule(
        &mut self,
        at: &'a oxc_css_parser::ast::AtRule<'a>,
        parent: Option<&SelectorSet>,
        out: &mut Vec<Item>,
    ) -> Result<(), Box<CompileError>> {
        let (selectors, span) = match &at.prelude {
            Some(AtRulePrelude::SassAtRoot(root)) => match &root.kind {
                SassAtRootKind::Selector(list) => {
                    let resolved = self.selectors(list, None)?;
                    (Some(resolved.0), resolved.1)
                }
                _ => {
                    return Err(self.err(
                        at.span.start,
                        "SCSS feature not implemented yet (S3): query form of @at-root",
                    ));
                }
            },
            // Bare `@at-root { ... }`: the child rules resolve against the root level, but
            // `&` is still the outer selector.
            None => (None, at.span),
            _ => return Err(self.err(at.span.start, "SCSS parse failure: @at-root")),
        };
        let Some(block) = &at.block else {
            return Ok(());
        };
        if selectors.as_ref().is_some_and(|s| s.is_empty()) {
            return Ok(());
        }
        let saved_parent = parent.map(|p| p.render(|_| true));
        let mut items = Vec::new();
        let scope = self.push_scope(false);
        // `@at-root .x { ... }`: `&` inside the block is `.x`; the bare form keeps the
        // outer selector.
        self.eval.set_parent_selector(match &selectors {
            Some(sels) => Some(sels.render(|_| true)),
            None => saved_parent.clone(),
        });
        let saved_empty = self.empty_tail;
        let saved_child = self.had_child;
        self.empty_tail = false;
        self.had_child = false;
        let result = self.block(
            &block.statements,
            block.span.end,
            selectors.as_ref(),
            span,
            &mut items,
        );
        let tail_empty = if self.had_child {
            self.empty_tail
        } else {
            items.is_empty()
        };
        self.empty_tail = saved_empty;
        self.had_child = saved_child;
        self.eval.set_parent_selector(saved_parent);
        self.pop_scope(scope);
        result?;
        // The product of `@at-root` is a root-level node: dart marks the last child node as
        // a group end.
        if !tail_empty {
            if let Some(item) = items.last_mut() {
                mark_group_end(item);
            }
        }
        out.extend(items);
        // An empty rule produced by `@at-root` is also "a child node": it eats the
        // blank-line marker of the outer rule.
        self.empty_tail = tail_empty;
        self.had_child = true;
        Ok(())
    }

    /// `@extend <compound> [!optional]`: only registers, and everything is applied after
    /// the compilation ends (dart also has a global extension table, so a later `@extend`
    /// affects rules produced earlier).
    fn extend_rule(
        &mut self,
        at: &'a oxc_css_parser::ast::AtRule<'a>,
        parent: Option<&SelectorSet>,
    ) -> Result<(), Box<CompileError>> {
        let Some(AtRulePrelude::SassExtend(extend)) = &at.prelude else {
            return Err(self.err(at.span.start, "SCSS parse failure: @extend"));
        };
        let Some(parent) = parent else {
            return Err(self.err(at.span.start, "Extend may only be used within style rules."));
        };
        let extenders = parent
            .parts
            .iter()
            .filter_map(|part| extend::parse_sel(&part.text, part.line_break))
            .collect::<Vec<_>>();
        let raw = self
            .src()
            .get(extend.selectors.span.start..extend.selectors.span.end)
            .unwrap_or("")
            .to_string();
        let targets = self
            .eval
            .interpolate(&raw, extend.selectors.span.start)
            .map_err(|e| self.eval_err(e))?;
        let optional = extend.optional.is_some();
        for target in split_top_level_commas(&targets) {
            let target = target.trim();
            if target.is_empty() {
                continue;
            }
            let Some(compound) = extend::target_compound(target) else {
                return Err(self.err(
                    at.span.start,
                    format!("Can't extend complex selector {}.", target),
                ));
            };
            self.extensions.add_extension(extend::Extension {
                extenders: extenders.clone(),
                target: compound,
                optional,
                span: at.span.start,
            });
        }
        Ok(())
    }

    /// End of compilation: extension transitive closure -> apply to every rule ->
    /// `!optional` check.
    fn finish_extensions(&mut self, items: &mut Vec<Item>) -> Result<(), Box<CompileError>> {
        self.extensions.resolve();
        if std::env::var("EXT_DEBUG").is_ok() {
            self.extensions.debug_lists();
        }
        if !self.extensions.is_empty() {
            extend::apply(items, &self.extensions.lists, &self.extensions.dirty);
        }
        match self.extensions.unmatched() {
            Some(span) => Err(self.err(span, "The target selector was not found.")),
            None => Ok(()),
        }
    }

    fn stylesheet(&mut self, stmts: &'a [Statement<'a>]) -> Result<Vec<Item>, Box<CompileError>> {
        let mut out = Vec::new();
        self.block(
            stmts,
            self.src().len(),
            None,
            Span { start: 0, end: 0 },
            &mut out,
        )?;
        Ok(out)
    }

    /// One complete block: execute the statements and pack the declarations into several
    /// rules with the same selector, in source order.
    ///
    /// Declarations that reappear after a nested rule / at-rule start a new rule (the
    /// mixed-declaration semantics of dart-sass 1.78+), so declarations are not "all moved
    /// to the front" here.
    fn block(
        &mut self,
        stmts: &'a [Statement<'a>],
        end: usize,
        parent: Option<&SelectorSet>,
        parent_span: Span,
        out: &mut Vec<Item>,
    ) -> Result<(), Box<CompileError>> {
        let mut decls: Vec<DeclItem> = Vec::new();
        let mut nested: Vec<Item> = Vec::new();
        self.pending_hoist.push(Vec::new());
        self.stmts(stmts, end, parent, parent_span, &mut decls, &mut nested)?;
        // Media hoisted out of the outer at-rule inside this block come after the block's
        // own rules (dart closes the current rule before emitting hoisted nodes):
        // `@media {.b { @media M{} y:2 }}` emits `.b { y: 2 }` first and M after.
        self.flush_pending_hoist(&mut decls, parent, parent_span, &mut nested)?;
        self.flush_rule(&mut decls, parent, parent_span, &mut nested)?;
        if let Some(pending) = self.pending_hoist.pop() {
            nested.extend(pending);
        }
        out.extend(nested);
        Ok(())
    }

    /// Emit a batch of hoisted items "at the same position": first pack the declarations
    /// before them into a rule.
    fn flush_pending_hoist(
        &mut self,
        decls: &mut Vec<DeclItem>,
        parent: Option<&SelectorSet>,
        parent_span: Span,
        nested: &mut Vec<Item>,
    ) -> Result<(), Box<CompileError>> {
        if self.pending_hoist.last().is_none_or(Vec::is_empty) {
            return Ok(());
        }
        self.flush_rule(decls, parent, parent_span, nested)?;
        if let Some(pending) = self.pending_hoist.last_mut() {
            nested.append(pending);
        }
        Ok(())
    }

    /// Pack the collected declarations into one rule and append it to `nested` (an empty
    /// buffer is left alone).
    ///
    /// Call sites: at the end of a block, and before every nested rule / at-rule product --
    /// so declarations stay where they appear in the source instead of being moved before
    /// all nested products.
    fn flush_rule(
        &mut self,
        decls: &mut Vec<DeclItem>,
        parent: Option<&SelectorSet>,
        parent_span: Span,
        nested: &mut Vec<Item>,
    ) -> Result<(), Box<CompileError>> {
        if decls.is_empty() {
            return Ok(());
        }
        let items = std::mem::take(decls);
        let Some(selectors) = parent else {
            return Err(self.err(
                parent_span.start,
                "Declarations may only be used within style rules.",
            ));
        };
        // A placeholder selector (`%p`) does not emit CSS directly: the extension phase
        // decides where it goes.
        let selector = selectors.render(|_| true);
        if !selector.is_empty() {
            // Enter the extension engine in its "registration time" shape: existing
            // extensions apply right away, later ones only apply to already registered rules
            // (dart's incremental semantics).
            let parsed = selectors
                .parts
                .iter()
                .map(|part| extend::parse_sel(&part.text, part.line_break))
                .collect::<Option<Vec<_>>>();
            let extend_id = parsed.map(|list| self.extensions.add_rule(list));
            nested.push(Item::Rule(Rule {
                selector: selector_lines(&selector, parent_span),
                body: Body::Decls(items),
                group_end: false,
                extend_id,
                placeholder: false,
            }));
        }
        Ok(())
    }

    /// A statement sequence. The collector is provided by the caller: the declarations of a
    /// mixin body / `@if` body must be merged into the rule at the call site.
    fn stmts(
        &mut self,
        stmts: &'a [Statement<'a>],
        end: usize,
        parent: Option<&SelectorSet>,
        parent_span: Span,
        decls: &mut Vec<DeclItem>,
        nested: &mut Vec<Item>,
    ) -> Result<(), Box<CompileError>> {
        self.block_depth += 1;
        let result = self.stmts_loop(stmts, end, parent, parent_span, decls, nested);
        self.block_depth -= 1;
        result
    }

    fn stmts_loop(
        &mut self,
        stmts: &'a [Statement<'a>],
        end: usize,
        parent: Option<&SelectorSet>,
        parent_span: Span,
        decls: &mut Vec<DeclItem>,
        nested: &mut Vec<Item>,
    ) -> Result<(), Box<CompileError>> {
        for st in stmts {
            self.eval.enter(self.scope_at(st.span().start));
            let decl_context = self.decl_context;
            self.flush_comments(st.span().start, &mut |c| {
                if parent.is_some() || decl_context {
                    decls.push(DeclItem::Comment(c));
                } else {
                    nested.push(Item::Comment(c));
                }
            })?;
            // The following statements are no longer declarations: emit the accumulated
            // hoisted items together with the preceding declarations first.
            if !matches!(st, Statement::Declaration(_)) {
                self.flush_pending_hoist(decls, parent, parent_span, nested)?;
            }
            self.statement(st, parent, parent_span, decls, nested)?;
            let region_end = match st {
                Statement::Declaration(decl) => self.decl_region_end(decl.span.end).end,
                other => other.span().end,
            };
            self.skip_comments(region_end);
        }
        let decl_context = self.decl_context;
        self.flush_comments(end, &mut |c| {
            if parent.is_some() || decl_context {
                decls.push(DeclItem::Comment(c));
            } else {
                nested.push(Item::Comment(c));
            }
        })?;
        Ok(())
    }

    fn statement(
        &mut self,
        st: &'a Statement<'a>,
        parent: Option<&SelectorSet>,
        parent_span: Span,
        decls: &mut Vec<DeclItem>,
        nested: &mut Vec<Item>,
    ) -> Result<(), Box<CompileError>> {
        match st {
            Statement::Declaration(decl) => match self.nesting_cut(decl) {
                Some(cut) => self.nested_decl(decl, cut, parent, parent_span, decls, nested),
                None => match self.decl(decl)? {
                    Some(decl) => {
                        if parent.is_none() && !self.decl_context {
                            Err(self.err(
                                decl.name_span.start,
                                "Declarations may only be used within style rules.",
                            ))
                        } else {
                            decls.push(DeclItem::Decl(decl));
                            Ok(())
                        }
                    }
                    None => Ok(()),
                },
            },
            Statement::SassVariableDeclaration(var) => self.var_decl(var),
            Statement::QualifiedRule(rule) => {
                self.flush_rule(decls, parent, parent_span, nested)?;
                self.nested_rule(rule, parent, nested)
            }
            Statement::KeyframeBlock(_) => Err(self.err(
                st.span().start,
                "SCSS feature not implemented yet (S1): bare keyframe block",
            )),
            Statement::SassIfAtRule(rule) => self.if_rule(rule, parent, parent_span, decls, nested),
            // oxc classifies SCSS's own control-flow/definition at-rules as
            // `UnknownSassAtRule`.
            Statement::UnknownSassAtRule(at) => Err(self.err(
                at.span.start,
                format!(
                    "SCSS feature not implemented yet (S2): @{}",
                    ident_name(&at.name).unwrap_or("?")
                ),
            )),
            Statement::AtRule(at) => self.sass_at_rule(at, parent, parent_span, decls, nested),
            other => Err(self.err(
                st.span().start,
                format!(
                    "SCSS feature not implemented yet (S1): {}",
                    snippet(self.src(), other.span().start, other.span().end)
                ),
            )),
        }
    }

    /// Dispatch between SCSS-specific at-rules and ordinary CSS at-rules.
    fn sass_at_rule(
        &mut self,
        at: &'a oxc_css_parser::ast::AtRule<'a>,
        parent: Option<&SelectorSet>,
        parent_span: Span,
        decls: &mut Vec<DeclItem>,
        nested: &mut Vec<Item>,
    ) -> Result<(), Box<CompileError>> {
        // Dispatch by name: `@content` without arguments has a `None` prelude, so the
        // prelude cannot be used to tell them apart.
        let lower = at.name.name.to_ascii_lowercase();
        // Inside a nested property block only declarations and these control directives are
        // allowed (dart's `_declarationAtRule`).
        if self.decl_prefix.is_some()
            && !matches!(
                lower.as_str(),
                "include"
                    | "content"
                    | "each"
                    | "for"
                    | "while"
                    | "if"
                    | "debug"
                    | "warn"
                    | "error"
            )
        {
            return Err(self.err(at.span.start, "This at-rule is not allowed here."));
        }
        match lower.as_str() {
            "mixin" => return self.define_mixin(at),
            "function" => return self.define_function(at),
            "include" => return self.include_rule(at, parent, parent_span, decls, nested),
            "content" => return self.content_rule(at, parent, parent_span, decls, nested),
            "each" => return self.each_rule(at, parent, parent_span, decls, nested),
            "for" => return self.for_rule(at, parent, parent_span, decls, nested),
            "while" => return self.while_rule(at, parent, parent_span, decls, nested),
            // `@use` / `@forward` are guaranteed to be top-level by the assembly layer; here
            // it is only member registration.
            "use" => return self.use_rule(at),
            "forward" => return self.forward_rule(at),
            _ => {}
        }
        // A block-less `@import "x.css"` inside a rule: dart leaves it inside the braces
        // instead of lifting it out.
        if parent.is_some() && at.block.is_none() && at.name.name.eq_ignore_ascii_case("import") {
            let raw = self.prelude_raw(at);
            let prelude = self
                .eval
                .interpolate(&raw, at.name.span.end)
                .map_err(|e| self.eval_err(e))?;
            decls.push(DeclItem::AtRule(OutAtRule {
                name: at.name.name.to_string(),
                name_span: at.name.span,
                prelude: normalize_import_urls(prelude.trim()),
                body: None,
                group_end: false,
                hoist: false,
            }));
            return Ok(());
        }
        let mut items = Vec::new();
        self.at_rule(at, parent, parent_span, &mut items)?;
        // Only split when something was really emitted: `@extend` / `@debug` and the like
        // do not interrupt the declaration order.
        if items.is_empty() {
            return Ok(());
        }
        // Media hoisted inside the layer must wait until this block's declarations land
        // (see `flush_pending_hoist`).
        if parent.is_some() && items.iter().any(Item::is_hoisted) {
            if let Some(pending) = self.pending_hoist.last_mut() {
                pending.extend(items);
            }
            return Ok(());
        }
        self.flush_rule(decls, parent, parent_span, nested)?;
        nested.extend(items);
        Ok(())
    }

    /// Push one lexical frame (variable frame and mixin frame correspond one-to-one) and
    /// return the scope id for `pop_scope`.
    fn push_scope(&mut self, semi_global: bool) -> usize {
        let scope = self.eval.active_scope();
        self.eval.push_frame(semi_global);
        self.mixins[scope].push(HashMap::new());
        scope
    }

    fn pop_scope(&mut self, scope: usize) {
        let saved = self.eval.active_scope();
        self.eval.enter(scope);
        if self.mixins[scope].len() > 1 {
            self.mixins[scope].pop();
        }
        self.eval.pop_frame();
        self.eval.enter(saved);
    }

    /// Comment cursor: index of the first comment at position >= `at`.
    fn comment_cursor(&self, at: usize) -> usize {
        self.comments.partition_point(|c| c.span.start < at)
    }

    /// Nested rule: the selector is resolved against the parent and output as its own rule.
    fn nested_rule(
        &mut self,
        rule: &'a QualifiedRule<'a>,
        parent: Option<&SelectorSet>,
        out: &mut Vec<Item>,
    ) -> Result<(), Box<CompileError>> {
        self.skip_comments(rule.block.span.start);
        let (selectors, span) = self.selectors(&rule.selector, parent)?;
        if selectors.is_empty() {
            return Ok(());
        }
        let parsed = selectors
            .parts
            .iter()
            .filter_map(|part| extend::parse_sel(&part.text, part.line_break))
            .collect::<Vec<_>>();
        self.extensions.note_selectors(&parsed);
        let mut items = Vec::new();
        let scope = self.push_scope(false);
        let saved_parent = self.eval.parent_selector_text();
        self.eval
            .set_parent_selector(Some(selectors.render(|_| true)));
        let saved_empty = self.empty_tail;
        let saved_child = self.had_child;
        self.empty_tail = false;
        self.had_child = false;
        let result = self.block(
            &rule.block.statements,
            rule.block.span.end,
            Some(&selectors),
            span,
            &mut items,
        );
        self.eval.set_parent_selector(saved_parent);
        self.pop_scope(scope);
        result?;
        let root_rule = parent.is_none();
        let tail_empty = if self.had_child {
            self.empty_tail
        } else {
            items.is_empty()
        };
        self.empty_tail = saved_empty;
        self.had_child = saved_child;
        let this_empty = items.is_empty();
        let before = out.len();
        out.extend(items);
        let _ = this_empty;
        self.empty_tail = tail_empty;
        self.had_child = true;
        // A blank line is only added after the last product of a "source-level rule" (no
        // blank lines between nested rules).
        if root_rule && !tail_empty {
            if let Some(item) = out[before..].last_mut() {
                mark_group_end(item);
            }
        }
        Ok(())
    }

    fn keyframe(
        &mut self,
        block: &'a KeyframeBlock<'a>,
    ) -> Result<Option<Item>, Box<CompileError>> {
        self.skip_comments(block.block.span.start);
        let selector = self.keyframe_selectors(block)?;
        let body = Body::Decls(self.decl_list(&block.block.statements, block.block.span.end)?);
        if body.is_empty() {
            return Ok(None);
        }
        Ok(Some(Item::Keyframe(Keyframe {
            selector: selector_lines(
                &selector,
                Span {
                    start: block
                        .selectors
                        .first()
                        .map_or(block.span.start, |s| self.keyframe_span(s).start),
                    end: block.block.span.start,
                },
            ),
            body,
        })))
    }

    fn at_rule(
        &mut self,
        at: &'a oxc_css_parser::ast::AtRule<'a>,
        parent: Option<&SelectorSet>,
        parent_span: Span,
        out: &mut Vec<Item>,
    ) -> Result<(), Box<CompileError>> {
        let name = at.name.name.to_string();
        let lower = name.to_ascii_lowercase();
        if matches!(&at.prelude, Some(AtRulePrelude::SassAtRoot(_))) || lower == "at-root" {
            return self.at_root_rule(at, parent, out);
        }
        match lower.as_str() {
            "charset" => return Ok(()),
            "use" => {
                // `@use` is only allowed at the top level of a file (same wording as
                // dart-sass).
                if self.block_depth > 1 {
                    return Err(self.err(at.span.start, "This at-rule is not allowed here."));
                }
                return self.use_rule(at);
            }
            "debug" => {
                // `@debug` uses inspect form (maps / unquoted strings / colors are written
                // the Sass way).
                if let Some(AtRulePrelude::SassExpr(expr)) = &at.prelude {
                    let value = self
                        .eval
                        .eval_component(self.src(), expr)
                        .map_err(|e| self.eval_err(e))?;
                    let (path, line, _) = self.merged.position(at.span.start);
                    let message = match &value {
                        // dart's `Value.toString()`: a top-level quoted string loses its
                        // quotes, other values go through inspect (strings inside maps/lists
                        // keep their quotes).
                        value::Value::Str(s) if s.quoted => s.text.clone(),
                        value => value.inspect(),
                    };
                    eprintln!("{path}:{line} DEBUG: {message}");
                }
                return Ok(());
            }
            "warn" => {
                // `@warn` uses CSS form (values that cannot be output as CSS, such as maps,
                // report an error; observed with dart).
                if let Some(AtRulePrelude::SassExpr(expr)) = &at.prelude {
                    let value = self
                        .eval
                        .eval_component(self.src(), expr)
                        .map_err(|e| self.eval_err(e))?;
                    let message = match &value {
                        value::Value::Str(s) if s.quoted => Ok(s.text.clone()),
                        value => value.to_css(),
                    }
                    .map_err(|message| self.err(at.span.start, message))?;
                    let (path, line, column) = self.merged.position(at.span.start);
                    // The column starts at 1 following dart's error format.
                    eprintln!(
                        "WARNING: {message}\n    {path} {line}:{}  root stylesheet\n",
                        column + 1
                    );
                }
                return Ok(());
            }
            "error" => {
                // The error body is the inspect form of the value (`@error "boom"` ->
                // `"boom"`).
                let message = match &at.prelude {
                    Some(AtRulePrelude::SassExpr(expr)) => self
                        .eval
                        .eval_component(self.src(), expr)
                        .map_err(|e| self.eval_err(e))?
                        .inspect(),
                    _ => String::new(),
                };
                return Err(self.err(at.span.start, message));
            }
            // A `@return` outside a function body has already been rejected by the static
            // container check (this is only a last resort).
            "return" => return Err(self.err(at.span.start, "This at-rule is not allowed here.")),
            "extend" => return self.extend_rule(at, parent),
            "forward" => {
                if self.block_depth > 1 {
                    return Err(self.err(at.span.start, "This at-rule is not allowed here."));
                }
                return self.forward_rule(at);
            }
            // An `@include` / `@content` without a usable prelude node (parser fallback) is
            // an explicit error.
            "mixin" | "include" | "content" => {
                return Err(self.err(at.span.start, format!("SCSS parse failure: @{lower}")));
            }
            _ => {}
        }

        let mut prelude = {
            let raw = self.prelude_raw(at);
            let base = at.name.span.end;
            let interpolated = self
                .eval
                .interpolate(&raw, base)
                .map_err(|e| self.eval_err(e))?;
            if lower == "supports" {
                match supports_prelude(&interpolated, &mut self.eval, base) {
                    Ok(prelude) => prelude,
                    Err((offset, message)) => {
                        return Err(self.err(base + offset, message));
                    }
                }
            } else if lower == "media" {
                // The value of a media query is SassScript: a bare `$var` is substituted too.
                let text = self.substitute_vars(&interpolated, base)?;
                normalize_prelude(&text)
            } else if lower == "import" {
                normalize_import_urls(&normalize_prelude(&interpolated))
            } else {
                normalize_prelude(&interpolated)
            }
        };

        // Nested `@media` merges into one query by dart's rules (cross product; pairs with
        // conflicting types are dropped).
        let mut merged = false;
        if lower == "media" {
            if let Some(outer) = self.media_stack.last().cloned() {
                match merge_media(&outer, &prelude) {
                    Some(queries) if queries.is_empty() => return Ok(()),
                    Some(queries) => {
                        prelude = queries;
                        merged = true;
                    }
                    None => {}
                }
            }
        }

        let Some(block) = &at.block else {
            out.push(Item::AtRule(OutAtRule {
                name,
                name_span: at.name.span,
                prelude,
                body: None,
                group_end: false,
                hoist: false,
            }));
            return Ok(());
        };
        self.skip_comments(block.span.start);

        let pushed = if lower == "media" {
            self.media_stack.push(prelude.clone());
            true
        } else {
            false
        };
        let body = if lower.ends_with("keyframes") {
            let mut items = Vec::new();
            for st in &block.statements {
                self.flush_comments(st.span().start, &mut |c| items.push(Item::Comment(c)))?;
                if let Statement::KeyframeBlock(kb) = st {
                    if let Some(item) = self.keyframe(kb)? {
                        items.push(item);
                    }
                } else if !matches!(st, Statement::AtRule(_)) {
                    return Err(self.err(
                        st.span().start,
                        "SCSS feature not implemented yet (S1): unusual statement inside @keyframes",
                    ));
                }
                self.skip_comments(st.span().end);
            }
            self.flush_comments(block.span.end, &mut |c| items.push(Item::Comment(c)))?;
            Body::Rules(items)
        } else if parent.is_some()
            || has_rule_statements(&block.statements) && !is_decl_at_rule(&lower)
        {
            let mut items = Vec::new();
            let scope = self.push_scope(false);
            let result = self.block(
                &block.statements,
                block.span.end,
                parent,
                parent_span,
                &mut items,
            );
            self.pop_scope(scope);
            result?;
            Body::Rules(items)
        } else {
            Body::Decls(self.decl_list(&block.statements, block.span.end)?)
        };

        if pushed {
            self.media_stack.pop();
        }
        if body.is_empty() && is_conditional(&lower) {
            return Ok(());
        }
        let merged_away = merged;
        let rule = OutAtRule {
            name,
            name_span: at.name.span,
            prelude,
            body: Some(body),
            group_end: false,
            hoist: merged_away,
        };
        // Media that were merged (or contain merged media) are lifted out of the outer
        // media: split the segments in order, rewrap each segment in this media, and the
        // hoisted children become sibling nodes.
        if let Some(Body::Rules(items)) = rule.body.clone() {
            if items.iter().any(Item::is_hoisted) {
                let mut current: Vec<Item> = Vec::new();
                let mut segments: Vec<Item> = Vec::new();
                // Hoisted children split the outer media: segments only break at the boundary
                // between the old and new block, and consecutive siblings after the split
                // point are collected into one block again (1.35.2 makes one block per rule,
                // modern dart-sass only breaks at hoists).
                for mut item in items {
                    if item.is_hoisted() {
                        item.clear_hoist();
                        if !current.is_empty() {
                            segments.push(wrap_media(&rule, std::mem::take(&mut current)));
                        }
                        segments.push(item);
                    } else {
                        current.push(item);
                    }
                }
                if !current.is_empty() {
                    segments.push(wrap_media(&rule, current));
                }
                out.extend(segments);
                return Ok(());
            }
        }
        out.push(Item::AtRule(rule));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_sass_syntax_reports_position() {
        let err = compile(
            Path::new("t.scss"),
            "// header\n@forward \"sass:math\";\n",
            &Options::default(),
        )
        .unwrap_err();
        assert!(
            err.message.contains("not implemented yet"),
            "{}",
            err.message
        );
        assert_eq!(err.line, Some(2));
        assert_eq!(err.path.as_deref(), Some("t.scss"));
    }

    #[test]
    fn prelude_normalizes_colon() {
        assert_eq!(normalize_prelude("(min-width:100px)"), "(min-width: 100px)");
        assert_eq!(
            normalize_prelude(" screen and (min-width:100px)"),
            "screen and (min-width: 100px)"
        );
        assert_eq!(
            normalize_prelude(" (400px <= width <= 700px)"),
            "(400px <= width <= 700px)"
        );
        assert_eq!(
            normalize_prelude(" url(\"x.css\") screen"),
            "url(\"x.css\") screen"
        );
        assert_eq!(normalize_prelude(" :first"), ":first");
        assert_eq!(normalize_prelude(" selector(:has(a))"), "selector(:has(a))");
    }

    /// Dropping the quotes of an attribute selector only depends on whether the value itself
    /// can be an identifier: if so the quotes go, and the following descendant space must
    /// **not** be eaten.
    #[test]
    fn attribute_selector_quote_rules() {
        assert_eq!(
            normalize_selector("[id=\"modal\"] .bd-example .btn"),
            "[id=modal] .bd-example .btn"
        );
        assert_eq!(normalize_selector(".a[x=\"1\"] .b"), ".a[x=\"1\"] .b");
        assert_eq!(normalize_selector("[type=\"button\"]"), "[type=button]");
        assert_eq!(normalize_selector("[class*=\"col-\"]"), "[class*=col-]");
        assert_eq!(
            normalize_selector("[data-x=\"a b\"] .y"),
            "[data-x=\"a b\"] .y"
        );
    }

    /// `@supports` is printed structurally: redundant parentheses collapse, but a group
    /// used as an and/or operand keeps one level (matched item by item against dart-sass
    /// 1.35.2's `_parenthesize`).
    #[test]
    fn supports_condition_parens() {
        fn supports(prelude: &str) -> String {
            let mut eval = Evaluator::new(1);
            supports_prelude(prelude, &mut eval, 0).expect("prelude should be printable")
        }
        assert_eq!(supports("((a:b))"), "(a: b)");
        assert_eq!(supports("(((a:b)))"), "(a: b)");
        assert_eq!(supports("((a:b) and (c:d))"), "(a: b) and (c: d)");
        assert_eq!(supports("(a:b) and ((c:d))"), "(a: b) and (c: d)");
        assert_eq!(supports("not ((a:b))"), "not (a: b)");
        assert_eq!(supports("(display: grid)"), "(display: grid)");
        assert_eq!(
            supports("((a:b) or (c:d)) and (e:f)"),
            "((a: b) or (c: d)) and (e: f)"
        );
        assert_eq!(
            supports("(a:b) and ((c:d) or (e:f))"),
            "(a: b) and ((c: d) or (e: f))"
        );
        assert_eq!(supports("not ((a:b) and (c:d))"), "not ((a: b) and (c: d))");
        assert_eq!(
            supports("(a:b) and (c:d) and (e:f)"),
            "(a: b) and (c: d) and (e: f)"
        );
        assert_eq!(supports("selector(:has(a))"), "selector(:has(a))");
        assert_eq!(supports("foo(bar)"), "foo(bar)");
        // Mixing and/or at the same level is a syntax error in dart
        let mut eval = Evaluator::new(1);
        let err = supports_prelude("(a:1) and (b:2) or (c:3)", &mut eval, 0).unwrap_err();
        assert_eq!(err.1, "Expected \"and\".");
    }

    /// `&` inside functional pseudo-class arguments: the whole parent list is substituted;
    /// inside the argument the list semantics (flattenVertically) still apply.
    #[test]
    fn pseudo_parent_selector_uses_whole_parent_list() {
        let parents = [
            Sel {
                text: ".a".into(),
                line_break: false,
            },
            Sel {
                text: ".b".into(),
                line_break: false,
            },
        ];
        let one = |child: &str| {
            resolve_selector(child, &parents, false)
                .into_iter()
                .map(|s| s.text)
                .collect::<Vec<_>>()
        };
        assert_eq!(one(":not(&)"), [":not(.a, .b)"]);
        assert_eq!(one(":has(> &)"), [":has(> .a, > .b)"]);
        assert_eq!(one(":nth-child(2n of &)"), [":nth-child(2n of .a, .b)"]);
        assert_eq!(one(":not(&, .x)"), [":not(.a, .x, .b)"]);
        assert_eq!(
            one(":not(& + &)"),
            [":not(.a + .a, .a + .b, .b + .a, .b + .b)"]
        );
        assert_eq!(one(":not(:is(&))"), [":not(:is(.a, .b))"]);
        assert_eq!(one("&:not(&)"), [".a:not(.a, .b)", ".b:not(.a, .b)"]);
        assert_eq!(one(":not(&) &"), [":not(.a, .b) .a", ":not(.a, .b) .b"]);
        assert_eq!(
            one(".c & :not(&)"),
            [".c .a :not(.a, .b)", ".c .b :not(.a, .b)"]
        );
    }

    /// Safety net: no selector in any product may contain an unreplaced `&` (otherwise it
    /// is invalid CSS).
    #[test]
    fn compiled_css_never_contains_parent_selector() {
        let source = r#"
.a { :not(&) { x: 1 } }
.a, .b { :has(> &) { x: 2 } &:not(&) { x: 3 } }
.a, .b { :is(&) & { x: 4 } }
.a { .b { :not(&) :is(&) { x: 5 } } }
.a, .b { @media screen { :not(&) { x: 6 } } }
"#;
        let out = compile(Path::new("t.scss"), source, &Options::default()).expect("compile");
        assert!(
            !out.css.contains('&'),
            "unreplaced `&` left in product:\n{}",
            out.css
        );
    }

    /// Built-in modules: namespace members, `as *`, the `as` alias, module variables.
    #[test]
    fn builtin_modules() {
        let css = |source: &str| {
            compile(Path::new("t.scss"), source, &Options::default())
                .expect("compile")
                .css
        };
        assert_eq!(
            css("@use \"sass:math\";\na { b: math.div(1, 2) }"),
            "a {\n  b: 0.5;\n}"
        );
        assert_eq!(
            css("@use \"sass:math\" as m;\na { b: m.$pi }"),
            "a {\n  b: 3.1415926536;\n}"
        );
        // `as *` merges members into the global scope and **shadows** global functions of
        // the same name (`index("abc","b")` has list semantics globally -> null, string
        // semantics from the module -> 2).
        assert_eq!(
            css("@use \"sass:string\" as *;\na { b: index(\"abc\", \"b\"); c: length(\"abc\") }"),
            "a {\n  b: 2;\n  c: 3;\n}"
        );
        // Built-in modules take no part in file resolution: a user file of the same name
        // does not replace it.
        assert!(
            compile(
                Path::new("t.scss"),
                "@use \"sass:math\";\n@use \"sass:math\";\na { b: 1 }",
                &Options::default()
            )
            .is_err()
        );
    }

    /// Error messages of module functions (matched word for word against 1.35.2).
    #[test]
    fn builtin_module_errors() {
        let err = |source: &str| {
            compile(Path::new("t.scss"), source, &Options::default())
                .unwrap_err()
                .message
        };
        assert_eq!(
            err("@use \"sass:math\";\na { b: math.nope(1) }"),
            "Undefined function."
        );
        assert_eq!(
            err("@use \"sass:color\";\na { b: color.lighten(#123, 10%) }"),
            "The function lighten() isn't in the sass:color module."
        );
        assert_eq!(
            err("@use \"sass:map\";\na { b: map.keys(1) }"),
            "$map: 1 is not a map."
        );
        assert_eq!(
            err("@use \"sass:string\";\na { b: string.quote(a b) }"),
            "$string: a b is not a string."
        );
    }

    /// Counter-intuitive details that the per-function conformance run cannot cover: the key
    /// order of deep-merge and the spacing of list.slash.
    #[test]
    fn builtin_module_quirks() {
        let css = |source: &str| {
            compile(Path::new("t.scss"), source, &Options::default())
                .expect("compile")
                .css
        };
        // dart uses the order of the second map as the skeleton.
        assert_eq!(
            css("@use \"sass:map\" as m;\na { b: inspect(m.deep-merge((a: 1, b: 2), (c: 3))) }"),
            "a {\n  b: (c: 3, a: 1, b: 2);\n}"
        );
        // `list.slash()` serializes as ` / `, while `a/b` in the source has no spaces.
        assert_eq!(
            css("@use \"sass:list\" as l;\na { b: l.slash(1, 2); c: 1/2 }"),
            "a {\n  b: 1 / 2;\n  c: 1/2;\n}"
        );
    }

    /// Module members only available in modern dart-sass (1.93.2): the color space API,
    /// `calc-*`, `string.split`.
    #[test]
    fn builtin_module_modern_members() {
        let css = |source: &str| {
            compile(Path::new("t.scss"), source, &Options::default())
                .expect("compile")
                .css
        };
        assert_eq!(
            css(
                "@use \"sass:color\" as c;\na { b: c.space(#036); c: c.space(oklch(0.5 0.1 20)); d: c.is-legacy(#036); e: c.is-legacy(oklch(0.5 0.1 20)) }"
            ),
            "a {\n  b: rgb;\n  c: oklch;\n  d: true;\n  e: false;\n}"
        );
        // `channel()` returns the channel **with units** (`hue` is `deg`, `lightness` is `%`).
        assert_eq!(
            css(
                "@use \"sass:color\" as c;\na { b: c.channel(#123456, \"red\"); c: c.channel(hsl(120, 50%, 50%), \"hue\"); d: c.channel(oklch(0.5 0.1 20), \"chroma\") }"
            ),
            "a {\n  b: 18;\n  c: 120deg;\n  d: 0.1;\n}"
        );
        assert_eq!(
            css(
                "@use \"sass:color\" as c;\na { b: c.to-space(#036, oklch); c: c.to-gamut(oklch(0.7 0.4 30), $method: clip) }"
            ),
            "a {\n  b: oklch(32.333781389% 0.1025438139 253.8851819964deg);\n  c: oklch(70% 0.4 30deg);\n}"
        );
        assert_eq!(
            css(
                "@use \"sass:color\" as c;\na { b: c.is-in-gamut(oklch(0.7 0.4 30), $space: rgb); c: c.is-powerless(hsl(120, 0%, 50%), \"hue\") }"
            ),
            "a {\n  b: false;\n  c: true;\n}"
        );
        assert_eq!(
            css(
                "@use \"sass:meta\" as m;\n$x: calc(1px + 2%);\na { b: m.calc-name($x); c: m.calc-args($x); d: m.calc-args(min(1px, 2%)) }"
            ),
            "a {\n  b: \"calc\";\n  c: 1px + 2%;\n  d: 1px, 2%;\n}"
        );
        // `$limit` is an upper bound on the **number of splits** (limit 2 -> 3 parts), and
        // the result is a bracketed comma list.
        assert_eq!(
            css(
                "@use \"sass:string\" as s;\na { b: s.split(\"a-b-c\", \"-\", 2); c: s.split(\"abc\", \"\") }"
            ),
            "a {\n  b: [\"a\", \"b\", \"c\"];\n  c: [\"a\", \"b\", \"c\"];\n}"
        );
        assert_eq!(
            css("@use \"sass:math\" as m;\na { b: m.$epsilon; c: m.$max-safe-integer }"),
            "a {\n  b: 0;\n  c: 9007199254740991;\n}"
        );
    }

    /// Escape sequences must be re-serialized in a prelude, and the terminator space must
    /// not be eaten by the whitespace removal at parentheses/commas.
    #[test]
    fn prelude_escapes() {
        assert_eq!(
            normalize_prelude("screen and (min-width: 0\\0 )"),
            "screen and (min-width: 0\\0 )"
        );
        assert_eq!(
            normalize_prelude("screen and (min-width: 0\\0)"),
            "screen and (min-width: 0\\0 )"
        );
        assert_eq!(
            normalize_prelude("(min-width: 4\\25 )"),
            "(min-width: 4\\%)"
        );
        // The first space after `\\61` belongs to the escape; only the second is a real
        // space.
        assert_eq!(normalize_prelude("(a\\61 b)"), "(aab)");
        assert_eq!(normalize_prelude("(a\\61  b)"), "(aa b)");
        assert_eq!(normalize_prelude("(x\\.y)"), "(x\\.y)");
        assert_eq!(normalize_prelude("(x\\9)"), "(x\\9 )");
    }
}
