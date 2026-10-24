//! SCSS 编译入口：装配（`@import` / `@use`）→ 解析（oxc `Syntax::Scss`）→ 求值 → 序列化。
//!
//! `.sass` 缩进语法在装载期由 [`indented::to_scss`] 折成等义 SCSS 文本，
//! 因此这里的合并文本永远是 SCSS 形态（解析器固定 `Syntax::Scss`）。
//!
//! S1 覆盖变量 / 作用域 / 嵌套 / 插值 / `@import` / `@use`（最小）与驱动它们的
//! SassScript 表达式求值；mixin / 函数 / 控制流仍显式报错，不静默产出错误产物。

pub(crate) mod builtins;
mod color_space;
pub(crate) mod eval;
pub(crate) mod extend;
pub(crate) mod imports;
// 测试钩子（`tests/sass_conversion_ast.rs` 直接调用），非产品 API。
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

/// mixin 递归上限（dart-sass 自身在 300 层左右栈溢出；这里显式报错而不是崩）。
const MAX_MIXIN_DEPTH: usize = 256;

/// 编译 SCSS / 纯 CSS。`path` 只用于报错、依赖解析与 sourcemap 的 `sources`。
pub(crate) fn compile(path: &Path, source: &str, opts: &Options) -> Result<Output, Box<CompileError>> {
    // ParserBuilder 会自行剥 BOM，这里先剥掉以保证 span 与本函数的 src 对齐。
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

    // 合并文本永远是 SCSS 形态：`.sass` 片段在 `imports::Loader` 装载时就已改写。
    let alloc = Allocator::default();
    let mut parser = ParserBuilder::new(&alloc, src)
        .syntax(Syntax::Scss)
        .comments()
        .build();
    let ast = parser
        .parse::<Stylesheet>()
        .map_err(|e| merged.error_at(e.span.start, format!("SCSS 解析失败: {}", e.kind)))?;
    // 声明类 at-rule 的容器校验（dart 在解析期做，与执行路径无关）。
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

/// 根层 CSS `@import` 前移到「开头的注释/import 段」之后（dart-sass 实测）。
///
/// 只有出现在其它语句之后的 import 才前移；移动保持彼此相对顺序，
/// 开头的注释不跨过（`/* c */ @import` 保持注释在前）。
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

/// 一个选择器列表：输出文本 + 拆开后的各条选择器（供嵌套解析）。
struct SelectorSet {
    parts: Vec<Sel>,
}

/// 一条复杂选择器 + dart-sass `ComplexSelector.lineBreak`（序列化时在它之前换行）。
struct Sel {
    text: String,
    line_break: bool,
}

impl SelectorSet {
    fn is_empty(&self) -> bool {
        self.parts.is_empty()
    }

    /// 序列化选择器列表；`keep` 用来滤掉占位符选择器（`%p`）。
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

/// 一个形参（`@mixin` / `@function` 共用，定义在 `eval` 里）。
type Param<'a> = eval::Param<'a>;

/// mixin 定义。`depth` 是定义处的帧数：调用时把帧栈截到这里（词法闭包），
/// 这样 mixin 体看不到调用方的局部变量，但能看到定义处的环境。
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

/// `@include m { ... }` 的 content 块：按调用方环境求值（词法闭包）。
/// `tail` 是在 `@include` 处从调用方帧栈里摘下的部分（`def.scope == 调用方 scope` 时才有），
/// 每次 `@content` 时换回调用方环境，执行完再把变动写回这里。
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
    /// 创建这个块时「生效中」的 content 块（dart 的 `_contentBlock` 捕获）：
    /// 块里的 `@content` 指向它，而不是指向自己。
    outer: Option<usize>,
}

struct Converter<'a> {
    merged: &'a Merged,
    opts: &'a Options,
    eval: Evaluator<'a>,
    modules: &'a HashMap<PathBuf, ModuleInfo>,
    /// 文件 id → 作用域编号（`@use` 的模块有自己的作用域）。
    scope_of: &'a HashMap<u32, usize>,
    /// 作用域 → 拥有它的模块文件（其余文件都是 `@import` 内联进来的）。
    module_file: &'a HashMap<usize, u32>,
    comments: Vec<CommentRec>,
    cursor: usize,
    /// 每个作用域一套 mixin 帧（与变量帧同步压/弹）。
    mixins: Vec<Vec<HashMap<String, MixinDef<'a>>>>,
    /// 每次 mixin 调用的 content 块槽位（`None` = 该 mixin 没收到 content 块）。
    /// 栈顶是「当前生效」的 content 块，与 dart 的 `_contentBlock` 一致。
    content: Vec<Option<usize>>,
    /// content 块本体（用索引引用，重复 `@content` 需要多次执行同一个块）。
    content_blocks: Vec<ContentBlock<'a>>,
    /// 当前 mixin 调用嵌套层数（递归上限）。
    mixin_depth: usize,
    /// 当前控制指令（`@if`）嵌套层数：mixin 定义禁止出现在里面。
    control_depth: usize,
    in_mixin: bool,
    /// 声明列表上下文（`@font-face` 等）：允许裸声明。
    decl_context: bool,
    /// 嵌套属性块（`font: { family: serif }`）的属性名前缀。
    decl_prefix: Option<String>,
    /// 当前嵌套深度（根 stylesheet 为 1）：`@use` 只允许在 1。
    block_depth: usize,
    /// 外层 `@media` 的 prelude 栈（嵌套 media 要并成一条查询）。
    media_stack: Vec<String>,
    /// `@extend` 记录：全部编译完后统一作用到已产出的规则上。
    extensions: extend::Engine,
    /// 块里最后一条**嵌套规则**有没有产出节点（空规则也算一个子节点，会「吃掉」
    /// 空行标记：dart 的 `isGroupEnd` 落在最后一个子节点上，空规则随序列化一起消失）。
    empty_tail: bool,
    /// 当前块里是否出现过嵌套规则（决定 `empty_tail` 是看子节点还是看自己）。
    had_child: bool,
    /// 当前块里「等位置」的 hoisted 项（跨层合并的 media）。
    /// dart 先关闭当前规则再吐这类节点，所以要把它们攒到声明之后。
    pending_hoist: Vec<Vec<Item>>,
}

impl<'a> Converter<'a> {
    fn src(&self) -> &'a str {
        &self.merged.text
    }

    /// 语句所在文件的作用域（`@import` 内联的文件与导入者同作用域）。
    fn scope_at(&self, at: usize) -> usize {
        *self.scope_of.get(&self.file_at(at)).unwrap_or(&0)
    }

    /// 合并偏移 → 文件 id；未命中时回退到最后一个段（越界定位的 scss 语义）。
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

    /// `@at-root <selector> { ... }`：选择器相对于根解析，产物提到当前规则之外
    /// （`@at-root a#{&}` 用来给 `.badge` 生成 `a.badge`）。
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
                        "SCSS 特性尚未实现（S3）: @at-root 的 query 形式",
                    ));
                }
            },
            // 裸 `@at-root { ... }`：子规则按根层解析，但 `&` 仍是外层选择器。
            None => (None, at.span),
            _ => return Err(self.err(at.span.start, "SCSS 解析失败: @at-root")),
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
        // `@at-root .x { ... }`：块里的 `&` 就是 `.x`；裸形式则仍是外层选择器。
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
        // `@at-root` 的产物是根层节点：dart 会给最后一个子节点打上分组结尾。
        if !tail_empty {
            if let Some(item) = items.last_mut() {
                mark_group_end(item);
            }
        }
        out.extend(items);
        // `@at-root` 产出的空规则也是「一个子节点」：会吃掉外层规则的空行标记。
        self.empty_tail = tail_empty;
        self.had_child = true;
        Ok(())
    }

    /// `@extend <compound> [!optional]`：只登记，编译结束后统一作用（dart 同样是
    /// 全局的扩展表，后来的 `@extend` 会影响先前产出的规则）。
    fn extend_rule(
        &mut self,
        at: &'a oxc_css_parser::ast::AtRule<'a>,
        parent: Option<&SelectorSet>,
    ) -> Result<(), Box<CompileError>> {
        let Some(AtRulePrelude::SassExtend(extend)) = &at.prelude else {
            return Err(self.err(at.span.start, "SCSS 解析失败: @extend"));
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

    /// 编译结束：扩展传递闭包 → 作用到每条规则 → `!optional` 校验。
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

    /// 一个完整块：执行语句，把声明按源码顺序封成若干条同选择器规则。
    ///
    /// 嵌套规则 / at-rule 之后再次出现的声明要另起一条规则（dart-sass 1.78 起
    /// 的混合声明语义），所以这里不做「声明统一提到前面」。
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
        // 块内被 hoist 出外层 at-rule 的 media 排在块自己的规则之后（dart 先关
        // 闭当前规则再吐 hoisted 节点）：`@media {.b { @media M{} y:2 }}` 先出
        // `.b { y: 2 }` 再出 M。
        self.flush_pending_hoist(&mut decls, parent, parent_span, &mut nested)?;
        self.flush_rule(&mut decls, parent, parent_span, &mut nested)?;
        if let Some(pending) = self.pending_hoist.pop() {
            nested.extend(pending);
        }
        out.extend(nested);
        Ok(())
    }

    /// 吐出一批「等位置」的 hoisted 项：先把它们之前的声明封成规则。
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

    /// 把已收集的声明封成一条规则追加到 `nested`（空缓冲不动）。
    ///
    /// 调用点：块收尾，以及每次嵌套规则 / at-rule 产出之前——保证声明留在
    /// 源码里它出现的位置，而不是被提到所有嵌套产出之前。
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
        // 占位符选择器（`%p`）不直接产出 CSS：留给扩展阶段决定去向。
        let selector = selectors.render(|_| true);
        if !selector.is_empty() {
            // 以「登记时刻」的形态进入扩展引擎：此刻已有的扩展会立即作用，
            // 之后的扩展只作用到已登记的规则上（dart 的增量语义）。
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

    /// 语句序列。收集器由调用方给：mixin 体 / `@if` 体的声明要并进调用处那条规则。
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
            // 后续语句不是声明了：先把攒着的 hoisted 项连同在前声明一起吐出。
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
            Statement::KeyframeBlock(_) => {
                Err(self.err(st.span().start, "SCSS 特性尚未实现（S1）: 裸 keyframe 块"))
            }
            Statement::SassIfAtRule(rule) => self.if_rule(rule, parent, parent_span, decls, nested),
            // oxc 把 SCSS 自己的控制流/定义类 at-rule 归到 `UnknownSassAtRule`。
            Statement::UnknownSassAtRule(at) => Err(self.err(
                at.span.start,
                format!(
                    "SCSS 特性尚未实现（S2）: @{}",
                    ident_name(&at.name).unwrap_or("?")
                ),
            )),
            Statement::AtRule(at) => self.sass_at_rule(at, parent, parent_span, decls, nested),
            other => Err(self.err(
                st.span().start,
                format!(
                    "SCSS 特性尚未实现（S1）: {}",
                    snippet(self.src(), other.span().start, other.span().end)
                ),
            )),
        }
    }

    /// SCSS 专属 at-rule 与普通 CSS at-rule 的分派。
    fn sass_at_rule(
        &mut self,
        at: &'a oxc_css_parser::ast::AtRule<'a>,
        parent: Option<&SelectorSet>,
        parent_span: Span,
        decls: &mut Vec<DeclItem>,
        nested: &mut Vec<Item>,
    ) -> Result<(), Box<CompileError>> {
        // 按名字分派：`@content` 无参数时 prelude 是 `None`，不能靠 prelude 判别。
        let lower = at.name.name.to_ascii_lowercase();
        // 嵌套属性块里只允许声明与这些控制指令（dart 的 `_declarationAtRule`）。
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
            // `@use` / `@forward` 由装配层保证在顶层，这里只是成员登记。
            "use" => return self.use_rule(at),
            "forward" => return self.forward_rule(at),
            _ => {}
        }
        // 规则内的无块 `@import "x.css"`：dart 原样留在花括号里，不外提。
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
        // 只有真的产出了东西才切段：`@extend` / `@debug` 之类不打断声明顺序。
        if items.is_empty() {
            return Ok(());
        }
        // 层内被 hoist 的 media 要等本块声明先落地（见 `flush_pending_hoist`）。
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

    /// 压一层词法帧（变量帧 + mixin 帧一一对应），返回作用域编号供 `pop_scope` 用。
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

    /// 注释游标：第一个位置 >= `at` 的注释下标。
    fn comment_cursor(&self, at: usize) -> usize {
        self.comments.partition_point(|c| c.span.start < at)
    }

    /// 嵌套规则：选择器相对父选择器解析后作为独立规则输出。
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
        // 空行只加在「源级规则」的最后一个产物之后（嵌套规则之间不空行）。
        if root_rule && !tail_empty {
            if let Some(item) = out[before..].last_mut() {
                mark_group_end(item);
            }
        }
        Ok(())
    }

    fn keyframe(&mut self, block: &'a KeyframeBlock<'a>) -> Result<Option<Item>, Box<CompileError>> {
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
                // `@use` 只能在文件顶层（dart-sass 同款文案）。
                if self.block_depth > 1 {
                    return Err(self.err(at.span.start, "This at-rule is not allowed here."));
                }
                return self.use_rule(at);
            }
            "debug" => {
                // `@debug` 用 inspect 形态（map / 无引号字符串 / 颜色都按 Sass 写法）。
                if let Some(AtRulePrelude::SassExpr(expr)) = &at.prelude {
                    let value = self
                        .eval
                        .eval_component(self.src(), expr)
                        .map_err(|e| self.eval_err(e))?;
                    let (path, line, _) = self.merged.position(at.span.start);
                    let message = match &value {
                        // dart 的 `Value.toString()`：顶层带引号的字符串去掉引号，
                        // 其它值走 inspect（map/list 里的字符串仍带引号）。
                        value::Value::Str(s) if s.quoted => s.text.clone(),
                        value => value.inspect(),
                    };
                    eprintln!("{path}:{line} DEBUG: {message}");
                }
                return Ok(());
            }
            "warn" => {
                // `@warn` 用 CSS 形态（map 之类不可输出为 CSS 的值直接报错，dart 实测）。
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
                    // 列号按 dart 的报错格式从 1 开始。
                    eprintln!(
                        "WARNING: {message}\n    {path} {line}:{}  root stylesheet\n",
                        column + 1
                    );
                }
                return Ok(());
            }
            "error" => {
                // 报错正文是值的 inspect 形态（`@error "boom"` → `"boom"`）。
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
            // 函数体外的 `@return` 已由静态容器校验拦下（这里只是兵底）。
            "return" => return Err(self.err(at.span.start, "This at-rule is not allowed here.")),
            "extend" => return self.extend_rule(at, parent),
            "forward" => {
                if self.block_depth > 1 {
                    return Err(self.err(at.span.start, "This at-rule is not allowed here."));
                }
                return self.forward_rule(at);
            }
            // 没有可用 prelude 节点的 `@include` / `@content`（解析降级时）显式报错。
            "mixin" | "include" | "content" => {
                return Err(self.err(at.span.start, format!("SCSS 解析失败: @{lower}")));
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
                // 媒体查询的值是 SassScript：裸 `$var` 也要代入。
                let text = self.substitute_vars(&interpolated, base)?;
                normalize_prelude(&text)
            } else if lower == "import" {
                normalize_import_urls(&normalize_prelude(&interpolated))
            } else {
                normalize_prelude(&interpolated)
            }
        };

        // 嵌套 `@media` 按 dart 的规则并成一条查询（交叉相乘；类型冲突的组合丢弃）。
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
                        "SCSS 特性尚未实现（S1）: @keyframes 内的非常规语句",
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
        // 合并过的（或内含合并过的）media 要提到外层 media 之外：按顺序拆段，
        // 段内重新包回本条 media，被 hoist 的子项成为兄弟节点。
        if let Some(Body::Rules(items)) = rule.body.clone() {
            if items.iter().any(Item::is_hoisted) {
                let mut current: Vec<Item> = Vec::new();
                let mut segments: Vec<Item> = Vec::new();
                // 被 hoist 的子项把外层 media 拆开：只在新旧块交界处分段，
                // 拆点之后的连续兄弟重新攒成一块（1.35.2 是每条规则一块，
                // 现代 dart-sass 只在 hoist 处断开）。
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
        assert!(err.message.contains("尚未实现"), "{}", err.message);
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

    /// 属性选择器去引号只看值本身能不能当标识符：能则去，且**不能**吃掉后面的后代空格。
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

    /// `@supports` 是结构化打印：冗余括号塌掉，但分组作为 and/or 操作数时留一层
    /// （与 dart-sass 1.35.2 的 `_parenthesize` 逐条对齐）。
    #[test]
    fn supports_condition_parens() {
        fn supports(prelude: &str) -> String {
            let mut eval = Evaluator::new(1);
            supports_prelude(prelude, &mut eval, 0).expect("prelude 应能打印")
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
        // 同级混用 and/or 在 dart 里是句法错误
        let mut eval = Evaluator::new(1);
        let err = supports_prelude("(a:1) and (b:2) or (c:3)", &mut eval, 0).unwrap_err();
        assert_eq!(err.1, "Expected \"and\".");
    }

    /// 函数式伪类参数里的 `&`：整份父列表代入；参数内部仍按列表语义 flattenVertically。
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

    /// 兜底：任何产物的选择器里都不允许出现未替换的 `&`（否则就是无效 CSS）。
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
        assert!(!out.css.contains('&'), "产物残留 `&`:\n{}", out.css);
    }

    /// 内置模块：命名空间成员、`as *`、`as` 别名、模块变量。
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
        // `as *` 把成员并入全局，且**遮蔽**同名全局函数（`index("abc","b")` 全局是
        // list 语义 → null，模块是 string 语义 → 2）。
        assert_eq!(
            css("@use \"sass:string\" as *;\na { b: index(\"abc\", \"b\"); c: length(\"abc\") }"),
            "a {\n  b: 2;\n  c: 3;\n}"
        );
        // 内置模块不参与文件解析：同名用户文件不会顶替它。
        assert!(
            compile(
                Path::new("t.scss"),
                "@use \"sass:math\";\n@use \"sass:math\";\na { b: 1 }",
                &Options::default()
            )
            .is_err()
        );
    }

    /// 模块函数的报错文案（1.35.2 逐字对齐）。
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

    /// 逐函数对拍覆盖不到的「反直觉」细节：deep-merge 的键顺序、list.slash 的空格。
    #[test]
    fn builtin_module_quirks() {
        let css = |source: &str| {
            compile(Path::new("t.scss"), source, &Options::default())
                .expect("compile")
                .css
        };
        // dart 以第二张 map 的顺序为骨架。
        assert_eq!(
            css("@use \"sass:map\" as m;\na { b: inspect(m.deep-merge((a: 1, b: 2), (c: 3))) }"),
            "a {\n  b: (c: 3, a: 1, b: 2);\n}"
        );
        // `list.slash()` 序列化成 ` / `，源码里的 `a/b` 不带空格。
        assert_eq!(
            css("@use \"sass:list\" as l;\na { b: l.slash(1, 2); c: 1/2 }"),
            "a {\n  b: 1 / 2;\n  c: 1/2;\n}"
        );
    }

    /// 现代 dart-sass（1.93.2）才有的模块成员：颜色空间 API、`calc-*`、`string.split`。
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
        // `channel()` 返回**带单位**的通道（`hue` 是 `deg`、`lightness` 是 `%`）。
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
        // `$limit` 是**切分次数**上限（limit 2 → 3 段），结果是括号化逗号 list。
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

    /// 转义序列在 prelude 里要重新序列化，终止空格不能被括号/逗号处的去空格吃掉。
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
        // `\\61` 后的第一个空格属于转义，第二个才是真空格。
        assert_eq!(normalize_prelude("(a\\61 b)"), "(aab)");
        assert_eq!(normalize_prelude("(a\\61  b)"), "(aa b)");
        assert_eq!(normalize_prelude("(x\\.y)"), "(x\\.y)");
        assert_eq!(normalize_prelude("(x\\9)"), "(x\\9 )");
    }
}
