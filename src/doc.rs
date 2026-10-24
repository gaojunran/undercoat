//! 输出中间表示与打印器：两个方言（scss / less）共用的产物层。
//!
//! 求值阶段产出 [`Item`] 树，[`print`] 按 [`Style`] 描述的形态把它写成 CSS（并同时
//! 喂 sourcemap）。方言差异只允许落在 [`Style`] 的字段上；语义差异（空规则删除、
//! 空 `@media` 删除、`@font-face` 保留 `{}`、`//` 注释丢弃等）留在各自的求值侧。

use oxc_css_parser::pos::Span;

use crate::Options;
use crate::merge::Merged;
use crate::print::{Printer, SourceSegment};

/// 产物形态配置：两个方言唯一允许不一致的地方。
pub(crate) struct Style {
    /// 每条 style rule（含 group_end 标记的组）之后是否补一个空行。
    pub(crate) blank_line_after_rule: bool,
    /// 选择器列表续行是否缩进到当前层级；false = 续行写在第 0 列。
    pub(crate) indent_selector_continuation: bool,
    /// 非 ASCII 内容时是否在开头补 `@charset "UTF-8";`。
    pub(crate) charset: bool,
    /// 产物末尾是否补换行。
    pub(crate) trailing_newline: bool,
    /// sourcemap 是否带 `sourcesContent`。两侧的产出条件**相反**（P11 已记录）：
    /// dart（scss）恒写（WDT 的 render 参数恒带 `sourceMapContents: true`）；
    /// less 4.1.3 只在多源时写（`outputSourceFiles` 跟 `@import` 内联走），
    /// 单源 `.map` 里根本没有这个字段。less 侧调用时按 `multi_source` 覆盖。
    pub(crate) sources_content: bool,
    /// 块注释是否原样写出（保留源码的换行与续行缩进）。
    ///
    /// true（less）：less 的注释节点直接回写原文，续行不重排；
    /// false（dart）：按 `write_with_indent` 剥掉 `min(最小缩进, 注释起始列)` 再缩进。
    pub(crate) verbatim_comments: bool,
}

impl Style {
    /// dart-sass 1.35.2 的 expanded 产物形态（SCSS 通道）。
    pub(crate) const SASS: Style = Style {
        blank_line_after_rule: true,
        indent_selector_continuation: false,
        charset: true,
        trailing_newline: false,
        sources_content: true,
        verbatim_comments: false,
    };

    /// less 4.1.3 的产物形态（LESS 通道）。四个字段都是从 lessc 实测反推的：
    ///
    /// - `blank_line_after_rule = false`：相邻规则 / 注释 / `@media` 之间都只换行
    ///   （`tests/fixtures/basic.css` 是 `}\n.a {`；bootstrap 3.4.1 的 144329 字节
    ///   产物里 `\n\n\n` 出现 0 次）。
    /// - `indent_selector_continuation = true`：选择器续行缩进到当前层级
    ///   （`tests/fixtures/media-indent.css` 的 `@media print {` 里是 `  *,\n  *:before`；
    ///   根层续行落在第 0 列，即 `.a,\n.b`）。
    /// - `charset = false`：less 不按非 ASCII 补 `@charset`——weui-wxss 2.6.26 的产物
    ///   含非 ASCII 注释却没有 `@charset`；源码里写的 `@charset` 由求值侧当普通
    ///   at-rule 原样输出（less 只保留第一条，见 `less::Compiler::charset`）。
    /// - `trailing_newline = true`：非空产物恒以单个 `\n` 结尾（fixture 全部如此），
    ///   空输入产出 0 字节（`items` 为空时不补，见 [`print`]）。
    /// - `sources_content`：基准值 `false`（单源不写），less 侧调用时按 `multi_source`
    ///   覆盖——`@import` 内联后才与 dart 一样写 `sourcesContent`（实测：`basic.less`
    ///   单源 map 无该字段，`import-multiple.less` 有）。
    /// - `verbatim_comments = true`：less 的块注释直接回写源码字节（含续行缩进），
    ///   不做 dart 的续行重排。
    pub(crate) const LESS: Style = Style {
        blank_line_after_rule: false,
        indent_selector_continuation: true,
        charset: false,
        trailing_newline: true,
        sources_content: false,
        verbatim_comments: true,
    };
}

/// 选择器列表：**逐输出行**的文本 + 该行的源锚点。
///
/// 拆行的唯一目的是让打印器给续行也打上 segment：dart 把列表的每一行都映射到
/// 列表起点（同一段文本连写多行时每行拿到同一个源位置），所以这里各行共用
/// 同一个 `Span`。文本逐字节等于未拆行的 `a,\nb` 形态。
pub(crate) type SelectorLines = Vec<(String, Span)>;

/// 把整段选择器文本按输出行拆成 [`SelectorLines`]（每行同一锚点）。
pub(crate) fn selector_lines(text: &str, span: Span) -> SelectorLines {
    text.split('\n')
        .map(|line| (line.to_string(), span))
        .collect()
}

#[derive(Clone)]
pub(crate) struct Rule {
    pub(crate) selector: SelectorLines,
    pub(crate) body: Body,
    /// dart-sass 的 `isGroupEnd`：该源规则（含其嵌套产物）的最后一个节点，
    /// 序列化后跟一个空行。
    pub(crate) group_end: bool,
    /// 扩展引擎里的编号（`@extend` 用它回写选择器）。
    pub(crate) extend_id: Option<usize>,
    /// 扩展后只剩占位符选择器：这条规则不产出输出。
    pub(crate) placeholder: bool,
}

/// `@keyframes` 内部的 keyframe 规则：与 style rule 不同，彼此之间**没有**空行。
#[derive(Clone)]
pub(crate) struct Keyframe {
    pub(crate) selector: SelectorLines,
    pub(crate) body: Body,
}

#[derive(Clone)]
pub(crate) struct AtRule {
    pub(crate) name: String,
    pub(crate) name_span: Span,
    pub(crate) prelude: String,
    pub(crate) body: Option<Body>,
    pub(crate) group_end: bool,
    /// 嵌套 `@media` 合并后要「提到外层 media 之外」成为兄弟节点的标记。
    pub(crate) hoist: bool,
}

#[derive(Clone)]
pub(crate) struct Comment {
    pub(crate) text: String,
    pub(crate) span: Span,
    /// 源码里 `/*` 所在的 0-based 列（dart 的注释重排以它为上界）。
    pub(crate) column: usize,
}

#[derive(Clone)]
pub(crate) struct Decl {
    pub(crate) name: String,
    pub(crate) name_span: Span,
    pub(crate) value: String,
    pub(crate) value_span: Span,
    /// 源码里是不是自定义属性（`--x: ...`）。只按源码判定：`#{$p}: $v` 里
    /// `$v` 是普通 SassScript，输出仍带 `: `。
    pub(crate) custom: bool,
    /// 自定义属性值里带换行时的重排参数（dart 的 `_writeReindentedValue`），
    /// 缩进量取决于声明所在的输出层级，所以只记录要剥掉的列数。
    pub(crate) custom_strip: Option<CustomStrip>,
}

/// 自定义属性值的重排方式（dart 的 `_writeReindentedValue` 三个分支）。
#[derive(Clone, Copy)]
pub(crate) enum CustomStrip {
    /// 有换行但没有带缩进的行：`trim_end` 后补一个空格。
    Trailing,
    /// 每行剥掉 `n` 列（`min(最小缩进, 声明名所在列)`）。
    Indent(usize),
}

#[derive(Clone)]
pub(crate) enum Item {
    Rule(Rule),
    Keyframe(Keyframe),
    AtRule(AtRule),
    Comment(Comment),
    /// 原样输出的片段（less 迁移期用）：文本 + 内部 `(字节偏移, span)` 锚点。
    Raw(Raw),
}

/// 原样片段：任意 CSS 文本 + 内部 `(字节偏移, span)` 锚点（`marks` 按偏移升序，
/// 每段从该偏移写到下一个 mark 或文本末尾）。
///
/// 只给「无法用结构化节点表达」的四类文本用（less 侧，见 `less/emit.rs`）：
///
/// 1. `(reference)` 紧凑体（`a: 1;b: 2;`）：less 的 `ruleset.genCSS` 对不可见声明
///    不插换行，而 `Body::Decls` 的打印恒是一行一条；
/// 2. `@import (inline) "x"` 的文件内容：不解析、原文写出；
/// 3. `@import "x.less"` 的子编译产物：一段完整 CSS（含自己的换行与缩进）；
/// 4. 根层裸函数调用 `e("…")` 的内文：求值结果本身就是一段 CSS 文本。
///
/// 注释/声明/规则一律走结构化节点（形态差异由 [`Style`] 表达）。
#[derive(Clone)]
pub(crate) struct Raw {
    pub(crate) text: String,
    pub(crate) marks: Vec<(usize, Span)>,
}

impl From<Raw> for Item {
    fn from(raw: Raw) -> Self {
        Item::Raw(raw)
    }
}

#[derive(Clone)]
pub(crate) enum Body {
    Rules(Vec<Item>),
    Decls(Vec<DeclItem>),
}

#[derive(Clone)]
pub(crate) enum DeclItem {
    Decl(Decl),
    Comment(Comment),
    /// 规则内的无块 at-rule（dart 只保留 `@import "x.css"` 这一种，缩进写在花括号里）。
    AtRule(AtRule),
}

impl Item {
    pub(crate) fn is_hoisted(&self) -> bool {
        matches!(self, Item::AtRule(at) if at.hoist)
    }

    /// 摘掉 hoist 标记；同一源规则拆出来的段之间不留空行。
    pub(crate) fn clear_hoist(&mut self) {
        if let Item::AtRule(at) = self {
            at.hoist = false;
            at.group_end = false;
        }
    }

    fn is_group_end(&self) -> bool {
        match self {
            Item::Rule(rule) => rule.group_end,
            Item::AtRule(at) => at.group_end,
            Item::Keyframe(_) | Item::Comment(_) | Item::Raw(_) => false,
        }
    }

    /// 序列化前扫描非 ASCII：决定是否补 `@charset "UTF-8";`。
    pub(crate) fn is_ascii(&self) -> bool {
        match self {
            Item::Rule(r) => {
                r.selector.iter().all(|(text, _)| text.is_ascii()) && r.body.is_ascii()
            }
            Item::Keyframe(k) => {
                k.selector.iter().all(|(text, _)| text.is_ascii()) && k.body.is_ascii()
            }
            Item::AtRule(a) => {
                a.name.is_ascii()
                    && a.prelude.is_ascii()
                    && a.body.as_ref().is_none_or(Body::is_ascii)
            }
            Item::Comment(c) => c.text.is_ascii(),
            Item::Raw(r) => r.text.is_ascii(),
        }
    }
}

impl Body {
    pub(crate) fn is_ascii(&self) -> bool {
        match self {
            Body::Rules(items) => items.iter().all(Item::is_ascii),
            Body::Decls(items) => items.iter().all(|item| match item {
                DeclItem::Decl(d) => d.name.is_ascii() && d.value.is_ascii(),
                DeclItem::Comment(c) => c.text.is_ascii(),
                DeclItem::AtRule(a) => a.name.is_ascii() && a.prelude.is_ascii(),
            }),
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        match self {
            Body::Rules(items) => items.is_empty(),
            Body::Decls(items) => items.is_empty(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Ctx {
    Rules,
    Keyframes,
}

/// 打印整个样式表。`style` 决定空行 / 续行缩进 / `@charset` / 尾随换行 /
/// `sourcesContent` / 注释是否原样输出。
///
/// 多文件（`@import` 内联）时把所有源文件与段表交给打印器：token 的 span 全是
/// 合并串坐标，收尾时二分回「哪个文件 + 文件内偏移」再换算行列。
pub(crate) fn print(
    items: &[Item],
    style: &Style,
    entry: &str,
    merged: &Merged,
    opts: &Options,
) -> (String, Option<String>) {
    let entry_text = merged.files.first().map_or("", |f| f.text.as_str());
    let mut printer = Printer::new(entry, entry_text, opts.source_map);
    for file in merged.files.iter().skip(1) {
        printer.add_file(&file.path.to_string_lossy(), &file.text);
    }
    // `sourcesContent`：两侧产出条件相反（见 `Style::sources_content`）。
    // 写入的是 `original`：less 的 `text` 是词法修补后的版本（span 坐标系），原文另存。
    if style.sources_content {
        for (i, file) in merged.files.iter().enumerate() {
            printer.set_content(i as u32, &file.original);
        }
    }
    if merged.files.len() > 1 {
        printer.set_segments(
            merged
                .segs
                .iter()
                .map(|s| SourceSegment {
                    start: s.start,
                    end: s.end,
                    file: s.file,
                    orig_start: s.orig,
                })
                .collect(),
        );
    }
    if style.charset && !items.iter().all(Item::is_ascii) {
        printer.raw("@charset \"UTF-8\";\n");
    }
    emit_items(items, style, Ctx::Rules, 0, &mut printer);
    if style.trailing_newline && !items.is_empty() {
        printer.newline();
    }
    printer.finish(opts)
}

fn emit_items(items: &[Item], style: &Style, ctx: Ctx, indent: usize, printer: &mut Printer) {
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            printer.newline();
            // 空行属于「前一个规则组（源规则 + 其嵌套产物）的收尾」。
            if style.blank_line_after_rule && ctx == Ctx::Rules && items[i - 1].is_group_end() {
                printer.newline();
            }
        }
        emit_item(item, style, indent, printer);
    }
}

fn emit_item(item: &Item, style: &Style, indent: usize, printer: &mut Printer) {
    match item {
        Item::Rule(rule) => {
            emit_selector(&rule.selector, style, indent, printer);
            printer.raw(" {");
            emit_body(&rule.body, style, indent + 1, Ctx::Rules, printer);
            printer.raw("}");
        }
        Item::Keyframe(keyframe) => {
            emit_selector(&keyframe.selector, style, indent, printer);
            printer.raw(" {");
            emit_body(&keyframe.body, style, indent + 1, Ctx::Keyframes, printer);
            printer.raw("}");
        }
        Item::AtRule(at) => emit_at_rule(at, style, indent, printer),
        Item::Comment(comment) => emit_comment(comment, style, indent, printer),
        Item::Raw(raw) => emit_raw(raw, indent, printer),
    }
}

/// 选择器输出：首行带缩进；续行默认从第 0 列开始（dart 实测：`@media` 里的多行
/// 选择器续行也不缩进），`indent_selector_continuation` 打开时缩进到当前层级。
/// 每行各自 `token`，续行才有 segment。
fn emit_selector(selector: &SelectorLines, style: &Style, indent: usize, printer: &mut Printer) {
    for (i, (text, span)) in selector.iter().enumerate() {
        if i == 0 {
            printer.indent(indent);
        } else {
            printer.newline();
            if style.indent_selector_continuation {
                printer.indent(indent);
            }
        }
        printer.token(text, *span);
    }
}

/// 原样片段：整段按当前缩进写一次，`marks` 按字节偏移切段并各自打映射。
fn emit_raw(raw: &Raw, indent: usize, printer: &mut Printer) {
    printer.indent(indent);
    let text = raw.text.as_str();
    let mut cursor = 0;
    for (i, &(offset, span)) in raw.marks.iter().enumerate() {
        if offset > cursor {
            printer.raw(&text[cursor..offset]);
        }
        let end = raw.marks.get(i + 1).map_or(text.len(), |&(next, _)| next);
        printer.token(&text[offset..end], span);
        cursor = end;
    }
    if cursor < text.len() {
        printer.raw(&text[cursor..]);
    }
}

/// 注释序列化。dart 侧用与自定义属性值同一套重排（dart 的 `_writeWithIndent`）：
/// 续行先剥掉 `min(最小缩进, 注释起始列)`，再补上当前缩进，空行保持真空行；
/// less 侧（[`Style::verbatim_comments`]）直接回写源码字节。
///
/// dart 给注释的每一行都打 segment（全部指向 `/*` 起点），这里逐行 `token`；
/// less 的注释在旧实现里是不映射的原文，所以原样分支整条 `raw`（保持 map 字节）。
fn emit_comment(comment: &Comment, style: &Style, indent: usize, printer: &mut Printer) {
    let text = format!("/*{}*/", comment.text);
    printer.indent(indent);
    if style.verbatim_comments {
        printer.raw(&text);
        return;
    }
    match minimum_indentation(&text) {
        None => printer.token(&text, comment.span),
        Some(min) => {
            let strip = min.min(comment.column);
            printer.token(
                text.split_once('\n').map_or(&text, |(first, _)| first),
                comment.span,
            );
            // `write_with_indent` 以 `\n` 开头：续行逐行拆开，空行只留换行。
            let rest = write_with_indent(&text, strip, indent);
            for line in rest.split('\n').skip(1) {
                printer.newline();
                if !line.is_empty() {
                    printer.token(line, comment.span);
                }
            }
        }
    }
}

/// 换行之后「最浅的非空行」的缩进（字符计列）。没有换行返回 `None`。
fn minimum_indentation(text: &str) -> Option<usize> {
    let mut lines = text.split('\n');
    lines.next()?;
    lines
        .filter_map(|line| {
            let lead = line.len() - line.trim_start_matches([' ', '\t']).len();
            (!line.trim().is_empty()).then_some(lead)
        })
        .min()
}

/// dart 的 `_writeWithIndent`：把 [text] 第一行之后的内容逐行按 [strip] 剥离后
/// 重新缩进到 [indent] 层（第一行由调用方输出）。空行只输出换行符（不留空白）；
/// 文本以空白结尾时压缩成一个空格。
fn write_with_indent(text: &str, strip: usize, indent: usize) -> String {
    let pad = "  ".repeat(indent);
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len() + pad.len());
    let mut i = 0;
    // 跳到第一行末尾。
    while i < chars.len() && chars[i] != '\n' {
        i += 1;
    }
    while i < chars.len() {
        let mut newlines = 0usize;
        let mut line_start = i;
        loop {
            if i >= chars.len() {
                // 结尾只剩空白：dart 换成单个空格（自定义属性靠它保持分隔）。
                out.push(' ');
                return out;
            }
            let ch = chars[i];
            i += 1;
            match ch {
                ' ' | '\t' => continue,
                '\n' => {
                    newlines += 1;
                    line_start = i;
                }
                _ => {
                    i -= 1;
                    break;
                }
            }
        }
        for _ in 0..newlines {
            out.push('\n');
        }
        out.push_str(&pad);
        let skip = (line_start + strip).min(i);
        out.extend(chars[skip..].iter().take_while(|&&ch| ch != '\n'));
        while i < chars.len() && chars[i] != '\n' {
            i += 1;
        }
    }
    out
}

fn emit_at_rule(at: &AtRule, style: &Style, indent: usize, printer: &mut Printer) {
    printer.indent(indent);
    // dart 把 at-rule 整条映射到 `@` 所在列（`name_span` 指向名字本身）。
    printer.token(
        &format!("@{}", at.name),
        Span {
            start: at.name_span.start.saturating_sub(1),
            end: at.name_span.end,
        },
    );
    if !at.prelude.is_empty() {
        printer.raw(" ");
        printer.raw(&at.prelude);
    }
    match &at.body {
        None => printer.raw(";"),
        Some(body) if body.is_empty() => printer.raw(" {}"),
        Some(body) => {
            let ctx = if at.name.to_ascii_lowercase().ends_with("keyframes") {
                Ctx::Keyframes
            } else {
                Ctx::Rules
            };
            printer.raw(" {");
            emit_body(body, style, indent + 1, ctx, printer);
            printer.raw("}");
        }
    }
}

fn emit_body(body: &Body, style: &Style, indent: usize, ctx: Ctx, printer: &mut Printer) {
    if body.is_empty() {
        return;
    }
    printer.newline();
    match body {
        Body::Rules(items) => emit_items(items, style, ctx, indent, printer),
        Body::Decls(items) => {
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    printer.newline();
                }
                match item {
                    DeclItem::Decl(decl) => {
                        printer.indent(indent);
                        printer.token(&decl.name, decl.name_span);
                        if decl.custom {
                            // 自定义属性保留原文（空白已折叠），冒号后不额外插空格。
                            printer.raw(":");
                            let tail = match decl.custom_strip {
                                None => std::borrow::Cow::Borrowed(decl.value.as_str()),
                                Some(CustomStrip::Trailing) => {
                                    std::borrow::Cow::Owned(format!("{} ", decl.value.trim_end()))
                                }
                                Some(CustomStrip::Indent(strip)) => {
                                    let first = decl
                                        .value
                                        .split_once('\n')
                                        .map_or(decl.value.as_str(), |(first, _)| first);
                                    printer.raw(first);
                                    std::borrow::Cow::Owned(write_with_indent(
                                        &decl.value,
                                        strip,
                                        indent,
                                    ))
                                }
                            };
                            printer.raw(&tail);
                        } else {
                            printer.raw(": ");
                            printer.token(&decl.value, decl.value_span);
                        }
                        printer.raw(";");
                    }
                    DeclItem::Comment(comment) => emit_comment(comment, style, indent, printer),
                    DeclItem::AtRule(at) => emit_at_rule(at, style, indent, printer),
                }
            }
        }
    }
    printer.newline();
    printer.indent(indent.saturating_sub(1));
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn render(src: &str) -> String {
        crate::scss::compile(std::path::Path::new("t.scss"), src, &Options::default())
            .unwrap()
            .css
    }

    /// 逃生舱：`Raw` 整段原样输出（含非 ASCII），`marks` 在对应偏移处打 segment。
    #[test]
    fn raw_is_printed_and_mapped() {
        let source = "中文 raw";
        let mut merged = Merged::new();
        merged.add_file(Path::new("t.scss"), source.to_string(), source.to_string());
        let items = vec![Item::from(Raw {
            text: "中文\n.x".to_string(),
            marks: vec![
                (0, Span { start: 0, end: 0 }),
                (7, Span { start: 3, end: 5 }),
            ],
        })];
        assert!(!items.iter().all(Item::is_ascii), "非 ASCII 要能被扫到");

        let opts = Options {
            source_map: true,
            ..Default::default()
        };
        let (css, map) = print(&items, &Style::SASS, "t.scss", &merged, &opts);
        assert_eq!(css, "@charset \"UTF-8\";\n中文\n.x");
        let map: serde_json::Value = serde_json::from_str(&map.expect("map")).unwrap();
        // 两段各一条映射（charset 行没有映射）。
        let mapped_lines = map["mappings"]
            .as_str()
            .unwrap()
            .split(';')
            .filter(|line| !line.is_empty())
            .count();
        assert_eq!(mapped_lines, 2);
    }

    #[test]
    fn blank_line_after_style_rule() {
        assert_eq!(
            render(".a{color:red}.b{color:blue}"),
            ".a {\n  color: red;\n}\n\n.b {\n  color: blue;\n}"
        );
        assert_eq!(
            render(".a{color:red}@media screen{.b{color:blue}}"),
            ".a {\n  color: red;\n}\n\n@media screen {\n  .b {\n    color: blue;\n  }\n}"
        );
        // at-rule → rule 没有空行
        assert_eq!(
            render("@media screen{.a{color:red}}.b{color:blue}"),
            "@media screen {\n  .a {\n    color: red;\n  }\n}\n.b {\n  color: blue;\n}"
        );
        // @media 内相邻 rule 之间有空行
        assert_eq!(
            render("@media screen{.a{color:red}.b{color:blue}}"),
            "@media screen {\n  .a {\n    color: red;\n  }\n\n  .b {\n    color: blue;\n  }\n}"
        );
    }

    /// D1：声明内部（属性名与值之间 / 值之后到 `;` 或 `}`）的注释整段丢弃，
    /// 只有语句级注释保留。
    #[test]
    fn comments_inside_declarations_are_dropped() {
        assert_eq!(render(".a{width:1px /* t */ }"), ".a {\n  width: 1px;\n}");
        assert_eq!(
            render(".a{width:1px /* t */; color:red}"),
            ".a {\n  width: 1px;\n  color: red;\n}"
        );
        assert_eq!(render(".a{width: /* t */ 1px }"), ".a {\n  width: 1px;\n}");
        assert_eq!(
            render(".a{width:1px /* t */ !important}"),
            ".a {\n  width: 1px !important;\n}"
        );
        // 语句级保留（`;` 之后、块首、块尾）
        assert_eq!(
            render(".a{color:red; /* t */}"),
            ".a {\n  color: red;\n  /* t */\n}"
        );
        assert_eq!(
            render(".a{ /* c1 */ color:red; /* c2 */ }"),
            ".a {\n  /* c1 */\n  color: red;\n  /* c2 */\n}"
        );
    }

    /// D2：只有 `nth-child` / `nth-last-child` 归一 An+B；`nth-of-type` 不动。
    #[test]
    fn anb_selector_normalization() {
        assert_eq!(
            render(":nth-child(2n + 1){x:1}"),
            ":nth-child(2n+1) {\n  x: 1;\n}"
        );
        assert_eq!(
            render(":nth-child( -n + 3 ){x:1}"),
            ":nth-child(-n+3) {\n  x: 1;\n}"
        );
        assert_eq!(
            render(":nth-child(3n- 1){x:1}"),
            ":nth-child(3n-1) {\n  x: 1;\n}"
        );
        assert_eq!(
            render(":nth-child(odd){x:1}"),
            ":nth-child(odd) {\n  x: 1;\n}"
        );
        assert_eq!(
            render(":nth-child(2n + 1 of .a, .b){x:1}"),
            ":nth-child(2n+1 of .a, .b) {\n  x: 1;\n}"
        );
        // nth-of-type / nth-last-of-type 实测不归一（空白按普通选择器折叠）
        assert_eq!(
            render(":nth-of-type( -n + 3 ){x:1}"),
            ":nth-of-type(-n + 3) {\n  x: 1;\n}"
        );
        assert_eq!(
            render(":nth-of-type(2N+1){x:1}"),
            ":nth-of-type(2N+1) {\n  x: 1;\n}"
        );
        // 非 An+B 伪类参数不受影响
        assert_eq!(render(":not(.a + .b){x:1}"), ":not(.a + .b) {\n  x: 1;\n}");
    }

    /// D3：根层 `@import` 前移到开头注释段之后，其它节点相对顺序不变。
    #[test]
    fn root_imports_are_hoisted() {
        assert_eq!(
            render("@namespace svg url(http://x);@import url(x.css);.a{v:1}"),
            "@import url(x.css);\n@namespace svg url(http://x);\n.a {\n  v: 1;\n}"
        );
        assert_eq!(
            render("@layer a;@import url(x.css);.a{v:1}"),
            "@import url(x.css);\n@layer a;\n.a {\n  v: 1;\n}"
        );
        assert_eq!(
            render("@page{margin:1cm}@import url(x.css);@namespace svg url(http://x);"),
            "@import url(x.css);\n@page {\n  margin: 1cm;\n}\n@namespace svg url(http://x);"
        );
        // 开头的注释不跨过，import 插在它后面
        assert_eq!(
            render("/* lead */@import url(a.css);.a{v:1}"),
            "/* lead */\n@import url(a.css);\n.a {\n  v: 1;\n}"
        );
        // 尾随 import 被前移，但注释保持与原语句的相对顺序
        assert_eq!(
            render(".a{v:1}/* c */@import url(x.css);"),
            "@import url(x.css);\n.a {\n  v: 1;\n}\n\n/* c */"
        );
        // 嵌套在 @media 里的 import 不前移
        assert_eq!(
            render("@media screen{@import url(a.css);}.a{v:1}"),
            "@media screen {\n  @import url(a.css);\n}\n.a {\n  v: 1;\n}"
        );
    }

    #[test]
    fn comment_rules() {
        assert_eq!(
            render("/*! keep */\n.a{color:red}"),
            "/*! keep */\n.a {\n  color: red;\n}"
        );
        assert_eq!(render("// drop\n.a{color:red}"), ".a {\n  color: red;\n}");
        assert_eq!(
            render(".a{color:red}/* c */.b{color:blue}"),
            ".a {\n  color: red;\n}\n\n/* c */\n.b {\n  color: blue;\n}"
        );
        // 带注释的空规则保留
        assert_eq!(render(".a{/* c */}"), ".a {\n  /* c */\n}");
    }

    #[test]
    fn empty_rules_removed() {
        assert_eq!(render(".a{}\n.b{color:red}"), ".b {\n  color: red;\n}");
        assert_eq!(render("@media screen{}"), "");
        // 非条件 at-rule 的空块保留
        assert_eq!(render("@font-face{}"), "@font-face {}");
        assert_eq!(render("@foo bar{}"), "@foo bar {}");
    }

    #[test]
    fn charset_and_no_trailing_newline() {
        assert_eq!(render(".a{color:red}"), ".a {\n  color: red;\n}");
        assert_eq!(
            render(".a{content:\"中\"}"),
            "@charset \"UTF-8\";\n.a {\n  content: \"中\";\n}"
        );
        assert!(!render(".a{color:red}").ends_with('\n'));
    }

    #[test]
    fn declarations_and_custom_properties() {
        assert_eq!(
            render(".a{color:red!important;--x: 1px  2px}"),
            ".a {\n  color: red !important;\n  --x: 1px 2px;\n}"
        );
        assert_eq!(
            render(".a{background:url(\"a b.png\") no-repeat}"),
            ".a {\n  background: url(\"a b.png\") no-repeat;\n}"
        );
    }

    #[test]
    fn attribute_selector_unquotes_identifiers() {
        assert_eq!(
            render("input[type=\"button\"], [class*=\"col-\"], [href^=\"javascript:\"]{color:red}"),
            "input[type=button], [class*=col-], [href^=\"javascript:\"] {\n  color: red;\n}"
        );
        assert_eq!(
            render("[type = \"button\"], [type=\"--x\"], [type=\"1\"]{color:red}"),
            "[type=button], [type=\"--x\"], [type=\"1\"] {\n  color: red;\n}"
        );
    }

    #[test]
    fn custom_property_whitespace() {
        assert_eq!(
            render(".a{--x: 1px  2px;--y:a,b }"),
            ".a {\n  --x: 1px 2px;\n  --y:a,b ;\n}"
        );
        assert_eq!(
            render(".a{--x: a !important}"),
            ".a {\n  --x: a !important;\n}"
        );
        // 续行按「源列 - 声明列 + 输出缩进」重排（dart-sass 实测）
        assert_eq!(render(".a{--x: a\n    b}"), ".a {\n  --x: a\n   b;\n}");
    }

    #[test]
    fn keyframes_have_no_blank_lines_between_blocks() {
        assert_eq!(
            render("@keyframes x{from{opacity:0}to{opacity:1}}"),
            "@keyframes x {\n  from {\n    opacity: 0;\n  }\n  to {\n    opacity: 1;\n  }\n}"
        );
    }

    #[test]
    fn empty_at_rule_blocks() {
        // media / supports 的空块整块删除；layer / font-face 等保留
        assert_eq!(render("@media screen{}"), "");
        assert_eq!(render("@supports (a:b){}"), "");
        assert_eq!(render("@layer base{}"), "@layer base {}");
    }

    #[test]
    fn raw_function_arguments() {
        assert_eq!(
            render(
                ".a{filter:alpha(opacity = 65);x:progid:DXImageTransform.Microsoft.gradient(enabled = false)}"
            ),
            ".a {\n  filter: alpha(opacity=65);\n  x: progid:DXImageTransform.Microsoft.gradient(enabled = false);\n}"
        );
    }

    #[test]
    fn source_map_records_declarations() {
        let opts = Options {
            source_map: true,
            ..Default::default()
        };
        let out =
            crate::scss::compile(std::path::Path::new("t.scss"), ".a{color:red}", &opts).unwrap();
        let map: serde_json::Value = serde_json::from_str(&out.map.unwrap()).unwrap();
        assert_eq!(map["version"], 3);
        assert_eq!(map["sources"][0], "t.scss");
        assert!(!map["mappings"].as_str().unwrap().is_empty());
    }

    #[test]
    fn selector_normalization() {
        assert_eq!(
            render(".a>.b,.c   .d{color:red}"),
            ".a > .b, .c .d {\n  color: red;\n}"
        );
        assert_eq!(render(".a,\n.b{color:red}"), ".a,\n.b {\n  color: red;\n}");
        assert_eq!(
            render(":not( .a , .b ){color:red}"),
            ":not(.a, .b) {\n  color: red;\n}"
        );
    }
}
