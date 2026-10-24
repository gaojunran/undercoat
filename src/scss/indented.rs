//! `.sass` 缩进语法 → 等义 SCSS 文本。
//!
//! 为什么在装载期改写文本：`@import` 是**文本级**内联（见 `imports.rs`），整条流水线
//! 共用一段合并文本、一次 oxc 解析（`Syntax::Scss`）。把缩进语法在装载时折成 SCSS，
//! 后续求值/序列化/sourcemap 全部零改动，`.scss` 与 `.sass` 互相 `@import` / `@use`
//! （dart 1.93.2 实测允许混用）也天然成立。
//!
//! 改写只做「结构插入」：块首行尾插 `{`、语句行尾插 `;`、块结束时在最后一行的可插入
//! 位置插 `}`（`//` 行尾注释之前），并把 `=m` / `+m` 归一成 `@mixin m` / `@include m`。
//! **不新增、不删除行**，所以行号与原文一一对应（`imports.rs` 的报错定位与
//! `compiler-style` 的 `align_to_disk` 都能直接沿用），原有字符的列位置也不动。
//!
//! 结构判定逐条对齐 dart-sass 1.93.2 实测：
//! - 缩进更深即子块，缩进回退即关块（可一次关多级）。
//! - 声明与选择器按「首个顶层冒号之后是否紧跟空白」区分：`color: red` 是声明，
//!   `a:hover` / `tr:not(.x)` / `color:red` 都是选择器（老式 `:color red` 同样按选择器，
//!   与 1.93.2 一致，不实现成属性）。
//! - 续行：未配对的 `(`/`[`/`#{`、行尾运算符（`1 +`）、指令与声明行尾的
//!   `and`/`or`/`not`/`in`/`from`/`through`/`to`；逗号续行只属于选择器列表（且下一非空行
//!   不左移）；续行必须比语句首行更深。
//! - 行首 `//` 注释吞掉更深的续行（dart 的 comment continuation），这里把这些行前缀
//!   `//` 保留成注释，免得它们变成真正的语句。
//! - `//` 只在括号外才是行注释（`url(http://x)` 不是）；`/* */` 未在本行闭合时，
//!   注释体由缩进界定，收尾在最后一行补 `*/`。
//! - 没有子块的语句给空块或分号（`{}` 由序列化层丢弃，与 dart 一致）；
//!   `@import ../a/b.sass` 这种不带引号的 URL 补引号后交给统一解析。
//!
//! `oxc-css-parser` 的 `Syntax::Sass` 与 dart 有两处行为差异（`tests/sass_conversion_ast.rs`
//! 的 `oxc_*` 探针逐条钉住）：它只认同缩进更深的选择器续行；块内的 `//` 注释行会被它
//! 当成块结束。本模块按 dart 语义实现，AST 门禁对这些差异做折行/去注释豁免。
//!
//! 已知偏差（都不影响语义）：
//! - 行号与插入点之前的列号精确；`=m` / `+m` 行会被改写成长度不同的 `@mixin ` /
//!   `@include `，该行上的报错列号偏移 6 / 8（行号仍精确）。
//! - 跨行 `/* */` 的续行保留原缩进，dart 会重排成 ` * …`（注释，语义等价）。

/// 一条输出行。`code` 是「可插入位置」：行尾 `//` 注释 / 未闭合 `/*` 之前，
/// 终止符与关块都插在这里（插在注释之后会被注释吃掉）。
struct Line {
    text: String,
    code: usize,
    appendable: bool,
}

impl Line {
    fn new(text: String, code: usize) -> Self {
        Self {
            text,
            code,
            appendable: true,
        }
    }

    fn push_code(&mut self, s: &str) {
        self.text.insert_str(self.code, s);
        self.code += s.len();
    }
}

/// 语句类型：决定逗号续行与 `=`/`+` 归一化。
#[derive(Clone, Copy, PartialEq)]
enum Kind {
    AtRule,
    MixinDef,
    Include,
    Decl,
    Selector,
    Comment,
}

/// 一个未结束的语句。
struct Pending {
    indent: usize,
    kind: Kind,
}

/// 一条跨行 `/*` 注释：块首缩进决定它的结束，`last` 是当前最后一行（收尾插 `*/`）。
struct OpenComment {
    indent: usize,
    last: usize,
}

/// 一个打开的块。
struct Block {
    indent: usize,
    line: usize,
}

/// 跨行保留的扫描状态（续行里的字符串 / 括号 / 块注释）。
#[derive(Default)]
struct Scan {
    quote: Option<u8>,
    paren: i32,
    in_block_comment: bool,
}

struct ScanOut {
    /// 终止符插入点。
    code: usize,
    /// 语句可以在此行结束（没有未闭合的字符串 / 括号 / 块注释）。
    complete: bool,
}

/// 把一段 `.sass` 源文本改写成等义 SCSS。
#[doc(hidden)]
pub fn to_scss(src: &str) -> String {
    let lines: Vec<&str> = src.split('\n').collect();
    let blank: Vec<bool> = lines.iter().map(|l| l.trim().is_empty()).collect();
    let indents: Vec<usize> = lines
        .iter()
        .map(|l| l.len() - l.trim_start_matches([' ', '\t']).len())
        .collect();
    // next[i]：i 之后第一个非空行的缩进（判定「子块」与逗号续行）。
    let mut next: Vec<Option<usize>> = vec![None; lines.len()];
    let mut upcoming = None;
    for i in (0..lines.len()).rev() {
        next[i] = upcoming;
        if !blank[i] {
            upcoming = Some(indents[i]);
        }
    }

    let mut out: Vec<Line> = Vec::with_capacity(lines.len());
    let mut blocks: Vec<Block> = Vec::new();
    let mut last_appendable: Option<usize> = None;
    let mut pending: Option<Pending> = None;
    let mut line_comment: Option<usize> = None;
    // 语句级 `/*` 注释：未在本行闭合时，body 由缩进界定（dart 的缩进语法行为）。
    let mut block_comment: Option<OpenComment> = None;
    let mut scan = Scan::default();

    for i in 0..lines.len() {
        let raw = lines[i];
        let indent = indents[i];

        // 缩进回退即结束未闭合的 `/*` 注释（dart：`/*` 的 body 是缩进块）。
        if block_comment
            .as_ref()
            .is_some_and(|c| !blank[i] && indent <= c.indent)
        {
            if let Some(c) = block_comment.take() {
                let target = &mut out[c.last];
                let at = target.code;
                target.text.insert_str(at, " */");
                target.code += 3;
            }
            scan.in_block_comment = false;
        }

        // 行首 `//` 注释吞掉的更深续行：保持为注释。
        if let Some(base) = line_comment {
            if !blank[i] && indent <= base {
                line_comment = None;
            } else {
                let (text, code) = if blank[i] {
                    (raw.to_string(), raw.len())
                } else {
                    (format!("{}// {}", &raw[..indent], &raw[indent..]), indent)
                };
                // 注释行不可追加：`}` 插在 `//` 之前会被注释吃掉，插在之后又没意义。
                let mut line = Line::new(text, code);
                line.appendable = false;
                out.push(line);
                continue;
            }
        }

        if blank[i] {
            out.push(Line::new(raw.to_string(), raw.len()));
            last_appendable = Some(out.len() - 1);
            continue;
        }

        // 行首就落在块注释里：整行不可追加；语句（若有）在注释闭合后继续。
        if scan.in_block_comment {
            let done = scan_line(raw, &mut scan);
            out.push(Line {
                text: raw.to_string(),
                code: done.code,
                appendable: false,
            });
            if scan.in_block_comment {
                if let Some(c) = block_comment.as_mut() {
                    c.last = out.len() - 1;
                }
            } else {
                block_comment = None;
            }
            if done.complete && pending.is_some() {
                let line = out.len() - 1;
                finish_statement(line, &mut out, &mut pending, &mut blocks, next[i]);
                last_appendable = Some(line);
            }
            continue;
        }

        if pending.is_none() {
            let kind = kind_of(&raw[indent..]);
            if kind == Kind::Comment {
                // 语句级注释：不产生语句，也不参与块结构。
                let done = scan_line(raw, &mut scan);
                out.push(Line {
                    text: raw.to_string(),
                    code: done.code,
                    appendable: false,
                });
                if raw[indent..].starts_with("//") {
                    line_comment = Some(indent);
                } else if scan.in_block_comment {
                    block_comment = Some(OpenComment {
                        indent,
                        last: out.len() - 1,
                    });
                }
                continue;
            }
            // 关块：缩进不深于块首即退出（可一次退出多级）。
            while blocks.last().is_some_and(|b| b.indent >= indent) {
                close_block(&mut out, &mut blocks, &mut last_appendable);
            }
            pending = Some(Pending { indent, kind });
        }

        let kind = pending.as_ref().map_or(Kind::Selector, |p| p.kind);
        let stmt_indent = pending.as_ref().map_or(indent, |p| p.indent);
        let done = scan_line(raw, &mut scan);

        let mut line = Line::new(raw.to_string(), done.code);
        if indent < done.code {
            let leading = &raw.as_bytes()[indent..];
            match (kind, leading.first()) {
                (Kind::MixinDef, Some(b'=')) => {
                    line.text.replace_range(indent..indent + 1, "@mixin ");
                    line.code += "@mixin ".len() - 1;
                }
                (Kind::Include, Some(b'+')) => {
                    line.text.replace_range(indent..indent + 1, "@include ");
                    line.code += "@include ".len() - 1;
                }
                (Kind::AtRule, Some(b'@')) => {
                    quote_import(&mut line, indent);
                }
                _ => {}
            }
        }
        line.appendable = !scan.in_block_comment;
        let appendable = line.appendable;
        let code = line.code;
        out.push(line);
        if appendable {
            last_appendable = Some(out.len() - 1);
        }

        if !done.complete || continues(&out[out.len() - 1].text[..code], kind, next[i], stmt_indent)
        {
            continue;
        }
        let line = out.len() - 1;
        finish_statement(line, &mut out, &mut pending, &mut blocks, next[i]);
        last_appendable = Some(line);
    }

    while !blocks.is_empty() {
        close_block(&mut out, &mut blocks, &mut last_appendable);
    }

    let mut text = out
        .iter()
        .map(|l| l.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    if src.ends_with('\n') && !text.ends_with('\n') {
        text.push('\n');
    }
    text
}

/// 把选择器列表的续行折成同一行（只给 AST 门禁当 oracle 用）。
///
/// oxc 的 `Syntax::Sass` 只把**更深缩进**的下一行当成语句续行（dart 同缩进也续），
/// 所以「转换器 vs oxc Sass」的 AST 对拍需要先把续行折成一行，才能比较同一份语义。
/// 只折 `kind == Selector` 且行尾是顶层逗号的行，声明/指令的续行不受影响。
#[doc(hidden)]
pub fn fold_selector_continuations(src: &str) -> String {
    let lines: Vec<&str> = src.split('\n').collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut scan = Scan::default();
    let mut i = 0;
    while i < lines.len() {
        let raw = lines[i];
        let done = scan_line(raw, &mut scan);
        let trimmed = raw[..done.code].trim();
        let foldable = !trimmed.is_empty()
            && !trimmed.starts_with("//")
            && !trimmed.starts_with("/*")
            && !scan.in_block_comment
            && scan.paren == 0
            && trimmed.ends_with(',')
            && kind_of(trimmed) == Kind::Selector;
        if !foldable {
            out.push(raw.to_string());
            i += 1;
            continue;
        }
        // 续行必须**并进同一行**：留空行会让 oxc 认为语句已结束。
        let mut joined = raw.trim_end().to_string();
        i += 1;
        while let Some(j) = (i..lines.len()).find(|&j| !lines[j].trim().is_empty()) {
            let next = lines[j].trim();
            joined.push(' ');
            joined.push_str(next);
            i = j + 1;
            if !next.ends_with(',') {
                break;
            }
        }
        out.push(joined);
    }
    out.join("\n")
}

/// 转换后 SCSS 解析失败：把错误映射回 `.sass` 源位置与 dart 文案。
///
/// 能精确复现的形态优先（dart 1.93.2 实测）：
/// - `{`：`Expected newline.` @ `{`
/// - 语句行尾的顶层逗号（非选择器列表）：dart 的错误位置**就是逗号之后**
///   - 声明 + 更深续行 / 指令 → `Expected identifier.`
///   - 声明 + 不更深 → `expected selector.`
///   - 变量声明（`$x: 1,`）→ `Nothing may be indented beneath a variable declaration.`
///     @ 下一条更深行的内容起点
///
/// 其余情况只保证**行号**与原文一致（改写不增删行），列号裁到该行代码末尾。
pub(crate) fn parse_error(
    src: &str,
    converted: &str,
    at: usize,
    kind: oxc_css_parser::error::ErrorKind,
) -> (usize, String) {
    if let Some(brace) = stray_brace(src) {
        return (brace, "Expected newline.".to_string());
    }
    let index = crate::LineIndex::new(converted);
    let (line, col) = index.line_col(at, converted);
    let (line, col) = (line as usize, col as usize);
    if let Some((off, message)) = trailing_comma_error(src, line) {
        return (off, message);
    }
    (
        source_line_end(src, line, col),
        format!("SCSS 解析失败: {kind}"),
    )
}

/// 0-based 行号 `line` 上的第 `col` 列（列超出该行代码长度时裁到代码末尾）。
fn source_line_end(src: &str, line: usize, col: usize) -> usize {
    let mut start = 0;
    for (i, raw) in src.split('\n').enumerate() {
        if i == line {
            return start + col.min(raw.trim_end().len());
        }
        start += raw.len() + 1;
    }
    src.len()
}

/// 语句行尾的顶层逗号（dart 的报错位置就是它）。返回 `(偏移, dart 文案)`。
fn trailing_comma_error(src: &str, fail_line: usize) -> Option<(usize, String)> {
    let lines: Vec<&str> = src.split('\n').collect();
    let indent_of = |i: usize| lines[i].len() - lines[i].trim_start_matches([' ', '\t']).len();
    let next_line = |i: usize| (i + 1..lines.len()).find(|&j| !lines[j].trim().is_empty());
    let line_start = |i: usize| lines[..i].iter().map(|l| l.len() + 1).sum::<usize>();

    let mut scan = Scan::default();
    let mut offset = 0usize;
    let mut fallback: Option<(usize, String)> = None;
    for (i, raw) in lines.iter().enumerate() {
        let start = offset;
        offset += raw.len() + 1;
        let done = scan_line(raw, &mut scan);
        let code = &raw[..done.code];
        let trimmed = code.trim();
        if trimmed.is_empty()
            || trimmed.starts_with("//")
            || trimmed.starts_with("/*")
            || scan.in_block_comment
            || !trimmed.ends_with(',')
            || scan.paren != 0
        {
            continue;
        }
        // dart 的位置是逗号**之后**（缺失的标识符应在处），所以不加 -1。
        let comma = start + code.trim_end().len();
        let deeper = next_line(i).is_some_and(|j| indent_of(j) > indent_of(i));
        let (at, message) = if trimmed.starts_with('$') {
            let at = next_line(i).map_or(comma, |j| line_start(j) + indent_of(j));
            (
                at,
                "Nothing may be indented beneath a variable declaration.",
            )
        } else if kind_of(trimmed) == Kind::AtRule || deeper {
            (comma, "Expected identifier.")
        } else {
            (comma, "expected selector.")
        };
        let message = message.to_string();
        if i <= fail_line {
            fallback = Some((at, message));
        } else {
            return fallback.or(Some((at, message)));
        }
    }
    fallback
}

/// 第一个不属于 `#{}` / 字符串 / 注释的 `{`（`.sass` 里写 `{}` 就是硬错误）。
pub(crate) fn stray_brace(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut i = 0;
    let mut quote: Option<u8> = None;
    let mut comment = false;
    while i < bytes.len() {
        let c = bytes[i];
        if comment {
            if c == b'*' && bytes.get(i + 1) == Some(&b'/') {
                comment = false;
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        if let Some(q) = quote {
            match c {
                b'\\' => i += 2,
                c if c == q => {
                    quote = None;
                    i += 1;
                }
                _ => i += 1,
            }
            continue;
        }
        match c {
            b'\'' | b'"' => {
                quote = Some(c);
                i += 1;
            }
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                comment = true;
                i += 2;
            }
            b'#' if bytes.get(i + 1) == Some(&b'{') => i += 2,
            b'{' => return Some(i),
            _ => i += 1,
        }
    }
    None
}

/// 缩进语法允许 `@import ../a/b.sass` 这种不带引号的 URL（dart 1.93.2 实测；SCSS 里
/// 同写法会报 `Expected string`），这里补上引号交给统一的 `@import` 解析。
fn quote_import(line: &mut Line, indent: usize) {
    let body = &line.text[indent..line.code];
    let Some(rest) = body.strip_prefix("@import") else {
        return;
    };
    if rest.contains(['"', '\'']) || rest.contains("url(") {
        return;
    }
    let quoted = rest
        .split(',')
        .map(|part| {
            let part = part.trim();
            if part.is_empty() {
                String::new()
            } else {
                format!("\"{part}\"")
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    if quoted.is_empty() {
        return;
    }
    line.text
        .replace_range(indent + "@import".len()..line.code, &format!(" {quoted}"));
    line.code = indent + "@import".len() + 1 + quoted.len();
}

/// 语句结束：缩进更深即 `{`（开块），否则 `;`。
fn finish_statement(
    line: usize,
    out: &mut [Line],
    pending: &mut Option<Pending>,
    blocks: &mut Vec<Block>,
    next_indent: Option<usize>,
) {
    let Some(p) = pending.take() else { return };
    if next_indent.is_some_and(|n| n > p.indent) {
        out[line].push_code("{");
        blocks.push(Block {
            indent: p.indent,
            line,
        });
        return;
    }
    // 没有子块的语句：选择器 / 需要块的指令给 `{}`（dart 对这类空块不产出任何 CSS），
    // 其余（`@import` / `@use` / `@return` 等）给 `;`。
    let head = out[line].text.get(..out[line].code).unwrap_or_default();
    let needs_block =
        matches!(p.kind, Kind::Selector | Kind::MixinDef) || needs_block_at_rule(head);
    out[line].push_code(if needs_block { "{}" } else { ";" });
}

/// 指令是否以「块」为载体（dart：无子块时整条被丢弃，等价于空块）。
fn needs_block_at_rule(head: &str) -> bool {
    let Some(name) = head.trim_start().strip_prefix('@') else {
        return false;
    };
    let name = name
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '-')
        .next()
        .unwrap_or("");
    matches!(
        name.to_ascii_lowercase().as_str(),
        "media"
            | "supports"
            | "if"
            | "else"
            | "each"
            | "for"
            | "while"
            | "mixin"
            | "function"
            | "at-root"
            | "layer"
            | "container"
            | "scope"
            | "document"
            | "page"
            | "property"
            | "starting-style"
            | "keyframes"
            | "font-face"
    )
}

fn close_block(out: &mut [Line], blocks: &mut Vec<Block>, last_appendable: &mut Option<usize>) {
    let Some(block) = blocks.pop() else { return };
    let target = match *last_appendable {
        Some(t) if t >= block.line => t,
        _ => block.line,
    };
    out[target].push_code("}");
    *last_appendable = Some(target);
}

/// 语句是否还要延续到下一行。
///
/// 逗号只在选择器列表里续行（dart：`&:focus,` 后同缩进/更深都续，左移则不续）；
/// 行尾运算符（`1 +`）与 `and` / `in` / `from` 之类关键字也续行。
fn continues(code: &str, kind: Kind, next_indent: Option<usize>, stmt_indent: usize) -> bool {
    let code = code.trim_end();
    let Some(&last) = code.as_bytes().last() else {
        return false;
    };
    if kind == Kind::Selector {
        return match last {
            // 逗号续行：下一非空行不左移（dart：左移即逗号是结尾）。
            b',' => next_indent.is_some_and(|n| n >= stmt_indent),
            // 组合器独占一行（`>` 后接更深的下一行）才续行；行尾组合器（`.a >`）是
            // **完整语句**（dart 实测：`.a >` 的后续行按嵌套处理，不拼选择器）。
            b'>' | b'+' | b'~' => {
                code.trim().len() == 1 && next_indent.is_some_and(|n| n >= stmt_indent)
            }
            _ => false,
        };
    }
    // 其余续行必须比语句首行更深（dart 用缩进判定续行；`@use "x" as *` 这类
    // 完整语句后面跟一个左移的下一行时不能当成续行）。
    if !next_indent.is_some_and(|n| n > stmt_indent) {
        return false;
    }
    match last {
        // 算术 / 比较运算符（`%` 不算：行尾 `96%` 是完整值，不是取模）。
        b'+' | b'*' | b'/' | b'=' | b'>' | b'<' => true,
        // `-` 只在两侧都是空白时当运算符（`margin: -1` 不会误续行）。
        b'-' => code
            .as_bytes()
            .get(code.len().wrapping_sub(2))
            .is_some_and(|c| c.is_ascii_whitespace()),
        _ => {
            // 关键字续行属于指令与声明（`@for $i from 1 through`、`@if a and`、
            // `$x: a and` + 缩进续行）。
            if !matches!(kind, Kind::AtRule | Kind::Decl) {
                return false;
            }
            let word = code
                .rsplit(|c: char| !c.is_ascii_alphanumeric())
                .next()
                .unwrap_or("");
            matches!(
                word,
                "and" | "or" | "not" | "in" | "from" | "through" | "to"
            ) && code.ends_with(&format!(" {word}"))
        }
    }
}

/// 扫描一行：更新跨行状态，给出终止符插入点与「语句能否在此结束」。
fn scan_line(raw: &str, st: &mut Scan) -> ScanOut {
    let bytes = raw.as_bytes();
    let mut i = 0;
    let mut code = raw.len();
    let mut comment_at: Option<usize> = None;
    while i < bytes.len() {
        if st.in_block_comment {
            if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
                st.in_block_comment = false;
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        let c = bytes[i];
        if let Some(q) = st.quote {
            if c == b'\\' {
                i += 2;
                continue;
            }
            if c == q {
                st.quote = None;
            }
            i += 1;
            continue;
        }
        match c {
            b'\'' | b'"' => {
                st.quote = Some(c);
                i += 1;
            }
            b'\\' => i += 2,
            // `//` 只在括号外才是行注释（`url(http://x)` 不是）。
            b'/' if bytes.get(i + 1) == Some(&b'/') && st.paren == 0 => {
                code = raw[..i].trim_end().len();
                break;
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => match raw[i + 2..].find("*/") {
                Some(rel) => i = i + 2 + rel + 2,
                None => {
                    st.in_block_comment = true;
                    comment_at.get_or_insert(raw[..i].trim_end().len());
                    i = bytes.len();
                }
            },
            b'(' | b'[' => {
                st.paren += 1;
                i += 1;
            }
            b')' | b']' => {
                st.paren = (st.paren - 1).max(0);
                i += 1;
            }
            b'#' if bytes.get(i + 1) == Some(&b'{') => {
                st.paren += 1;
                i += 2;
            }
            b'}' => {
                st.paren = (st.paren - 1).max(0);
                i += 1;
            }
            _ => i += 1,
        }
    }
    if st.in_block_comment {
        if let Some(at) = comment_at {
            code = at;
        }
    }
    ScanOut {
        code,
        complete: st.quote.is_none() && st.paren <= 0 && !st.in_block_comment,
    }
}

/// 语句类型（dart 1.93.2 实测：缩进语法里 `prop:value` 无空格即选择器）。
fn kind_of(body: &str) -> Kind {
    let bytes = body.as_bytes();
    match bytes.first() {
        None => Kind::Selector,
        Some(b'@') => Kind::AtRule,
        Some(b'=') => Kind::MixinDef,
        Some(b'+') if bytes.get(1).is_some_and(|c| is_name_start(*c)) => Kind::Include,
        Some(b'/') if matches!(bytes.get(1), Some(b'/') | Some(b'*')) => Kind::Comment,
        _ => match first_top_level_colon(body) {
            Some(at) if bytes.get(at + 1).is_none_or(|c| c.is_ascii_whitespace()) => Kind::Decl,
            _ => Kind::Selector,
        },
    }
}

fn is_name_start(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'-' || c >= 0x80
}

/// 首个顶层（不在字符串 / 括号 / 插值里）冒号的位置。
fn first_top_level_colon(body: &str) -> Option<usize> {
    let bytes = body.as_bytes();
    let mut i = 0;
    let mut paren = 0i32;
    let mut quote: Option<u8> = None;
    while i < bytes.len() {
        let c = bytes[i];
        if let Some(q) = quote {
            match c {
                b'\\' => i += 2,
                c if c == q => {
                    quote = None;
                    i += 1;
                }
                _ => i += 1,
            }
            continue;
        }
        match c {
            b'\'' | b'"' => {
                quote = Some(c);
                i += 1;
            }
            b'\\' => i += 2,
            b'(' | b'[' => {
                paren += 1;
                i += 1;
            }
            b')' | b']' => {
                paren -= 1;
                i += 1;
            }
            b'#' if bytes.get(i + 1) == Some(&b'{') => {
                paren += 1;
                i += 2;
            }
            b'}' => {
                paren -= 1;
                i += 1;
            }
            b':' if paren == 0 => return Some(i),
            _ => i += 1,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::to_scss;

    /// 块首行尾插 `{`、语句行尾插 `;`、关块在最后一条语句尾插 `}`（文件末尾的空行承载）。
    #[test]
    fn nested_rule_and_decl() {
        assert_eq!(to_scss(".a\n  color: red\n"), ".a{\n  color: red;\n}\n");
    }

    #[test]
    fn mixin_shorthand() {
        assert_eq!(
            to_scss("=m($p: 1px)\n  padding: $p\n.a\n  +m(2px)\n"),
            "@mixin m($p: 1px){\n  padding: $p;}\n.a{\n  @include m(2px);\n}\n"
        );
    }

    #[test]
    fn selector_comma_continuation_keeps_lines() {
        assert_eq!(
            to_scss(".a,\n.b\n  color: red\n"),
            ".a,\n.b{\n  color: red;\n}\n"
        );
    }

    /// dart 1.93.2 实测：`prop:value`（冒号后无空白）在选择器位置，不是声明；
    /// 没有子块的嵌套选择器给空块（两侧都不产出 CSS）。
    #[test]
    fn declaration_without_space_is_selector() {
        assert_eq!(to_scss(".a\n  color:red\n"), ".a{\n  color:red{}\n}\n");
    }

    #[test]
    fn nested_property_block() {
        assert_eq!(
            to_scss(".a\n  font:\n    family: serif\n"),
            ".a{\n  font:{\n    family: serif;\n}}\n"
        );
    }

    #[test]
    fn if_else_chain() {
        assert_eq!(
            to_scss("@if $x\n  a: 1\n@else\n  a: 2\n"),
            "@if $x{\n  a: 1;}\n@else{\n  a: 2;\n}\n"
        );
    }

    /// 行首 `//` 注释吞掉更深的续行（dart 的 comment continuation），这里保持为注释。
    #[test]
    fn line_comment_block_swallows_deeper_lines() {
        assert_eq!(
            to_scss(".a\n  color: red\n  // note\n    deeper\n.b\n  color: blue\n"),
            ".a{\n  color: red;}\n  // note\n    // deeper\n.b{\n  color: blue;\n}\n"
        );
    }

    /// 终止符插在行尾 `//` 注释之前，否则会被注释吃掉。
    #[test]
    fn trailing_comment_keeps_terminator_before_it() {
        assert_eq!(
            to_scss(".a\n  color: red // note\n  background: blue\n"),
            ".a{\n  color: red; // note\n  background: blue;\n}\n"
        );
    }

    /// 值的续行靠行尾运算符，且必须更深。
    #[test]
    fn operator_continuation() {
        assert_eq!(
            to_scss("$x: 1 +\n  2\n.a\n  width: $x\n"),
            "$x: 1 +\n  2;\n.a{\n  width: $x;\n}\n"
        );
    }

    /// 完整语句后面跟左移的下一行时不能续行（`@use "x" as *` 是典型）。
    #[test]
    fn complete_statement_never_continues_on_dedent() {
        assert_eq!(
            to_scss("@use \"a/star\" as *\n.a\n  color: red\n"),
            "@use \"a/star\" as *;\n.a{\n  color: red;\n}\n"
        );
    }

    #[test]
    fn blank_lines_are_preserved() {
        assert_eq!(
            to_scss(".a\n  color: red\n\n  background: blue\n"),
            ".a{\n  color: red;\n\n  background: blue;\n}\n"
        );
    }

    /// 未闭合的 `/*` 由缩进回退收尾（dart 的缩进语法行为），不能吞掉后面的文件内容。
    #[test]
    fn block_comment_closed_by_dedent() {
        assert_eq!(
            to_scss("/* a\n  b\n.c\n  color: red\n"),
            "/* a\n  b */\n.c{\n  color: red;\n}\n"
        );
    }

    /// `@keyframes` 的 `from` / `to` 是选择器，不是续行关键字。
    #[test]
    fn keyframe_selectors_are_not_keyword_continuations() {
        assert_eq!(
            to_scss("@keyframes s\n  from\n    opacity: 0\n  to\n    opacity: 1\n"),
            "@keyframes s{\n  from{\n    opacity: 0;}\n  to{\n    opacity: 1;\n}}\n"
        );
    }

    /// `url(http://x)` 里的 `//` 不是行注释。
    #[test]
    fn slashes_inside_parens_are_not_comments() {
        assert_eq!(
            to_scss(".a\n  background: url(http://x/y.png)\n"),
            ".a{\n  background: url(http://x/y.png);\n}\n"
        );
    }

    /// 没有子块的语句：选择器给空块（dart 不产出任何 CSS），`@import` 给分号。
    #[test]
    fn bodyless_statement_gets_empty_block_or_semicolon() {
        assert_eq!(
            to_scss(".gone\n.kept\n  color: red\n"),
            ".gone{}\n.kept{\n  color: red;\n}\n"
        );
        assert_eq!(
            to_scss("@import \"a\"\n.b\n  color: red\n"),
            "@import \"a\";\n.b{\n  color: red;\n}\n"
        );
    }

    /// 声明的值也能靠行尾 `and` + 更深缩进续行（dart 实测）。
    #[test]
    fn declaration_value_continues_on_keyword() {
        assert_eq!(
            to_scss("$a: true and\n  false\n.b\n  c: 1\n"),
            "$a: true and\n  false;\n.b{\n  c: 1;\n}\n"
        );
    }
}
