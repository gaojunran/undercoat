//! 值求值：变量替换、四则运算、颜色与内置函数。
//!
//! 对拍基线是 less@4.1.3（`math: parens-division` 默认）：`/` 在被括号包裹时才计算，
//! 否则原样输出；`*` `+` `-` 总是计算。

use std::path::{Path, PathBuf};

use oxc_css_parser::ast::*;

/// 变量绑定：AST 节点（源文件内）、文本（globalVars）或颜色。
///
/// 颜色单独成一类：less 的变量的值就是 Color 对象，`darken(@c, 10%)` 用的是浮点通道，
/// 若先写成 `#rrggbb` 再解析会丢掉亚整数部分（`#286090` 会变成 `#285f8f`）。
#[derive(Clone)]
pub(crate) enum VarRef<'a> {
    Ast(&'a ComponentValue<'a>),
    Text(String),
    /// 列表值（`@arguments`：实参之间的分组必须保留，`length`/`extract` 依赖它）。
    List(Vec<String>),
    Color(Color),
    /// detached ruleset 或作为规则集使用的 mixin 调用结果（`@dr()` / `@p[key]`）。
    Ruleset(std::rc::Rc<crate::less::RuleSet<'a>>),
}

/// 属性查找结果：单条声明，或 `+:` / `+_:` 合并后的分组值。
#[derive(Clone)]
pub(crate) enum PropSource<'a> {
    Decl(&'a Declaration<'a>),
    /// 外层逗号分组、内层空格分隔（less `_mergeRules` 的产物）。
    Merged(Vec<Vec<&'a Declaration<'a>>>),
}

/// 变量/属性查找（作用域链由调用方实现）。
pub(crate) trait Scope<'a> {
    fn var(&self, name: &str) -> Option<VarRef<'a>>;
    fn prop(&self, name: &str) -> Option<Vec<PropSource<'a>>>;
    /// 变量声明值末尾的块注释：less 把注释当值节点，引用处要带出来。
    fn var_suffix(&self, _name: &str) -> Option<String> {
        None
    }
    /// `$prop` 没有命中：less 报 `Property '$prop' is undefined`，调用方据此记录错误。
    fn prop_miss(&self, _name: &str, _span: oxc_css_parser::pos::Span) {}
    /// `@var` 没有命中：less 报 `variable @var is undefined`。
    ///
    /// 只在真正求值的引用处调用：`isdefined(@x)`、未展开的 detached ruleset 等探测场景
    /// 直接把 span 传 `None`，不产生错误。
    fn var_miss(&self, _name: &str, _span: oxc_css_parser::pos::Span) {}
    /// 文件类函数（`data-uri` / `image-size`）的查找根：入口 less 文件目录 + `paths`。
    fn file_roots(&self) -> Option<(&Path, &[PathBuf])> {
        None
    }
}

fn text<'a>(src: &'a str, cv: &ComponentValue) -> &'a str {
    text_of(*cv.span(), src)
}

fn text_of(span: oxc_css_parser::pos::Span, src: &str) -> &str {
    let (lo, hi) = (span.start, span.end);
    if lo <= hi && hi <= src.len() {
        &src[lo..hi]
    } else {
        ""
    }
}

/// 未命中变量的占位文本：按需记录 `variable @name is undefined`。
fn undefined_var<'a>(
    name: &str,
    scope: &dyn Scope<'a>,
    span: Option<oxc_css_parser::pos::Span>,
) -> String {
    if let Some(span) = span {
        scope.var_miss(name, span);
    }
    format!("@{name}")
}

/// `isdefined(expr)`：less 的 `isdefined` 关闭实参求值，求值失败即 false。
pub(crate) fn probe_defined<'a>(cv: &ComponentValue<'a>, scope: &dyn Scope<'a>) -> bool {
    match cv {
        ComponentValue::LessVariable(v) => scope.var(v.name.name).is_some(),
        ComponentValue::LessVariableCall(c) => scope.var(c.variable.name.name).is_some(),
        // `@x[...]`：先确认变量存在，再管查找结果
        ComponentValue::LessNamespaceValue(ns) => match &ns.callee {
            LessNamespaceValueCallee::LessVariable(v) => scope.var(v.name.name).is_some(),
            LessNamespaceValueCallee::LessMixinCall(_) => true,
        },
        _ => true,
    }
}

/// `isruleset(expr)`：`expr` 是不是 detached ruleset（变量指向它也算）。
pub(crate) fn probe_ruleset<'a>(cv: &ComponentValue<'a>, scope: &dyn Scope<'a>) -> bool {
    match cv {
        ComponentValue::LessDetachedRuleset(_) => true,
        ComponentValue::LessVariable(v) => matches!(
            scope.var(v.name.name),
            Some(VarRef::Ast(ComponentValue::LessDetachedRuleset(_)))
        ),
        ComponentValue::LessVariableCall(c) => matches!(
            scope.var(c.variable.name.name),
            Some(VarRef::Ast(ComponentValue::LessDetachedRuleset(_)))
        ),
        _ => false,
    }
}

pub(crate) fn eval<'a>(
    cv: &ComponentValue<'a>,
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
) -> String {
    if depth > 32 {
        return text(src, cv).to_string();
    }
    match cv {
        // 数值一律按源文本重新解析成 f64：AST 里的 f32 会丢精度（0.7 → 0.69999999）
        ComponentValue::Number(_) => {
            fmt_number(parse_number(text(src, cv)).map(|(v, _)| v).unwrap_or(0.0))
        }
        ComponentValue::Dimension(d) => match parse_number(text(src, cv)) {
            Some((v, u)) => format!(
                "{}{}",
                fmt_number(v),
                u.unwrap_or_else(|| d.unit.name.to_string())
            ),
            None => text(src, cv).to_string(),
        },
        ComponentValue::Percentage(_) => format!(
            "{}%",
            fmt_number(parse_number(text(src, cv)).map(|(v, _)| v).unwrap_or(0.0))
        ),
        ComponentValue::HexColor(_) => text(src, cv).to_string(),
        ComponentValue::LessVariable(v) => {
            var_value(v.name.name, src, scope, depth, Some(*cv.span()))
        }
        ComponentValue::LessVariableCall(call) => {
            var_value(call.variable.name.name, src, scope, depth, Some(*cv.span()))
        }
        // `@@b`：先求 @b 的值作为变量名，再查 @<值>
        ComponentValue::LessVariableVariable(vv) => {
            let inner = var_name(
                vv.variable.name.name,
                src,
                scope,
                depth + 1,
                Some(*cv.span()),
            );
            match scope.var(&inner) {
                Some(VarRef::Ast(node)) => eval(node, src, scope, depth + 1),
                Some(VarRef::List(v)) => v.join(" "),
                Some(VarRef::Text(t)) => t,
                Some(VarRef::Color(c)) => c.to_css(),
                Some(VarRef::Ruleset(_)) => String::new(),
                None => undefined_var(&inner, scope, Some(*cv.span())),
            }
        }
        // `$prop`：同作用域（含外层规则）里同名属性的值
        ComponentValue::LessPropertyVariable(p) => {
            match prop_text(p.name.name, src, scope, depth) {
                Some(text) => text,
                None => {
                    scope.prop_miss(p.name.name, p.span);
                    format!("${}", p.name.name)
                }
            }
        }
        ComponentValue::LessBinaryOperation(op) => eval_binary(op, src, scope, depth),
        // math=parens-division：括号内的除法要算，括号外原样保留
        ComponentValue::LessParenthesizedOperation(p) => {
            eval_paren(&p.operation, src, scope, depth + 1)
        }
        // `~"..."` → 去掉 ~ 与引号，字符串内仍做 `@{}` / `${}` 插值
        ComponentValue::LessEscapedStr(_) => {
            // 取源文本以保留转义（`~"\9"` 里的 `\9` 不能被解码成制表符）
            let raw = text(src, cv);
            let inner = raw.strip_prefix('~').unwrap_or(raw);
            interpolate(unquote_ref(inner), src, scope, depth, Some(*cv.span()))
        }
        ComponentValue::InterpolableStr(InterpolableStr::LessInterpolated(s)) => {
            interpolated_str(&s.elements, src, scope, depth, *cv.span())
        }
        ComponentValue::InterpolableIdent(InterpolableIdent::LessInterpolated(i)) => i
            .elements
            .iter()
            .map(|e| match e {
                LessInterpolatedIdentElement::Static(p) => p.value.to_string(),
                LessInterpolatedIdentElement::Variable(v) => {
                    interp_var(v.name.name, src, scope, depth, Some(*cv.span()))
                }
                LessInterpolatedIdentElement::Property(p) => {
                    prop_text(p.name.name, src, scope, depth)
                        .unwrap_or_else(|| format!("${{{}}}", p.name.name))
                }
            })
            .collect(),
        ComponentValue::Function(f) => eval_function(f, src, scope, depth),
        ComponentValue::LessList(list) => {
            // 纯 token 序列（less 的匿名值，如 `(min-width: 768px) and (...)`）按源文本
            // 原样输出：逐 token 拼接会插入多余空格，也可能破坏 `1 ,2` 这类写法
            if list
                .elements
                .iter()
                .all(|e| matches!(e, ComponentValue::TokenWithSpan(_)))
            {
                return substitute(text(src, cv), src, scope, depth, Some(*cv.span()));
            }
            // 有无逗号分隔符决定拼接方式（space list 用空格）；`/` 这类定界符两侧不留空格
            // （less 把它解析成 Operation，genCSS 只在 isSpaced 时加空格）
            let sep = if list.comma_spans.is_some() {
                ", "
            } else {
                " "
            };
            let mut out = String::new();
            for (i, e) in list.elements.iter().enumerate() {
                let delim = matches!(e, ComponentValue::Delimiter(_));
                if i > 0 && !delim && !matches!(list.elements[i - 1], ComponentValue::Delimiter(_))
                {
                    out.push_str(sep);
                }
                out.push_str(&eval(e, src, scope, depth + 1));
            }
            out
        }
        ComponentValue::LessNegativeValue(n) => {
            let inner = eval(&n.value, src, scope, depth + 1);
            format!("-{inner}")
        }
        ComponentValue::Url(u) => eval_url(u, src, scope, depth, Some(*cv.span())),
        ComponentValue::Ratio(_) => text(src, cv).to_string(),
        ComponentValue::Delimiter(_) => text(src, cv).to_string(),
        _ => text(src, cv).to_string(),
    }
}

/// 变量取值（未定义时保留 `@name` 并记录错误）。
fn var_value<'a>(
    name: &str,
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
    span: Option<oxc_css_parser::pos::Span>,
) -> String {
    let text = match scope.var(name) {
        Some(VarRef::Ast(node)) => eval(node, src, scope, depth + 1),
        Some(VarRef::List(v)) => v.join(" "),
        Some(VarRef::Text(t)) => t,
        Some(VarRef::Color(c)) => c.to_css(),
        Some(VarRef::Ruleset(_)) => String::new(),
        None => undefined_var(name, scope, span),
    };
    match scope.var_suffix(name) {
        Some(suffix) => format!("{text} {suffix}"),
        None => text,
    }
}

/// 变量作为名字使用时的文本：字符串取内容（去引号），其余按值求值。
pub(crate) fn var_name<'a>(
    name: &str,
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
    span: Option<oxc_css_parser::pos::Span>,
) -> String {
    match scope.var(name) {
        Some(VarRef::Ast(node)) => text_value(node, src, scope, depth + 1),
        Some(VarRef::List(v)) => v.join(" "),
        Some(VarRef::Text(t)) => unquote(&t),
        Some(VarRef::Color(c)) => c.to_css(),
        Some(VarRef::Ruleset(_)) => String::new(),
        None => {
            if let Some(span) = span {
                scope.var_miss(name, span);
            }
            name.to_string()
        }
    }
}

/// 插值用文本：quoted 变量取引号内的内容，其余取求值结果的 CSS 文本。
fn text_value<'a>(
    node: &'a ComponentValue<'a>,
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
) -> String {
    match node {
        ComponentValue::InterpolableStr(InterpolableStr::Literal(s)) => s.value.to_string(),
        _ => unquote(&eval(node, src, scope, depth)),
    }
}

/// 插值里的变量替换（未定义时原样保留）。
fn interp_var<'a>(
    name: &str,
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
    span: Option<oxc_css_parser::pos::Span>,
) -> String {
    // 变量名本身可以带插值（`@{box-@{suffix}}`）
    let resolved = if name.contains("@{") {
        interpolate(name, src, scope, depth, span)
    } else {
        name.to_string()
    };
    if !plain_var_name(&resolved) {
        return format!("@{{{name}}}");
    }
    match scope.var(&resolved) {
        Some(VarRef::Ast(node)) => text_value(node, src, scope, depth + 1),
        Some(VarRef::List(v)) => v.join(" "),
        Some(VarRef::Text(t)) => unquote(&t),
        Some(VarRef::Color(c)) => c.to_css(),
        Some(VarRef::Ruleset(_)) => String::new(),
        None => {
            if let Some(span) = span {
                scope.var_miss(&resolved, span);
            }
            format!("@{{{name}}}")
        }
    }
}

/// `@{}` 内是普通变量名（less 的名字插值正则只认 `[\w-]`）。
/// `@{paths[dir]}` 这类在 less 4.1.3 里当原文输出（permissive 上下文不求值），不报未定义。
fn plain_var_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// 选择器/属性名里的插值：值原样嵌入（quoted 值的引号保留，同 less）。
fn interp_var_raw<'a>(
    name: &str,
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
    span: Option<oxc_css_parser::pos::Span>,
) -> String {
    let resolved = if name.contains("@{") {
        interpolate(name, src, scope, depth, span)
    } else {
        name.to_string()
    };
    if !plain_var_name(&resolved) {
        return format!("@{{{name}}}");
    }
    let text = match scope.var(&resolved) {
        // 名字/选择器插值发生在 less 的 eval 阶段：数值此处不按 numPrecision 截断
        Some(VarRef::Ast(node)) => precise(|| eval(node, src, scope, depth + 1)),
        Some(VarRef::List(v)) => v.join(" "),
        Some(VarRef::Text(t)) => t,
        Some(VarRef::Color(c)) => c.to_css(),
        Some(VarRef::Ruleset(_)) => String::new(),
        None => {
            if let Some(span) = span {
                scope.var_miss(&resolved, span);
            }
            format!("@{{{name}}}")
        }
    };
    match scope.var_suffix(&resolved) {
        Some(suffix) => format!("{text} {suffix}"),
        None => text,
    }
}

/// `"a@{n}b"` 这类插值字符串：静态片段保留原文（含引号），变量/属性处替换。
fn interpolated_str<'a>(
    elements: &[LessInterpolatedStrElement<'a>],
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
    span: oxc_css_parser::pos::Span,
) -> String {
    // 元素替换后还要再插值一遍：`"@{box-@{suffix}}"` 会先拼成 `@{box-large}`
    let joined: String = elements
        .iter()
        .map(|e| match e {
            LessInterpolatedStrElement::Static(p) => p.raw.to_string(),
            LessInterpolatedStrElement::Variable(v) => {
                interp_var(v.name.name, src, scope, depth, Some(span))
            }
            LessInterpolatedStrElement::Property(p) => prop_text(p.name.name, src, scope, depth)
                .unwrap_or_else(|| format!("${{{}}}", p.name.name)),
        })
        .collect();
    interpolate(&joined, src, scope, depth, Some(span))
}

/// `@{name}` / `${prop}` 文本插值（选择器、属性名也用这一套）。
///
/// less 的 `Quoted.eval` 会反复替换直到不再变化，这里同样循环，但限次防死循环。
pub(crate) fn interpolate<'a>(
    text: &str,
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
    span: Option<oxc_css_parser::pos::Span>,
) -> String {
    if !text.contains("@{") && !text.contains("${") {
        return text.to_string();
    }
    let mut out = text.to_string();
    for _ in 0..8 {
        let next = interpolate_once(&out, src, scope, depth, span);
        if next == out {
            break;
        }
        out = next;
    }
    out
}

fn interpolate_once<'a>(
    text: &str,
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
    span: Option<oxc_css_parser::pos::Span>,
) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some((pos, is_prop)) = next_marker(rest) {
        out.push_str(&rest[..pos]);
        let after = &rest[pos + 2..];
        // 名字里可以再嵌套 `@{}`（`@{box-@{suffix}}`）：从内向外配对括号
        let Some(end) = matching_brace(after) else {
            out.push_str(&rest[pos..]);
            return out;
        };
        let name = &after[..end];
        // 引号状态要看整串前缀：插入的值会改变 rest 的起点（`"a@{x}@{y}"`）
        let abs = text.len() - rest.len() + pos;
        let quoted = inside_quotes(&text[..abs]);
        out.push_str(&if is_prop {
            prop_text(name, src, scope, depth).unwrap_or_else(|| format!("${{{name}}}"))
        } else if quoted {
            interp_var(name, src, scope, depth, span)
        } else {
            interp_var_raw(name, src, scope, depth, span)
        });
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

/// `@{` 之后到匹配 `}` 的下标（考虑嵌套的 `@{`）。
fn matching_brace(text: &str) -> Option<usize> {
    let mut depth = 0usize;
    for (i, ch) in text.char_indices() {
        match ch {
            '{' => depth += 1,
            '}' if depth == 0 => return Some(i),
            '}' => depth -= 1,
            _ => {}
        }
    }
    None
}

/// 位置 `pos` 是否落在引号字符串内部（跳过转义）。
fn inside_quotes(text: &str) -> bool {
    let mut quote = None;
    let mut escaped = false;
    for ch in text.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' => escaped = true,
            '"' | '\'' => match quote {
                Some(q) if q == ch => quote = None,
                Some(_) => {}
                None => quote = Some(ch),
            },
            _ => {}
        }
    }
    quote.is_some()
}

/// 返回最早的插值标记位置与它是不是 `${prop}`。
fn next_marker(text: &str) -> Option<(usize, bool)> {
    match (text.find("@{"), text.find("${")) {
        (Some(a), Some(b)) if a <= b => Some((a, false)),
        (Some(_), Some(b)) => Some((b, true)),
        (Some(a), None) => Some((a, false)),
        (None, Some(b)) => Some((b, true)),
        (None, None) => None,
    }
}

/// 前一个字符是标识符的一部分时，`@` 不是变量引用（`url(user@example.com)`）。
fn after_ident(prefix: &str) -> bool {
    prefix
        .chars()
        .next_back()
        .is_some_and(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.'))
}

/// `@x` 是否处在 `[...]` 查找名里：less 把它当命名空间查找的名字，不是变量引用。
fn inside_lookup(prefix: &str) -> bool {
    let (mut depth, mut quote) = (0i32, None);
    for c in prefix.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None => match c {
                '\'' | '"' => quote = Some(c),
                '[' => depth += 1,
                ']' => depth -= 1,
                _ => {}
            },
        }
    }
    depth > 0
}

/// `@name` / `@{name}` 文本替换：at-rule prelude、`url()` 内部用。
///
/// 与选择器插值不同，`@name` 保留值的引号（`url(@p)` → `url("a.png")`）。
/// 引号内与标识符中间的 `@` 不是变量引用（less 把它们当普通文本），不报错。
pub(crate) fn substitute<'a>(
    text: &str,
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
    span: Option<oxc_css_parser::pos::Span>,
) -> String {
    if !text.contains('@') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(pos) = rest.find('@') {
        out.push_str(&rest[..pos]);
        let abs = text.len() - rest.len() + pos;
        let prefix = &text[..abs];
        let after = &rest[pos + 1..];
        if let Some(name) = after.strip_prefix('{') {
            match name.find('}') {
                Some(end) => {
                    out.push_str(&interp_var(&name[..end], src, scope, depth, span));
                    rest = &name[end + 1..];
                }
                None => {
                    out.push_str(&rest[pos..]);
                    return out;
                }
            }
            continue;
        }
        let len = after
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
            .unwrap_or(after.len());
        if len == 0 {
            out.push('@');
            rest = after;
            continue;
        }
        let report = if inside_quotes(prefix) || after_ident(prefix) || inside_lookup(prefix) {
            None
        } else {
            span
        };
        out.push_str(&var_value(&after[..len], src, scope, depth, report));
        rest = &after[len..];
    }
    out.push_str(rest);
    out
}

/// `url(...)`：内部文本按 less 求值（插值/变量），`data:` 等裸值原样保留。
fn eval_url<'a>(
    u: &Url<'a>,
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
    span: Option<oxc_css_parser::pos::Span>,
) -> String {
    let raw = text_of(u.span, src);
    let (Some(open), Some(close)) = (raw.find('('), raw.rfind(')')) else {
        return raw.to_string();
    };
    let inner = &raw[open + 1..close];
    if !inner.contains('@') || inner.trim_start().starts_with("data:") {
        return raw.to_string();
    }
    format!(
        "{}({})",
        &raw[..open],
        substitute(inner, src, scope, depth, span)
    )
}

/// 属性值文本：命中则返回（合并过的）值，未定义返回 None。
fn prop_text<'a>(name: &str, src: &'a str, scope: &dyn Scope<'a>, depth: usize) -> Option<String> {
    let last = scope.prop(name)?.pop()?;
    Some(match last {
        PropSource::Decl(d) => with_important(eval_values(&d.value, src, scope, depth + 1), d),
        PropSource::Merged(groups) => groups
            .iter()
            .map(|g| {
                g.iter()
                    .map(|d| with_important(eval_values(&d.value, src, scope, depth + 1), d))
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect::<Vec<_>>()
            .join(", "),
    })
}

/// less 的 `$prop` 会把源声明的 `!important` 带到引用处。
fn with_important(text: String, d: &Declaration) -> String {
    if d.important.is_some() {
        format!("{text} !important")
    } else {
        text
    }
}

/// 声明的整条值：段间空白归一为单个空格（less `Expression` 的拼接方式）。
pub(crate) fn eval_values<'a>(
    values: &[ComponentValue<'a>],
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
) -> String {
    join_seq(values, src, scope, depth)
}

/// 值/参数序列拼接：段的空白归一为单个空格，逗号归一为 `, `。
fn join_seq<'a>(
    items: &[ComponentValue<'a>],
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
) -> String {
    let mut out = String::new();
    let mut prev_end: Option<usize> = None;
    let mut skip_gap = false;
    for v in items {
        let span = *v.span();
        let comma =
            matches!(v, ComponentValue::Delimiter(d) if matches!(d.kind, DelimiterKind::Comma));
        if let Some(end) = prev_end
            && end <= span.start
            && span.start <= src.len()
        {
            let gap = &src[end..span.start];
            if gap.contains("/*") {
                // 参里夹着的块注释是值节点（less `arguments()` 把 comment 当实体）
                if !skip_gap {
                    out.push(' ');
                }
                out.push_str(gap.trim());
                if !comma {
                    out.push(' ');
                }
            } else if !skip_gap && !comma {
                out.push_str(if gap.trim().is_empty() && !gap.is_empty() {
                    " "
                } else {
                    gap
                });
            }
        }
        if comma {
            out.push_str(", ");
        } else {
            out.push_str(&eval(v, src, scope, depth));
        }
        skip_gap = comma;
        prev_end = Some(span.end);
    }
    out
}

/// 括号内求值：除法在这里才计算（less 的 `math: parens-division`）。
/// less 在括号内全量开 math，因此嵌套运算里的除法也要算。
fn eval_paren<'a>(
    cv: &ComponentValue<'a>,
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
) -> String {
    if depth > 32 {
        return eval(cv, src, scope, depth);
    }
    match cv {
        ComponentValue::LessBinaryOperation(op) => {
            // 数值域直接算 f64：文本往返（8 位小数）会丢精度
            if let Some((v, u)) = number_in(cv, src, scope, depth + 1, true) {
                return fmt_with_unit(v, u);
            }
            let left = eval_paren(&op.left, src, scope, depth + 1);
            let right = eval_paren(&op.right, src, scope, depth + 1);
            operate(&op.op.kind, &left, &right).unwrap_or_else(|| format!("{left} {right}"))
        }
        ComponentValue::LessParenthesizedOperation(p) => {
            eval_paren(&p.operation, src, scope, depth + 1)
        }
        // 括号内的变量引用按「math 开」求值：`(@b)` 里 `@b: 18 / 2` 要算成 9
        ComponentValue::LessVariable(v) => {
            paren_var(v.name.name, src, scope, depth, Some(*cv.span()))
        }
        ComponentValue::LessVariableCall(c) => {
            paren_var(c.variable.name.name, src, scope, depth, Some(*cv.span()))
        }
        _ => eval(cv, src, scope, depth + 1),
    }
}

fn paren_var<'a>(
    name: &str,
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
    span: Option<oxc_css_parser::pos::Span>,
) -> String {
    match scope.var(name) {
        Some(VarRef::Ast(node)) => eval_paren(node, src, scope, depth + 1),
        Some(VarRef::List(v)) => v.join(" "),
        Some(VarRef::Text(t)) => t,
        Some(VarRef::Color(c)) => c.to_css(),
        Some(VarRef::Ruleset(_)) => String::new(),
        None => undefined_var(name, scope, span),
    }
}

/// 二元运算。
fn eval_binary<'a>(
    op: &LessBinaryOperation<'a>,
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
) -> String {
    let left_text = eval(&op.left, src, scope, depth + 1);
    let right_text = eval(&op.right, src, scope, depth + 1);

    // 除法默认不计算（math=parens-division），原样输出，保留运算符周边空白
    if matches!(op.op.kind, LessOperationOperatorKind::Division) {
        let lo = op.left.span().end;
        let hi = op.right.span().start;
        let sep = if lo <= hi && hi <= src.len() {
            &src[lo..hi]
        } else {
            "/"
        };
        return format!("{left_text}{sep}{right_text}");
    }

    let (lo, hi) = (op.left.span().end, op.right.span().start);
    let gap = if lo <= hi && hi <= src.len() {
        &src[lo..hi]
    } else {
        ""
    };
    let op_text = match op.op.kind {
        LessOperationOperatorKind::Plus => "+",
        LessOperationOperatorKind::Minus => "-",
        LessOperationOperatorKind::Multiply => "*",
        LessOperationOperatorKind::Division | LessOperationOperatorKind::DotDivision => "/",
    };
    // less 的 `Operation.eval`：操作数不可运算（例如未求值的除法）时整棵运算原样保留
    let spaced = !gap.is_empty() && gap.starts_with(|c: char| c.is_whitespace());
    let fallback = if spaced {
        format!("{left_text} {op_text} {right_text}")
    } else {
        format!("{left_text}{op_text}{right_text}")
    };
    operate(&op.op.kind, &left_text, &right_text).unwrap_or(fallback)
}

/// 操作数：带单位的数，或颜色（less 里 Dimension 参与颜色运算时会转成灰度色）。
#[derive(Clone)]
enum Operand {
    Num(f64, Option<String>),
    Color(Color),
}

fn parse_operand(text: &str) -> Option<Operand> {
    if let Some(c) = parse_color(text) {
        return Some(Operand::Color(c));
    }
    // 只接受「数值 + 单位」整体：`10px / 2` 的单位部分不是单位，不能当数值算
    parse_number(text)
        .filter(|(_, u)| u.as_deref().is_none_or(is_unit_name))
        .map(|(v, u)| Operand::Num(v, u))
}

fn is_unit_name(unit: &str) -> bool {
    unit.chars()
        .all(|c| c.is_alphabetic() || c == '%' || c == '_')
}

/// 同组单位换算（less `Dimension.convertTo`）；不可换算时原样返回。
fn convert_unit(value: f64, from: &str, to: &str) -> f64 {
    for (_, table) in UNIT_GROUPS {
        let f = table.iter().find(|(u, _)| *u == from).map(|(_, f)| *f);
        let t = table.iter().find(|(u, _)| *u == to).map(|(_, f)| *f);
        if let (Some(f), Some(t)) = (f, t) {
            return value * f / t;
        }
    }
    value
}

/// 两侧已是求值文本时的四则运算（命名空间查找结果参与运算用）。
pub(crate) fn arithmetic(
    kind: &LessOperationOperatorKind,
    left: &str,
    right: &str,
) -> Option<String> {
    operate(kind, left, right)
}

fn operate(kind: &LessOperationOperatorKind, left: &str, right: &str) -> Option<String> {
    match (parse_operand(left)?, parse_operand(right)?) {
        (Operand::Num(lv, lu), Operand::Num(rv, ru)) => {
            let (v, unit) = match *kind {
                // less 的 +/-：结果单位取左；右单位可换算时先换算
                LessOperationOperatorKind::Plus | LessOperationOperatorKind::Minus => {
                    let rv = match (lu.as_deref(), ru.as_deref()) {
                        (Some(l), Some(r)) => convert_unit(rv, r, l),
                        _ => rv,
                    };
                    (arith(kind, lv, rv), lu.or(ru))
                }
                _ => (arith(kind, lv, rv), lu.or(ru)),
            };
            Some(fmt_with_unit(v, unit))
        }
        (l, r) => {
            let (l, r) = (to_color(l), to_color(r));
            // less Color.operate：alpha 走合成公式，与运算符无关
            let alpha = l.a * (1.0 - r.a) + r.a;
            Some(
                Color {
                    r: arith(kind, l.r, r.r),
                    g: arith(kind, l.g, r.g),
                    b: arith(kind, l.b, r.b),
                    a: alpha,
                }
                .to_css(),
            )
        }
    }
}

fn arith(kind: &LessOperationOperatorKind, a: f64, b: f64) -> f64 {
    match kind {
        LessOperationOperatorKind::Plus => a + b,
        LessOperationOperatorKind::Minus => a - b,
        LessOperationOperatorKind::Multiply => a * b,
        LessOperationOperatorKind::Division | LessOperationOperatorKind::DotDivision => a / b,
    }
}

fn to_color(op: Operand) -> Color {
    match op {
        Operand::Color(c) => c,
        // less Dimension.toColor：数值当灰度
        Operand::Num(v, _) => Color {
            r: v,
            g: v,
            b: v,
            a: 1.0,
        },
    }
}

/// less `unit-conversions.js`：同组单位可换算。
const UNIT_GROUPS: &[(&str, &[(&str, f64)])] = &[
    (
        "length",
        &[
            ("m", 1.0),
            ("cm", 0.01),
            ("mm", 0.001),
            ("in", 0.0254),
            ("px", 0.0254 / 96.0),
            ("pt", 0.0254 / 72.0),
            ("pc", 0.0254 / 72.0 * 12.0),
        ],
    ),
    ("duration", &[("s", 1.0), ("ms", 0.001)]),
    (
        "angle",
        &[
            ("rad", 1.0 / (2.0 * std::f64::consts::PI)),
            ("deg", 1.0 / 360.0),
            ("grad", 1.0 / 400.0),
            ("turn", 1.0),
        ],
    ),
];

pub(super) fn eval_function_text<'a>(
    f: &Function<'a>,
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
) -> String {
    eval_function(f, src, scope, depth)
}

/// 单位换算组：返回（组序号，到组基本单位的系数）。
pub(super) fn unit_factor(unit: &str) -> Option<(usize, f64)> {
    (0..UNIT_GROUPS.len()).find_map(|g| {
        UNIT_GROUPS[g]
            .1
            .iter()
            .find(|(u, _)| *u == unit)
            .map(|(_, f)| (g, *f))
    })
}

/// 函数名与实参；`if`/`boolean` 的实参里有条件节点，必须单独求值（只算选中分支）。
/// IE 滤镜函数的 `(路径, 括号内)`: `progid:DXImageTransform.Microsoft.f(x=1)` →
/// `("progid:DXImageTransform.Microsoft.f", "x=1")`。
fn progid_parts<'a>(f: &Function<'a>, src: &'a str) -> Option<(&'a str, &'a str)> {
    let raw = src.get(f.span.start..f.span.end)?;
    let head = raw.get(..raw.find('(')?)?;
    let tail = raw.get(head.len() + 1..)?;
    if !head.to_ascii_lowercase().starts_with("progid:") {
        return None;
    }
    let inner = tail.strip_suffix(')')?;
    Some((head, inner))
}

/// `key = value` → `key=value`（引号内不动）：less 的 `Assignment` 输出形态。
fn tighten_assignments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut quote = None;
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match quote {
            Some(q) => {
                out.push(ch);
                if ch == q {
                    quote = None;
                }
            }
            None => match ch {
                '"' | '\'' => {
                    quote = Some(ch);
                    out.push(ch);
                }
                '=' => {
                    while out.ends_with(' ') {
                        out.pop();
                    }
                    out.push('=');
                    while chars.peek() == Some(&' ') {
                        chars.next();
                    }
                }
                _ => out.push(ch),
            },
        }
    }
    out
}

/// 语句级函数调用（根层裸调用 `e('…');`）的求值入口。
pub(crate) fn eval_function_stmt<'a>(
    f: &'a Function<'a>,
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
) -> String {
    eval_function(f, src, scope, depth)
}

fn eval_function<'a>(
    f: &Function<'a>,
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
) -> String {
    let name = function_name(f);
    // IE 滤镜 `progid:DXImageTransform.Microsoft.f(opacity = 20)`：less 把整段路径当函数名，
    // 参数是 `key=value` 赋值（`Assignment` 输出 `key=value`，不留空格）。
    if let Some((prefix, inner)) = progid_parts(f, src) {
        let inner = substitute(inner, src, scope, depth, Some(*f.span()));
        return format!("{prefix}({})", tighten_assignments(&inner));
    }
    // 探测类函数直接看节点，不求值实参：`isdefined(@nope)` / `isruleset(@nope)` 不该报错。
    match name.as_str() {
        "isdefined" => {
            return is_css(f.args.first().is_some_and(|a| probe_defined(a, scope)));
        }
        "isruleset" => {
            return is_css(f.args.first().is_some_and(|a| probe_ruleset(a, scope)));
        }
        _ => {}
    }
    if matches!(name.as_str(), "if" | "boolean") {
        if let Some(out) = cond_function(&name, f, src, scope, depth) {
            return out;
        }
    }
    // 逗号在 AST 里是独立 Delimiter 节点，不是参数：按名取参时必须过滤，
    // 否则 `lighten(#333, 10%)` 的 args[1] 会是逗号。
    let args: Vec<String> = f
        .args
        .iter()
        .filter(|a| !matches!(a, ComponentValue::Delimiter(_)))
        .map(|a| eval(a, src, scope, depth + 1))
        .collect();
    let joined = join_args(&f.args, src, scope, depth);

    match name.as_str() {
        "lighten" | "darken" | "saturate" | "desaturate" | "spin" | "greyscale" | "fade"
        | "fadein" | "fadeout" | "mix" | "tint" | "shade" => color_function(f, src, scope, depth)
            .map(|c| c.to_css())
            .unwrap_or_else(|| format!("{name}({joined})")),
        // `rgba(color)` / `rgba(color, alpha)`：参数是颜色时只覆盖 alpha（less `functions/color.js`）
        "rgba" | "rgb" | "hsl" | "hsla" => {
            let single = f
                .args
                .first()
                .and_then(|a| color_arg(a, src, scope, depth))
                .filter(|_| args.len() <= 2);
            match (single, name.as_str()) {
                (Some(c), "rgba" | "rgb") => {
                    let a = args.get(1).and_then(|a| color_number(a)).unwrap_or(c.a);
                    Color { a, ..c }.to_css()
                }
                (Some(c), _) => {
                    let (h, s, l) = c.to_hsl();
                    let a = args.get(1).and_then(|a| color_number(a)).unwrap_or(c.a);
                    hsl_css(h, s, l, a)
                }
                _ => {
                    let text = match name.as_str() {
                        "hsl" | "hsla" => hsl_text(&args),
                        _ => parse_color(&format!("{name}({joined})")).map(|c| c.to_css()),
                    };
                    text.unwrap_or_else(|| format!("{name}({joined})"))
                }
            }
        }
        // 颜色通道/属性（less `functions/color.js`）
        "red" | "green" | "blue" => {
            let c = parse_color(arg_at(&args, 0));
            let v = c.map(|c| match name.as_str() {
                "red" => c.r,
                "green" => c.g,
                _ => c.b,
            });
            v.map(fmt_number)
                .unwrap_or_else(|| format!("{name}({joined})"))
        }
        "alpha" => parse_color(arg_at(&args, 0))
            .map(|c| fmt_number(c.a))
            .unwrap_or_else(|| format!("{name}({joined})")),
        "hue" | "saturation" | "lightness" => parse_color(arg_at(&args, 0))
            .map(|c| {
                let (h, s, l) = c.to_hsl();
                match name.as_str() {
                    "hue" => fmt_number(h),
                    "saturation" => format!("{}%", fmt_number(s * 100.0)),
                    _ => format!("{}%", fmt_number(l * 100.0)),
                }
            })
            .unwrap_or_else(|| format!("{name}({joined})")),
        "argb" => parse_color(arg_at(&args, 0))
            .map(|c| c.to_argb())
            .unwrap_or_else(|| format!("{name}({joined})")),
        "percentage" => number_arg(&f.args, src, scope, depth)
            .map(|(v, u)| {
                let v = if u.as_deref() == Some("%") {
                    v / 100.0
                } else {
                    v
                };
                format!("{}%", fmt_number(v * 100.0))
            })
            .unwrap_or_else(|| format!("{name}({joined})")),
        "unit" => {
            let v = args.first().cloned().unwrap_or_default();
            let unit = args.get(1).cloned().unwrap_or_default();
            let unit = unit.trim_matches('"').trim_matches('\'');
            match parse_number(&v) {
                Some((n, _)) => format!("{}{unit}", fmt_number(n)),
                None => v,
            }
        }
        "round" | "ceil" | "floor" | "abs" => {
            let v = number_arg(&f.args, src, scope, depth);
            // `round(n, precision)`：less 走 `toFixed`，小数位后转回数值
            let digits =
                number_arg_at(&f.args, 1, src, scope, depth).map(|(d, _)| d.max(0.0) as usize);
            match (v, name.as_str()) {
                (Some((n, u)), "round") => fmt_with_unit(fixed(n, digits.unwrap_or(0)), u),
                (Some((n, u)), "ceil") => fmt_with_unit(n.ceil(), u),
                (Some((n, u)), "floor") => fmt_with_unit(n.floor(), u),
                (Some((n, u)), "abs") => fmt_with_unit(n.abs(), u),
                _ => format!("{name}({joined})"),
            }
        }
        // ---- 列表（less `functions/list.js`）----
        "length" => fmt_number(list_items(f, src, scope, depth).len() as f64),
        "extract" => {
            let items = list_items(f, src, scope, depth);
            match extract_index(f, src, scope, depth) {
                Some(i) if i < items.len() => items[i].clone(),
                _ => format!("{name}({joined})"),
            }
        }
        "range" => range_list(f, src, scope, depth).unwrap_or_else(|| format!("{name}({joined})")),
        // ---- 数学（less `functions/math.js`、`functions/number.js`）----
        "pi" => fmt_number(std::f64::consts::PI),
        "sin" | "cos" | "tan" | "asin" | "acos" | "atan" => {
            math_fn(&name, f, src, scope, depth).unwrap_or_else(|| format!("{name}({joined})"))
        }
        "pow" | "sqrt" | "mod" => {
            number_fn(&name, f, src, scope, depth).unwrap_or_else(|| format!("{name}({joined})"))
        }
        "min" | "max" => min_max(f, name == "min", src, scope, depth)
            .unwrap_or_else(|| format!("{name}({joined})")),
        "convert" => {
            convert_fn(f, src, scope, depth).unwrap_or_else(|| format!("{name}({joined})"))
        }
        "get-unit" => match number_arg(&f.args, src, scope, depth) {
            Some((_, u)) => u.unwrap_or_default(),
            None => format!("{name}({joined})"),
        },
        // ---- 颜色通道与派生（less `functions/color.js`）----
        "luma" | "luminance" => parse_color(arg_at(&args, 0))
            .map(|c| {
                let v = match name.as_str() {
                    "luma" => c.luma(),
                    _ => c.luminance(),
                };
                format!("{}%", fmt_number(v * c.a * 100.0))
            })
            .unwrap_or_else(|| format!("{name}({joined})")),
        "hsvhue" | "hsvsaturation" | "hsvvalue" => parse_color(arg_at(&args, 0))
            .map(|c| {
                let (h, s, v) = c.to_hsv();
                match name.as_str() {
                    "hsvhue" => fmt_number(h),
                    "hsvsaturation" => format!("{}%", fmt_number(s * 100.0)),
                    _ => format!("{}%", fmt_number(v * 100.0)),
                }
            })
            .unwrap_or_else(|| format!("{name}({joined})")),
        "hsv" | "hsva" => {
            let number = |i: usize| args.get(i).and_then(|a| color_number(a));
            match (number(0), number(1), number(2)) {
                (Some(h), Some(s), Some(v)) => {
                    let a = number(3).unwrap_or(1.0);
                    hsv_color(h, s, v, a).to_css()
                }
                _ => format!("{name}({joined})"),
            }
        }
        "contrast" => parse_color(arg_at(&args, 0))
            .map(|c| contrast_color(c, &args))
            .unwrap_or_else(|| format!("{name}({joined})")),
        // `color('#55FF5599')`：引号内的十六进制原样交回（保留大小写）；
        // 颜色关键字则回落到 `#rrggbb`（less 把 Color 的 `value` 清空）
        "color" => {
            let a = arg_at(&args, 0);
            let inner = unquote(a);
            let hex = inner.strip_prefix('#').filter(|h| {
                matches!(h.len(), 3 | 4 | 6 | 8) && h.chars().all(|c| c.is_ascii_hexdigit())
            });
            match hex {
                Some(_) => inner,
                None => parse_color(a)
                    .map(|c| c.to_css())
                    .unwrap_or_else(|| format!("{name}({joined})")),
            }
        }
        // 颜色混合（less `functions/color-blending.js`）
        "multiply" | "screen" | "overlay" | "softlight" | "hardlight" | "difference"
        | "exclusion" | "average" | "negation" => {
            let (Some(a), Some(b)) = (parse_color(arg_at(&args, 0)), parse_color(arg_at(&args, 1)))
            else {
                return format!("{name}({joined})");
            };
            blend(&name, a, b).to_css()
        }
        "replace" => {
            replace_fn(&f.args, src, scope, depth).unwrap_or_else(|| format!("{name}({joined})"))
        }
        // `calc(...)`：内部不再做四则运算，只写回变量与函数结果
        "calc" => calc_text(f, src, scope, depth),
        // 类型判断（less `functions/types.js`）
        "isnumber" => is_css(parse_number(arg_at(&args, 0)).is_some()),
        "iscolor" => is_css(is_color_text(arg_at(&args, 0))),
        "isstring" => is_css({
            let a = arg_at(&args, 0);
            a.len() >= 2 && (a.starts_with('"') || a.starts_with('\''))
        }),
        "iskeyword" => is_css({
            let a = arg_at(&args, 0);
            !a.is_empty()
                && parse_number(a).is_none()
                && !is_color_text(a)
                && !a.starts_with(['"', '\''])
        }),
        "isurl" => is_css(arg_at(&args, 0).starts_with("url(")),
        "ispixel" => is_css(unit_of(arg_at(&args, 0)) == Some("px".to_string())),
        "ispercentage" => is_css(unit_of(arg_at(&args, 0)).as_deref() == Some("%")),
        "isem" => is_css(unit_of(arg_at(&args, 0)).as_deref() == Some("em")),
        "isunit" => is_css({
            let unit = arg_at(&args, 1);
            let unit = unit.trim_matches(['"', '\'']);
            unit_of(arg_at(&args, 0)).as_deref() == Some(unit) && !unit.is_empty()
        }),
        // `e("...")`：去引号的匿名值（less `Anonymous`）
        "e" => args
            .first()
            .map(|a| a.trim_matches(['"', '\'']).to_string())
            .unwrap_or_default(),
        // `%("format", args...)`：less `format`（`%s %d %a`）
        "%" => format_string(&args).unwrap_or_else(|| format!("{name}({joined})")),
        // `escape(str)`：less `encodeURIComponent` + 额外转义 `= : # ; ( )`
        "escape" => args
            .first()
            .map(|a| escape_url(&unquote(a)))
            .unwrap_or_else(|| format!("{name}({joined})")),
        // 文件类函数：`data-uri` / `image-size` / `image-width` / `image-height`
        "data-uri" => {
            let args = arg_list(f, src, scope, depth);
            scope
                .file_roots()
                .and_then(|(dir, paths)| super::file_fn::data_uri(dir, paths, &args))
                .unwrap_or_else(|| format!("{name}({joined})"))
        }
        "image-size" => image_size(f, src, scope, depth)
            .map(|s| format!("{}px {}px", s.width, s.height))
            .unwrap_or_else(|| format!("{name}({joined})")),
        "image-width" => image_size(f, src, scope, depth)
            .map(|s| format!("{}px", s.width))
            .unwrap_or_else(|| format!("{name}({joined})")),
        "image-height" => image_size(f, src, scope, depth)
            .map(|s| format!("{}px", s.height))
            .unwrap_or_else(|| format!("{name}({joined})")),
        "svg-gradient" => super::file_fn::svg_gradient(&arg_list(f, src, scope, depth))
            .unwrap_or_else(|| format!("{name}({joined})")),
        // 未知函数：保留原名与已求值参数
        _ => format!("{name}({joined})"),
    }
}

fn arg_at(args: &[String], i: usize) -> &str {
    args.get(i).map(String::as_str).unwrap_or("")
}

/// `image-size` 家族：缺文件/格式不认识时 less 抛错，这里保留原调用文本。
fn image_size<'a>(
    f: &Function<'a>,
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
) -> Option<super::file_fn::Size> {
    let (dir, paths) = scope.file_roots()?;
    super::file_fn::image_size(dir, paths, arg_list(f, src, scope, depth).first()?)
}

/// 实参按 less 的 `parser.arguments` 分组（逗号分隔，组内空格分隔），但每组求值后
/// 拼回一段文本：`svg-gradient(to bottom, black, white)` 的方向就是一组 `to bottom`。
fn arg_list<'a>(
    f: &Function<'a>,
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
) -> Vec<String> {
    fn_arg_groups(f)
        .iter()
        .map(|group| {
            group
                .iter()
                .map(|cv| eval(cv, src, scope, depth + 1))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect()
}

fn unit_of(text: &str) -> Option<String> {
    parse_number(text).and_then(|(_, u)| u)
}

// ---------------------------------------------------------------------------
// 列表函数（less `functions/list.js`）
// ---------------------------------------------------------------------------

/// 实参分组：less 的 `parser.arguments`：含分号时只按分号切（逗号留在组内），
/// 否则按逗号切；组内是空格分隔的值。
pub(super) fn arg_groups<'a, 'b>(
    args: &'b [ComponentValue<'a>],
) -> Vec<Vec<&'b ComponentValue<'a>>> {
    // less：`each(10px 15px, 20px 25px; { ... })` 的第二个实参是规则集，
    // 所以出现分号时逗号不再分隔实参
    let semi = args.iter().any(|a| {
        matches!(
            a,
            ComponentValue::Delimiter(d) if matches!(d.kind, DelimiterKind::Semicolon)
        )
    });
    let mut groups: Vec<Vec<&ComponentValue>> = vec![Vec::new()];
    for a in args {
        let split = matches!(
            a,
            ComponentValue::Delimiter(d)
                if if semi {
                    matches!(d.kind, DelimiterKind::Semicolon)
                } else {
                    matches!(d.kind, DelimiterKind::Comma)
                }
        );
        if split {
            groups.push(Vec::new());
        } else if let Some(last) = groups.last_mut() {
            last.push(a);
        }
    }
    groups.retain(|g| !g.is_empty());
    groups
}

fn fn_arg_groups<'a, 'b>(f: &'b Function<'a>) -> Vec<Vec<&'b ComponentValue<'a>>> {
    arg_groups(&f.args)
}

/// `length`/`extract` 的元素（less `getItemsFromNode`）：列表展开，其余算一个。
fn list_items<'a>(
    f: &Function<'a>,
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
) -> Vec<String> {
    let Some(group) = fn_arg_groups(f).into_iter().next() else {
        return Vec::new();
    };
    if group.len() > 1 {
        return group
            .iter()
            .map(|n| eval(n, src, scope, depth + 1))
            .collect();
    }
    match group.first() {
        Some(node) => node_items(node, src, scope, depth),
        None => Vec::new(),
    }
}

fn node_items<'a>(
    node: &ComponentValue<'a>,
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
) -> Vec<String> {
    if depth > 32 {
        return vec![eval(node, src, scope, depth)];
    }
    match node {
        ComponentValue::LessVariable(v) => var_items(v.name.name, src, scope, depth),
        ComponentValue::LessVariableCall(c) => var_items(c.variable.name.name, src, scope, depth),
        ComponentValue::LessList(l) => l
            .elements
            .iter()
            .map(|e| eval(e, src, scope, depth + 1))
            .collect(),
        _ => {
            let text = eval(node, src, scope, depth);
            if text.is_empty() {
                vec![text]
            } else {
                split_items(&text)
            }
        }
    }
}

fn var_items<'a>(name: &str, src: &'a str, scope: &dyn Scope<'a>, depth: usize) -> Vec<String> {
    if depth > 32 {
        return vec![format!("@{name}")];
    }
    match scope.var(name) {
        Some(VarRef::Ast(n)) => node_items(n, src, scope, depth + 1),
        Some(VarRef::List(v)) => v.clone(),
        Some(VarRef::Text(t)) => split_items(&t),
        Some(VarRef::Color(c)) => vec![c.to_css()],
        Some(VarRef::Ruleset(_)) => Vec::new(),
        None => vec![format!("@{name}")],
    }
}

/// 文本形态的列表（globalVars、变参绑定）：按顶层逗号分，再按顶层空白分。
pub(super) fn split_items(text: &str) -> Vec<String> {
    let t = text.trim();
    if t.is_empty() {
        return Vec::new();
    }
    let commas = crate::text::split_top_level(t, b',');
    if commas.len() > 1 {
        return commas
            .into_iter()
            .map(|(s, e)| t[s..e].trim().to_string())
            .filter(|p| !p.is_empty())
            .collect();
    }
    split_ws(t)
}

/// 顶层空白切分。
pub(super) fn split_ws(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut depth = 0usize;
    let mut quote = None;
    let mut escaped = false;
    for ch in text.chars() {
        if escaped {
            cur.push(ch);
            escaped = false;
            continue;
        }
        match ch {
            '\\' => {
                cur.push(ch);
                escaped = true;
            }
            '"' | '\'' if quote.is_none() => {
                quote = Some(ch);
                cur.push(ch);
            }
            c if Some(c) == quote => {
                quote = None;
                cur.push(ch);
            }
            '(' | '[' => {
                depth += 1;
                cur.push(ch);
            }
            ')' | ']' => {
                depth = depth.saturating_sub(1);
                cur.push(ch);
            }
            c if c.is_whitespace() && depth == 0 && quote.is_none() => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// `extract` 的下标（0 基）；非正整数下标 less 返回 undefined（调用原样输出）。
fn extract_index<'a>(
    f: &Function<'a>,
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
) -> Option<usize> {
    let groups = fn_arg_groups(f);
    let node = groups.get(1)?.first()?;
    let (v, _) = number_in(node, src, scope, depth + 1, false)?;
    (v >= 1.0 && v.fract() == 0.0 && v < 1e9).then(|| v as usize - 1)
}

/// `range(start, end, step)`：less 的 `functions/list.js`，输出空格列表。
fn range_list<'a>(
    f: &Function<'a>,
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
) -> Option<String> {
    let groups = fn_arg_groups(f);
    let num = |i: usize| -> Option<(f64, Option<String>)> {
        number_in(groups.get(i)?.first()?, src, scope, depth + 1, false)
    };
    let (from, to, step) = match groups.len() {
        1 => (1.0, num(0)?.0, 1.0),
        2 => (num(0)?.0, num(1)?.0, 1.0),
        _ => (num(0)?.0, num(1)?.0, num(2)?.0),
    };
    let unit = num(groups.len().saturating_sub(1))?.1;
    if step == 0.0 {
        return None;
    }
    let mut out: Vec<String> = Vec::new();
    let mut v = from;
    while (step > 0.0 && v <= to) || (step < 0.0 && v >= to) {
        out.push(fmt_with_unit(v, unit.clone()));
        if out.len() > 10_000 {
            return None;
        }
        v += step;
    }
    if out.is_empty() {
        return None;
    }
    Some(out.join(" "))
}

// ---------------------------------------------------------------------------
// 数学函数（less `functions/math.js`、`functions/number.js`）
// ---------------------------------------------------------------------------

/// `Dimension.unify()`：长度 → px、时长 → s、角度 → rad（不可换算的单位保留）。
fn unify(v: f64, unit: Option<String>) -> (f64, Option<String>) {
    let Some(u) = unit else {
        return (v, None);
    };
    let Some((group, from)) = unit_factor(&u) else {
        return (v, Some(u));
    };
    let target = ["px", "s", "rad"].get(group).copied().unwrap_or("px");
    match unit_factor(target) {
        Some((_, to)) => (v * from / to, Some(target.to_string())),
        None => (v, Some(u)),
    }
}

/// less 的 `round(n, d)` 走 `toFixed`：四舍五入、远离零。
fn fixed(v: f64, digits: usize) -> f64 {
    let scale = 10f64.powi(digits as i32);
    (v * scale).round() / scale
}

/// `sin`/`cos`/`tan`（结果无单位）与 `asin`/`acos`/`atan`（结果 rad）。
fn math_fn<'a>(
    name: &str,
    f: &Function<'a>,
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
) -> Option<String> {
    let (v, u) = number_arg(&f.args, src, scope, depth)?;
    let (v, _) = unify(v, u);
    let out = match name {
        "sin" => v.sin(),
        "cos" => v.cos(),
        "tan" => v.tan(),
        "asin" => v.asin(),
        "acos" => v.acos(),
        _ => v.atan(),
    };
    let unit = if name.starts_with('a') { "rad" } else { "" };
    Some(fmt_with_unit(out, Some(unit.to_string())))
}

/// `pow`/`sqrt`/`mod`（less `functions/number.js`）。
fn number_fn<'a>(
    name: &str,
    f: &Function<'a>,
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
) -> Option<String> {
    let (a, au) = number_arg(&f.args, src, scope, depth)?;
    let out = match name {
        "sqrt" => (a.sqrt(), au),
        _ => {
            let (b, _) = number_arg_at(&f.args, 1, src, scope, depth)?;
            match name {
                "pow" => (a.powf(b), au),
                _ => (a % b, au),
            }
        }
    };
    Some(fmt_with_unit(out.0, out.1))
}

/// 第 `i` 个实参（跳过分隔符）的数值。
fn number_arg_at<'a>(
    args: &[ComponentValue<'a>],
    i: usize,
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
) -> Option<(f64, Option<String>)> {
    let node = args
        .iter()
        .filter(|a| !matches!(a, ComponentValue::Delimiter(_)))
        .nth(i)?;
    number_in(node, src, scope, depth, false)
}

/// `min`/`max`：同单位（可换算）比较取极值，多单位不可换算时 less 原样输出。
fn min_max<'a>(
    f: &Function<'a>,
    is_min: bool,
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
) -> Option<String> {
    let mut cands: Vec<(f64, Option<String>)> = Vec::new();
    for group in fn_arg_groups(f) {
        for node in group {
            cands.push(number_in(node, src, scope, depth + 1, false)?);
        }
    }
    if cands.is_empty() {
        return None;
    }
    // less 把无单位的实参当作「首个带单位的实参」的单位
    let unit_static = cands.iter().find_map(|c| c.1.clone());
    let unified: Vec<(f64, Option<String>)> = cands
        .iter()
        .map(|c| unify(c.0, c.1.clone().or_else(|| unit_static.clone())))
        .collect();
    let units: Vec<&String> = unified.iter().filter_map(|u| u.1.as_ref()).collect();
    if units.windows(2).any(|w| w[0] != w[1]) {
        return None;
    }
    let mut best = 0;
    for i in 1..unified.len() {
        let better = if is_min {
            unified[i].0 < unified[best].0
        } else {
            unified[i].0 > unified[best].0
        };
        if better {
            best = i;
        }
    }
    // 输出原始实参（less 取 `order[j].toCSS()`，保留它自己的单位）
    let (v, u) = &cands[best];
    Some(fmt_with_unit(*v, u.clone()))
}

/// `convert(value, unit)`：同组单位换算（less `Dimension.convertTo`）。
fn convert_fn<'a>(
    f: &Function<'a>,
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
) -> Option<String> {
    let (v, u) = number_arg(&f.args, src, scope, depth)?;
    let target = unquote(&eval(f.args.last()?, src, scope, depth + 1));
    let (group, from) = unit_factor(u.as_deref()?)?;
    let (target_group, to) = unit_factor(&target)?;
    // 不同组不做换算（less 的 `convertTo` 只处理命中的组）
    let v = if group == target_group {
        v * from / to
    } else {
        v
    };
    Some(fmt_with_unit(v, Some(target)))
}

/// 颜色混合模式（less `functions/color-blending.js`，W3C 合成公式）。
fn blend(mode: &str, backdrop: Color, source: Color) -> Color {
    let f = |cb: f64, cs: f64| match mode {
        "multiply" => cb * cs,
        "screen" => cb + cs - cb * cs,
        "overlay" => {
            let cb = cb * 2.0;
            if cb <= 1.0 {
                cb * cs
            } else {
                let (cb, cs) = (cb - 1.0, cs);
                cb + cs - cb * cs
            }
        }
        "softlight" => {
            let (d, e) = if cs > 0.5 {
                let d = if cb > 0.25 {
                    cb.sqrt()
                } else {
                    ((16.0 * cb - 12.0) * cb + 4.0) * cb
                };
                (d, 1.0)
            } else {
                (1.0, cb)
            };
            cb - (1.0 - 2.0 * cs) * e * (d - cb)
        }
        // hardlight 就是 overlay 的参数互换：overlay(cs, cb)
        "hardlight" => {
            let cs2 = cs * 2.0;
            if cs2 <= 1.0 {
                cs2 * cb
            } else {
                let (cs2, cb) = (cs2 - 1.0, cb);
                cs2 + cb - cs2 * cb
            }
        }
        "difference" => (cb - cs).abs(),
        "exclusion" => cb + cs - 2.0 * cb * cs,
        "average" => (cb + cs) / 2.0,
        _ => 1.0 - (cb + cs - 1.0).abs(),
    };
    let (ab, as_) = (backdrop.a, source.a);
    let ar = as_ + ab * (1.0 - as_);
    let mut ch = [0.0f64; 3];
    for (i, slot) in ch.iter_mut().enumerate() {
        let cb = [backdrop.r, backdrop.g, backdrop.b][i] / 255.0;
        let cs = [source.r, source.g, source.b][i] / 255.0;
        let mut cr = f(cb, cs);
        if ar != 0.0 {
            cr = (as_ * cs + ab * (cb - as_ * (cb + cs - cr))) / ar;
        }
        *slot = cr * 255.0;
    }
    Color {
        r: ch[0],
        g: ch[1],
        b: ch[2],
        a: ar,
    }
}

/// `hsv(h, s, v, a)`：less 的 perm 表实现。
fn hsv_color(h: f64, s: f64, v: f64, a: f64) -> Color {
    let h = ((h % 360.0) / 360.0) * 360.0;
    let i = ((h / 60.0) % 6.0).floor();
    let f = h / 60.0 - i;
    let vs = [
        v,
        v * (1.0 - s),
        v * (1.0 - f * s),
        v * (1.0 - (1.0 - f) * s),
    ];
    let perm = [
        [0, 3, 1],
        [2, 0, 1],
        [1, 0, 3],
        [1, 2, 0],
        [3, 1, 0],
        [0, 1, 2],
    ];
    let p = perm[(i as usize).min(5)];
    Color {
        r: vs[p[0]] * 255.0,
        g: vs[p[1]] * 255.0,
        b: vs[p[2]] * 255.0,
        a,
    }
}

/// `contrast(color[, dark, light, threshold])`：按 luma 选深色或浅色。
///
/// 返回选中实参的**原文本**：less 直接交回那个 Color 节点，`#eee` 不会被拉长成 `#eeeeee`。
fn contrast_color(c: Color, args: &[String]) -> String {
    let pick = |i: usize, fallback: &str| -> (String, Color) {
        match args.get(i).and_then(|a| parse_color(a)) {
            Some(color) => (args[i].clone(), color),
            None => (
                fallback.to_string(),
                parse_color(fallback).expect("valid fallback"),
            ),
        }
    };
    let (mut dark_text, mut dark) = pick(1, "#000000");
    let (mut light_text, mut light) = pick(2, "#ffffff");
    if dark.luma() > light.luma() {
        std::mem::swap(&mut dark, &mut light);
        std::mem::swap(&mut dark_text, &mut light_text);
    }
    let threshold = args.get(3).and_then(|a| color_number(a)).unwrap_or(0.43);
    if c.luma() < threshold {
        light_text
    } else {
        dark_text
    }
}

/// `replace(string, pattern, replacement, flags)`：仅字面量模式（无正则元字符）。
fn replace_fn<'a>(
    args: &[ComponentValue<'a>],
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
) -> Option<String> {
    let groups = arg_groups(args);
    let subject = groups.first()?.first()?;
    let raw = eval(subject, src, scope, depth + 1);
    let quote = match subject {
        ComponentValue::LessEscapedStr(_) => None,
        _ => raw.chars().next().filter(|c| *c == '"' || *c == '\''),
    };
    let body = unquote(&raw);
    let pattern = unquote(&eval(groups.get(1)?.first()?, src, scope, depth + 1));
    let replacement = unquote(&eval(groups.get(2)?.first()?, src, scope, depth + 1));
    let flags = groups
        .get(3)
        .and_then(|g| g.first())
        .map(|n| unquote(&eval(n, src, scope, depth + 1)))
        .unwrap_or_default();
    // JavaScript 正则表达式语法不在支持范围内：带元字符时保留调用原样
    if pattern.is_empty() || pattern.chars().any(|c| "\\^$.|?*+()[]{}".contains(c)) {
        return None;
    }
    let global = flags.contains('g');
    let folded = body.to_lowercase();
    let needle = if flags.contains('i') {
        pattern.to_lowercase()
    } else {
        pattern.clone()
    };
    let haystack = if flags.contains('i') { &folded } else { &body };
    let mut out = String::new();
    let mut rest = 0usize;
    while let Some(pos) = haystack[rest..].find(&needle) {
        let at = rest + pos;
        out.push_str(&body[rest..at]);
        out.push_str(&replacement);
        rest = at + needle.len();
        if !global {
            break;
        }
    }
    out.push_str(&body[rest..]);
    Some(match quote {
        Some(q) => format!("{q}{out}{q}"),
        None => out,
    })
}

/// `boolean(cond)` / `if(cond, a, b)`：只对选中分支求值。
fn cond_function<'a>(
    name: &str,
    f: &Function<'a>,
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
) -> Option<String> {
    let groups = fn_arg_groups(f);
    let node = groups.first()?.first()?;
    let ComponentValue::LessCondition(cond) = node else {
        return None;
    };
    let params = std::collections::HashMap::new();
    let ctx = super::guard::Ctx {
        src,
        scope,
        params: &params,
        default: true,
    };
    let ok = super::guard::eval_single(cond, &ctx);
    if name == "boolean" {
        return Some(is_css(ok));
    }
    let branch = groups.get(if ok { 1 } else { 2 })?;
    Some(
        branch
            .iter()
            .map(|n| eval(n, src, scope, depth + 1))
            .collect::<Vec<_>>()
            .join(" "),
    )
}

// ---------------------------------------------------------------------------
// `calc()`（less `tree/call.js`）：calc 内关掉 math，只写回变量与函数的结果
// ---------------------------------------------------------------------------

/// calc 内部求值结果：数值（可丢括号）与其他（括号保留）。
enum CalcOut {
    Dim(String),
    Other(String),
}

impl CalcOut {
    fn text(&self) -> &str {
        match self {
            CalcOut::Dim(t) | CalcOut::Other(t) => t,
        }
    }
}

fn calc_text<'a>(f: &Function<'a>, src: &'a str, scope: &dyn Scope<'a>, depth: usize) -> String {
    let mut out: Vec<String> = Vec::new();
    for group in fn_arg_groups(f) {
        let mut parts: Vec<String> = Vec::new();
        for node in group {
            parts.push(calc_top(node, src, scope, depth + 1).text().to_string());
        }
        out.push(parts.join(" "));
    }
    format!("calc({})", out.join(", "))
}

/// calc 的顶层实参：括号全部剥掉（less 的 Expression 单值拍平）。
fn calc_top<'a>(
    cv: &ComponentValue<'a>,
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
) -> CalcOut {
    match cv {
        ComponentValue::CalcParenthesized(p) => calc_top(&p.expr, src, scope, depth + 1),
        _ => calc_value(cv, src, scope, depth + 1),
    }
}

/// 运算的操作数：带括号且结果不是数值时保留一层括号。
fn calc_operand<'a>(
    cv: &ComponentValue<'a>,
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
) -> CalcOut {
    if !matches!(cv, ComponentValue::CalcParenthesized(_)) {
        return calc_value(cv, src, scope, depth + 1);
    }
    match calc_top(cv, src, scope, depth + 1) {
        CalcOut::Dim(t) => CalcOut::Dim(t),
        CalcOut::Other(t) => CalcOut::Other(format!("({t})")),
    }
}

fn calc_value<'a>(
    cv: &ComponentValue<'a>,
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
) -> CalcOut {
    if depth > 32 {
        return CalcOut::Other(eval(cv, src, scope, depth));
    }
    match cv {
        ComponentValue::Calc(c) => {
            let left = calc_operand(&c.left, src, scope, depth + 1);
            let right = calc_operand(&c.right, src, scope, depth + 1);
            CalcOut::Other(format!(
                "{}{}{}",
                left.text(),
                calc_sep(&c.left, &c.right, &c.op.kind, src),
                right.text()
            ))
        }
        ComponentValue::Number(_)
        | ComponentValue::Dimension(_)
        | ComponentValue::Percentage(_) => CalcOut::Dim(eval(cv, src, scope, depth + 1)),
        ComponentValue::LessVariable(v) => calc_var(v.name.name, src, scope, depth, *cv.span()),
        ComponentValue::LessVariableCall(c) => {
            calc_var(c.variable.name.name, src, scope, depth, *cv.span())
        }
        ComponentValue::Function(f) => {
            let text = eval_function(f, src, scope, depth + 1);
            if function_name(f) == "calc" || parse_number(&text).is_none() {
                CalcOut::Other(text)
            } else {
                CalcOut::Dim(text)
            }
        }
        _ => CalcOut::Other(eval(cv, src, scope, depth + 1)),
    }
}

/// calc 内的变量：先按 `math: always` 求值（less 用 `_SELF` 包一层级联求值）。
fn calc_var<'a>(
    name: &str,
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
    span: oxc_css_parser::pos::Span,
) -> CalcOut {
    let numeric = match scope.var(name) {
        Some(VarRef::Ast(n)) => number_in(n, src, scope, depth + 1, true),
        Some(VarRef::List(v)) => parse_number(&v.join(" ")),
        Some(VarRef::Text(t)) => parse_number(&t),
        _ => None,
    };
    match numeric {
        Some((v, u)) => CalcOut::Dim(fmt_with_unit(v, u)),
        None => CalcOut::Other(var_value(name, src, scope, depth, Some(span))),
    }
}

/// 运算符两侧的空白：less 的 `Operation.isSpaced` 只看运算符前的空白。
fn calc_sep(
    left: &ComponentValue<'_>,
    right: &ComponentValue<'_>,
    kind: &CalcOperatorKind,
    src: &str,
) -> String {
    let op = match kind {
        CalcOperatorKind::Plus => "+",
        CalcOperatorKind::Minus => "-",
        CalcOperatorKind::Multiply => "*",
        CalcOperatorKind::Division => "/",
        CalcOperatorKind::Modulo => "%",
    };
    let (lo, hi) = (left.span().end, right.span().start);
    let gap = if lo <= hi && hi <= src.len() {
        &src[lo..hi]
    } else {
        ""
    };
    if gap.starts_with(|c: char| c.is_whitespace()) {
        format!(" {op} ")
    } else {
        op.to_string()
    }
}

/// 数值参数的精确求值：直接算 f64，避免「格式化成 8 位小数再解析」丢精度。
fn number_arg<'a>(
    args: &[ComponentValue<'a>],
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
) -> Option<(f64, Option<String>)> {
    number_in(args.first()?, src, scope, depth, false)
}

fn number_in<'a>(
    cv: &ComponentValue<'a>,
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
    paren: bool,
) -> Option<(f64, Option<String>)> {
    if depth > 32 {
        return None;
    }
    match cv {
        ComponentValue::Number(_)
        | ComponentValue::Dimension(_)
        | ComponentValue::Percentage(_) => parse_number(text(src, cv)),
        ComponentValue::LessVariable(v) => number_in_var(v.name.name, src, scope, depth, paren),
        ComponentValue::LessVariableCall(c) => {
            number_in_var(c.variable.name.name, src, scope, depth, paren)
        }
        ComponentValue::LessNegativeValue(n) => {
            number_in(&n.value, src, scope, depth + 1, paren).map(|(v, u)| (-v, u))
        }
        ComponentValue::LessPropertyVariable(p) => {
            prop_text(p.name.name, src, scope, depth + 1).and_then(|t| parse_number(&t))
        }
        ComponentValue::LessParenthesizedOperation(p) => {
            number_in(&p.operation, src, scope, depth + 1, true)
        }
        // min/max 等函数的实参是 calc 树：那里没有 calc() 的「关 math」语义
        ComponentValue::Calc(c) => {
            let (lv, lu) = number_in(&c.left, src, scope, depth + 1, paren)?;
            let (rv, ru) = number_in(&c.right, src, scope, depth + 1, paren)?;
            let rv = if matches!(c.op.kind, CalcOperatorKind::Plus | CalcOperatorKind::Minus) {
                match (lu.as_deref(), ru.as_deref()) {
                    (Some(l), Some(r)) => convert_unit(rv, r, l),
                    _ => rv,
                }
            } else {
                rv
            };
            let v = match c.op.kind {
                CalcOperatorKind::Plus => lv + rv,
                CalcOperatorKind::Minus => lv - rv,
                CalcOperatorKind::Multiply => lv * rv,
                CalcOperatorKind::Division => lv / rv,
                CalcOperatorKind::Modulo => lv % rv,
            };
            Some((v, lu.or(ru)))
        }
        ComponentValue::CalcParenthesized(p) => number_in(&p.expr, src, scope, depth + 1, paren),
        // 变量值里嵌套的数值函数（`floor((@a * 1.15))`）
        ComponentValue::Function(f) => {
            let (v, u) = number_in(f.args.first()?, src, scope, depth + 1, true)?;
            match function_name(f).as_str() {
                "ceil" => Some((v.ceil(), u)),
                "floor" => Some((v.floor(), u)),
                "round" => Some((v.round(), u)),
                "abs" => Some((v.abs(), u)),
                "percentage" => {
                    let v = if u.as_deref() == Some("%") {
                        v
                    } else {
                        v * 100.0
                    };
                    Some((v, Some("%".to_string())))
                }
                "unit" => {
                    let unit = unquote(text(src, f.args.get(1)?));
                    Some((v, Some(unit)))
                }
                _ => None,
            }
        }
        // math=parens-division：括号外的除法不计算
        ComponentValue::LessBinaryOperation(op) => {
            if matches!(op.op.kind, LessOperationOperatorKind::Division) && !paren {
                return None;
            }
            let (lv, lu) = number_in(&op.left, src, scope, depth + 1, paren)?;
            let (rv, ru) = number_in(&op.right, src, scope, depth + 1, paren)?;
            // 同 `operate`：只有 +/- 会做同组单位换算（乘法/除法只保留左单位）
            let rv = match (lu.as_deref(), ru.as_deref()) {
                (Some(l), Some(r))
                    if matches!(
                        op.op.kind,
                        LessOperationOperatorKind::Plus | LessOperationOperatorKind::Minus
                    ) =>
                {
                    convert_unit(rv, r, l)
                }
                _ => rv,
            };
            Some((arith(&op.op.kind, lv, rv), lu.or(ru)))
        }
        _ => None,
    }
}

fn number_in_var<'a>(
    name: &str,
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
    paren: bool,
) -> Option<(f64, Option<String>)> {
    match scope.var(name) {
        Some(VarRef::Ast(node)) => number_in(node, src, scope, depth + 1, paren),
        Some(VarRef::List(v)) => parse_number(&v.join(" ")),
        Some(VarRef::Text(t)) => parse_number(&t),
        Some(VarRef::Color(_)) => None,
        Some(VarRef::Ruleset(_)) => None,
        None => None,
    }
}

fn is_color_text(text: &str) -> bool {
    let t = text.trim();
    parse_color(t).is_some()
}

fn is_css(v: bool) -> String {
    if v {
        "true".to_string()
    } else {
        "false".to_string()
    }
}

/// less `escape`：JS `encodeURIComponent`（保留 `A-Za-z0-9-_.!~*'()`），
/// 再把 `= : # ; ( )` 转义。
fn escape_url(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for b in text.bytes() {
        let keep = b.is_ascii_alphanumeric()
            || matches!(b, b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'');
        if keep {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// less `%("format", args...)`：逐个实参替换第一个 `%s`/`%d`/`%a`，最后把 `%%` 收回 `%`。
fn format_string(args: &[String]) -> Option<String> {
    let first = args.first()?;
    let quote = first.chars().next().filter(|c| *c == '"' || *c == '\'');
    let mut out = match quote {
        Some(_) => unquote(first),
        None => first.clone(),
    };
    for arg in &args[1..] {
        let Some(pos) = out.find('%').filter(|p| {
            out[*p..]
                .chars()
                .nth(1)
                .is_some_and(|c| matches!(c, 's' | 'd' | 'a' | 'S' | 'D' | 'A'))
        }) else {
            break;
        };
        let token = out[pos + 1..].chars().next()?;
        let value = if matches!(token, 's' | 'S') && arg.starts_with(['"', '\'']) {
            unquote(arg)
        } else {
            arg.clone()
        };
        out.replace_range(pos..pos + 2, &value);
    }
    Some(match quote {
        Some(q) => format!("{q}{}{q}", out.replace("%%", "%")),
        None => out.replace("%%", "%"),
    })
}

/// 函数参数按源区间拼接：逗号与空白来自源文本（args 里可能含 Delimiter 节点）。
fn join_args<'a>(
    args: &[ComponentValue<'a>],
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
) -> String {
    join_seq(args, src, scope, depth)
}

pub(super) fn function_name(f: &Function) -> String {
    match &f.name {
        FunctionName::Ident(InterpolableIdent::Literal(ident)) => ident.name.to_string(),
        // `%("...", args)`：less 的 format 函数
        FunctionName::LessFormatFunction(_) => "%".to_string(),
        other => format!("{other:?}"),
    }
}

fn fmt_with_unit(v: f64, unit: Option<String>) -> String {
    match unit {
        Some(u) => format!("{}{u}", fmt_number(v)),
        None => fmt_number(v),
    }
}

// ---------------------------------------------------------------------------
// 数字
// ---------------------------------------------------------------------------

/// `"10px"` → `(10.0, Some("px"))`；`"0.5"` → `(0.5, None)`；`"50%"` → `(50.0, Some("%"))`。
pub(crate) fn parse_number(s: &str) -> Option<(f64, Option<String>)> {
    let s = s.trim();
    let b = s.as_bytes();
    let mut i = 0;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        i += 1;
    }
    let digits = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    if i < b.len() && b[i] == b'.' {
        i += 1;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
    }
    if i == digits {
        return None;
    }
    // 指数部分：`e`/`E` 后必须紧跟（可带符号的）数字，否则当作单位开头（如 `em`）
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        let mut j = i + 1;
        if j < b.len() && (b[j] == b'+' || b[j] == b'-') {
            j += 1;
        }
        let exp = j;
        while j < b.len() && b[j].is_ascii_digit() {
            j += 1;
        }
        if j > exp {
            i = j;
        }
    }
    let (num, unit) = s.split_at(i);
    let v: f64 = num.parse().ok()?;
    let unit = unit.trim();
    Some((v, (!unit.is_empty()).then(|| unit.to_string())))
}

/// less `Node.fround`：`Number((value + 2e-16).toFixed(8))`，偏置避免二进制下溢把
/// 8 位小数卡在进位边界（`0.66666666` vs `0.66666667`）。
fn round8(v: f64) -> f64 {
    ((v + 2e-16) * 1e8).round() / 1e8
}

thread_local! {
    /// 求值期（选择器/属性名插值）的数值格式：less 在 `eval` 阶段用 `String(value)`，
    /// 只有 `genCSS` 阶段才受 `numPrecision: 8` 约束。
    static PRECISE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// 在「求值期数值不经 8 位截断」的模式下执行。
pub(crate) fn precise<R>(f: impl FnOnce() -> R) -> R {
    PRECISE.with(|p| {
        let old = p.replace(true);
        let out = f();
        p.set(old);
        out
    })
}

/// JS `Number.prototype.toString` 的最短往返形式（求值期用）。
fn fmt_number_exact(v: f64) -> String {
    if !v.is_finite() || v == 0.0 {
        return "0".to_string();
    }
    format!("{v}")
}

/// JS `Number.prototype.toString` 的近似：整数不带 `.0`，小数最多 8 位并去尾随 0。
pub(crate) fn fmt_number(v: f64) -> String {
    if PRECISE.with(|p| p.get()) {
        return fmt_number_exact(v);
    }
    if !v.is_finite() {
        return "0".to_string();
    }
    if v == 0.0 {
        return "0".to_string();
    }
    if v.fract() == 0.0 && v.abs() < 1e15 {
        return format!("{}", v as i64);
    }
    let mut s = format!("{:.8}", v + 2e-16);
    while s.ends_with('0') {
        s.pop();
    }
    if s.ends_with('.') {
        s.pop();
    }
    if s == "-0" {
        return "0".to_string();
    }
    if s.starts_with('.') {
        s.insert(0, '0');
    } else if s.starts_with("-.") {
        s.insert(1, '0');
    }
    s
}

/// 去掉值两端成对的引号（字符串变量参与插值/变量变量时用）。
fn unquote(s: &str) -> String {
    unquote_ref(s).to_string()
}

pub(super) fn unquote_ref(s: &str) -> &str {
    for q in ['"', '\''] {
        if s.len() >= 2 && s.starts_with(q) && s.ends_with(q) {
            return &s[1..s.len() - 1];
        }
    }
    s
}

// ---------------------------------------------------------------------------
// 颜色
// ---------------------------------------------------------------------------

/// less 的颜色：通道是浮点。HSL→RGB 不取整，只在输出时 `Math.round`，
/// 因此 `darken(darken(#428bca, 6.5%), 15%)` 这类链式调用必须保留中间精度。
#[derive(Debug, Clone, Copy)]
pub(crate) struct Color {
    pub(crate) r: f64,
    pub(crate) g: f64,
    pub(crate) b: f64,
    pub(crate) a: f64,
}

impl Color {
    pub(crate) fn to_css(self) -> String {
        let ch = |v: f64| v.round().clamp(0.0, 255.0) as u8;
        let a = round8(self.a).clamp(0.0, 1.0);
        if a >= 1.0 {
            format!("#{:02x}{:02x}{:02x}", ch(self.r), ch(self.g), ch(self.b))
        } else {
            format!(
                "rgba({}, {}, {}, {})",
                ch(self.r),
                ch(self.g),
                ch(self.b),
                fmt_number(a)
            )
        }
    }

    /// less `Color.toHSL`：h 为度，s/l 为 0..1。
    pub(crate) fn to_hsl(self) -> (f64, f64, f64) {
        let (r, g, b) = (self.r / 255.0, self.g / 255.0, self.b / 255.0);
        let (max, min) = (r.max(g).max(b), r.min(g).min(b));
        let l = (max + min) / 2.0;
        let d = max - min;
        if max == min {
            return (0.0, 0.0, l);
        }
        let s = if l > 0.5 {
            d / (2.0 - max - min)
        } else {
            d / (max + min)
        };
        let h = if max == r {
            (g - b) / d + if g < b { 6.0 } else { 0.0 }
        } else if max == g {
            (b - r) / d + 2.0
        } else {
            (r - g) / d + 4.0
        };
        (h / 6.0 * 360.0, s, l)
    }

    /// less `functions.hsla`：h 为度，s/l/a 为 0..1。
    pub(crate) fn from_hsl(h: f64, s: f64, l: f64, a: f64) -> Self {
        let h = (h % 360.0) / 360.0;
        let (s, l, a) = (s.clamp(0.0, 1.0), l.clamp(0.0, 1.0), a.clamp(0.0, 1.0));
        let m2 = if l <= 0.5 {
            l * (s + 1.0)
        } else {
            l + s - l * s
        };
        let m1 = l * 2.0 - m2;
        let hue = |h: f64| {
            let h = if h < 0.0 {
                h + 1.0
            } else if h > 1.0 {
                h - 1.0
            } else {
                h
            };
            if h * 6.0 < 1.0 {
                m1 + (m2 - m1) * h * 6.0
            } else if h * 2.0 < 1.0 {
                m2
            } else if h * 3.0 < 2.0 {
                m1 + (m2 - m1) * (2.0 / 3.0 - h) * 6.0
            } else {
                m1
            }
        };
        Self {
            r: hue(h + 1.0 / 3.0) * 255.0,
            g: hue(h) * 255.0,
            b: hue(h - 1.0 / 3.0) * 255.0,
            a,
        }
    }

    /// less `toARGB`：`#aarrggbb`。
    fn to_argb(self) -> String {
        let ch = |v: f64| v.round().clamp(0.0, 255.0) as u8;
        format!(
            "#{:02x}{:02x}{:02x}{:02x}",
            ch(self.a * 255.0),
            ch(self.r),
            ch(self.g),
            ch(self.b)
        )
    }

    /// less `Color.luma()`（WCAG 相对亮度，带 gamma 校正），比值为 0..1。
    fn luma(self) -> f64 {
        let f = |c: f64| {
            let c = c / 255.0;
            if c <= 0.03928 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * f(self.r) + 0.7152 * f(self.g) + 0.0722 * f(self.b)
    }

    /// less `luminance()`：未做 gamma 校正的加权亮度，比值为 0..1。
    fn luminance(self) -> f64 {
        0.2126 * self.r / 255.0 + 0.7152 * self.g / 255.0 + 0.0722 * self.b / 255.0
    }

    /// less `toHSV`：h 为度，s/v 为 0..1。
    fn to_hsv(self) -> (f64, f64, f64) {
        let (r, g, b) = (self.r / 255.0, self.g / 255.0, self.b / 255.0);
        let (max, min) = (r.max(g).max(b), r.min(g).min(b));
        let d = max - min;
        let h = if d == 0.0 {
            0.0
        } else if max == r {
            ((g - b) / d).rem_euclid(6.0)
        } else if max == g {
            (b - r) / d + 2.0
        } else {
            (r - g) / d + 4.0
        };
        (h * 60.0, if max == 0.0 { 0.0 } else { d / max }, max)
    }
}

const NAMED: &[(&str, u32)] = &[
    ("black", 0x000000),
    ("white", 0xffffff),
    ("red", 0xff0000),
    ("green", 0x008000),
    ("blue", 0x0000ff),
    ("yellow", 0xffff00),
    ("cyan", 0x00ffff),
    ("magenta", 0xff00ff),
    ("gray", 0x808080),
    ("grey", 0x808080),
    ("orange", 0xffa500),
    ("purple", 0x800080),
    ("pink", 0xffc0cb),
    ("brown", 0xa52a2a),
    ("silver", 0xc0c0c0),
    ("transparent", 0x000000),
];

/// `number(n)`：百分比取 `/100`，其余取原值（less `functions/color.js`）。
fn color_number(text: &str) -> Option<f64> {
    parse_number(text).map(|(v, u)| {
        if u.as_deref() == Some("%") {
            v / 100.0
        } else {
            v
        }
    })
}

pub(crate) fn parse_color(s: &str) -> Option<Color> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix('#') {
        if !hex.is_ascii() {
            return None;
        }
        let pair = |i: usize| -> Option<f64> {
            u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16)
                .ok()
                .map(f64::from)
        };
        let single = |i: usize| -> Option<f64> {
            u8::from_str_radix(&hex[i..i + 1].repeat(2), 16)
                .ok()
                .map(f64::from)
        };
        return match hex.len() {
            3 => Some(Color {
                r: single(0)?,
                g: single(1)?,
                b: single(2)?,
                a: 1.0,
            }),
            4 => Some(Color {
                r: single(0)?,
                g: single(1)?,
                b: single(2)?,
                a: single(3)? / 255.0,
            }),
            6 => Some(Color {
                r: pair(0)?,
                g: pair(1)?,
                b: pair(2)?,
                a: 1.0,
            }),
            8 => Some(Color {
                r: pair(0)?,
                g: pair(1)?,
                b: pair(2)?,
                a: pair(3)? / 255.0,
            }),
            _ => None,
        };
    }
    let lower = s.to_ascii_lowercase();
    let funcs = [
        ("rgba(", true),
        ("rgb(", false),
        ("hsla(", true),
        ("hsl(", false),
    ];
    for (name, has_alpha) in funcs {
        if lower.starts_with(name) && s.ends_with(')') {
            let inner = &s[name.len()..s.len() - 1];
            let slash_alpha = inner.contains('/') && !inner.contains("//");
            let parts = color_parts(inner);
            let alpha = parts.3.as_deref().and_then(color_number).unwrap_or(1.0);
            return if name.contains('l') {
                let h = parse_number(&parts.0)?.0;
                let (s, l) = (color_number(&parts.1)?, color_number(&parts.2)?);
                Some(Color::from_hsl(h, s, l, alpha))
            } else {
                let (r, g, b) = (
                    color_channel(&parts.0)?,
                    color_channel(&parts.1)?,
                    color_channel(&parts.2)?,
                );
                Some(Color {
                    r,
                    g,
                    b,
                    a: if has_alpha || slash_alpha { alpha } else { 1.0 },
                })
            };
        }
    }
    NAMED.iter().find(|(n, _)| *n == lower).map(|(_, v)| Color {
        r: f64::from((v >> 16) as u8),
        g: f64::from((v >> 8) as u8),
        b: f64::from(*v as u8),
        a: 1.0,
    })
}

/// 颜色函数的参数：逗号分隔（旧语法）或空白 + `/ alpha`（新语法）。
fn color_parts(inner: &str) -> (String, String, String, Option<String>) {
    let inner = inner.trim();
    let (main, alpha) = match inner.split_once('/') {
        Some((m, a)) if !m.contains("//") => (m.trim(), Some(a.trim().to_string())),
        _ => (inner, None),
    };
    let parts: Vec<&str> = main
        .split([',', ' '])
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();
    let at = |i: usize| parts.get(i).map_or(String::new(), |p| p.to_string());
    (
        at(0),
        at(1),
        at(2),
        alpha.or_else(|| parts.get(3).map(|p| p.to_string())),
    )
}

/// rgb 通道：百分比按 255 缩放，其余原值（less `scaled`）。
fn color_channel(text: &str) -> Option<f64> {
    match parse_number(text) {
        Some((v, Some(u))) if u == "%" => Some(v * 255.0 / 100.0),
        Some((v, _)) => Some(v),
        None => None,
    }
}

/// 结构化取色：沿 AST 求值颜色，保留 HSL→RGB 的浮点通道。
///
/// 文本求值会把颜色写成 `#rrggbb`，链式调用（`darken(spin(@c, -10), 5%)`）会因此丢精度；
/// less 传的是 Color 对象，所以这里沿 AST 递归。
fn color_arg<'a>(
    cv: &ComponentValue<'a>,
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
) -> Option<Color> {
    if depth > 32 {
        return None;
    }
    match cv {
        ComponentValue::HexColor(_) => parse_color(text(src, cv)),
        ComponentValue::LessVariable(v) => color_in_var(v.name.name, src, scope, depth),
        ComponentValue::LessVariableCall(c) => {
            color_in_var(c.variable.name.name, src, scope, depth)
        }
        ComponentValue::Function(f) => color_function(f, src, scope, depth),
        // `$prop`：取属性值文本再当颜色（`lighten($color, 10%)`）
        ComponentValue::LessPropertyVariable(p) => {
            prop_text(p.name.name, src, scope, depth + 1).and_then(|t| parse_color(&t))
        }
        // 命名颜色（`red`/`transparent`）等纯文本颜色
        _ => parse_color(text(src, cv).trim()),
    }
}

fn color_in_var<'a>(
    name: &str,
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
) -> Option<Color> {
    match scope.var(name) {
        Some(VarRef::Ast(node)) => color_arg(node, src, scope, depth + 1),
        Some(VarRef::List(v)) => parse_color(&v.join(" ")),
        Some(VarRef::Text(t)) => parse_color(&t),
        Some(VarRef::Color(c)) => Some(c),
        Some(VarRef::Ruleset(_)) => None,
        None => None,
    }
}

/// 供编译器绑定实参用：AST 求值成颜色（保留浮点通道）。
pub(super) fn color_of<'a>(
    cv: &ComponentValue<'a>,
    src: &'a str,
    scope: &dyn Scope<'a>,
) -> Option<Color> {
    // 字面量（含沿变量链找到的字面量）保留源文本：`#333` 不应该展开成 `#333333`
    if literal_color_text(cv, src, scope, 0).is_some() {
        return None;
    }
    color_arg(cv, src, scope, 0)
}

/// 颜色值的字面量源文本：`#333`、`#FFAA00`、`red`，沿变量链查找。
fn literal_color_text<'a>(
    cv: &ComponentValue<'a>,
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
) -> Option<String> {
    if depth > 32 {
        return None;
    }
    match cv {
        ComponentValue::HexColor(_) => Some(text(src, cv).to_string()),
        ComponentValue::InterpolableIdent(InterpolableIdent::Literal(i))
            if parse_color(i.name).is_some() =>
        {
            Some(i.name.to_string())
        }
        ComponentValue::LessVariable(v) => literal_color_in_var(v.name.name, src, scope, depth),
        ComponentValue::LessVariableCall(c) => {
            literal_color_in_var(c.variable.name.name, src, scope, depth)
        }
        _ => None,
    }
}

fn literal_color_in_var<'a>(
    name: &str,
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
) -> Option<String> {
    match scope.var(name) {
        Some(VarRef::Ast(node)) => literal_color_text(node, src, scope, depth + 1),
        Some(VarRef::List(v)) => parse_color(&v.join(" ")).map(|_| v.join(" ")),
        Some(VarRef::Text(t)) => parse_color(&t).map(|_| t),
        _ => None,
    }
}

fn color_function<'a>(
    f: &Function<'a>,
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
) -> Option<Color> {
    let name = function_name(f);
    // 逗号在 AST 里是独立节点：按参数序号取节点时必须过滤
    let vals: Vec<&ComponentValue> = f
        .args
        .iter()
        .filter(|a| !matches!(a, ComponentValue::Delimiter(_)))
        .collect();
    let at = |i: usize| vals.get(i).copied();
    let number = |i: usize| at(i).and_then(|a| number_in(a, src, scope, depth + 1, true));
    let color = |i: usize| at(i).and_then(|a| color_arg(a, src, scope, depth + 1));
    let relative = at(2).is_some_and(|a| text(src, a).trim() == "relative");
    Some(match name.as_str() {
        "greyscale" => adjust(color(0)?, "greyscale", 0.0, false)?,
        "lighten" | "darken" | "saturate" | "desaturate" | "spin" => {
            adjust(color(0)?, &name, number(1)?.0, relative)?
        }
        "fade" | "fadein" | "fadeout" => fade(color(0)?, &name, number(1)?.0),
        "mix" => mix_colors(
            color(0)?,
            color(1)?,
            number(2).map_or(0.5, |(v, _)| v / 100.0),
        ),
        "tint" => mix_colors(
            parse_color("#ffffff")?,
            color(0)?,
            number(1).map_or(0.5, |(v, _)| v / 100.0),
        ),
        "shade" => mix_colors(
            parse_color("#000000")?,
            color(0)?,
            number(1).map_or(0.5, |(v, _)| v / 100.0),
        ),
        "rgb" | "rgba" => parse_color(&format!(
            "{}({})",
            name,
            join_args(&f.args, src, scope, depth)
        ))?,
        "hsl" | "hsla" => {
            let args = eval_args(f, src, scope, depth);
            let (h, s, l, a) = hsl_args(&args)?;
            Color::from_hsl(h, s, l, a)
        }
        _ => return None,
    })
}

/// `lighten`/`darken`/`saturate`/`desaturate`/`spin`/`greyscale` 共用的 HSL 调整。
fn adjust(c: Color, name: &str, amount: f64, relative: bool) -> Option<Color> {
    let (h, s, l) = c.to_hsl();
    let d = amount / 100.0;
    let (h, s, l) = match name {
        // less: greyscale = desaturate(color, 100)
        "greyscale" => (h, 0.0, l),
        "lighten" => (h, s, clamp01(if relative { l + l * d } else { l + d })),
        "darken" => (h, s, clamp01(if relative { l - l * d } else { l - d })),
        "saturate" => (h, clamp01(if relative { s + s * d } else { s + d }), l),
        "desaturate" => (h, clamp01(if relative { s - s * d } else { s - d }), l),
        "spin" => (h + amount, s, l),
        _ => return None,
    };
    Some(Color::from_hsl(h, s, l, c.a))
}

fn fade(c: Color, name: &str, amount: f64) -> Color {
    let d = amount / 100.0;
    let a = match name {
        "fade" => d,
        "fadein" => c.a + d,
        _ => c.a - d,
    };
    Color { a: clamp01(a), ..c }
}

/// less `mix`（带 alpha 加权的 Sass 公式）。
fn mix_colors(a: Color, b: Color, weight: f64) -> Color {
    let w = weight * 2.0 - 1.0;
    let da = a.a - b.a;
    let w1 = (if w * da == -1.0 {
        w
    } else {
        (w + da) / (1.0 + w * da)
    } + 1.0)
        / 2.0;
    let w2 = 1.0 - w1;
    let mix = |x: f64, y: f64| x * w1 + y * w2;
    Color {
        r: mix(a.r, b.r),
        g: mix(a.g, b.g),
        b: mix(a.b, b.b),
        a: a.a * weight + b.a * (1.0 - weight),
    }
}

fn clamp01(v: f64) -> f64 {
    v.clamp(0.0, 1.0)
}

/// 函数参数逐个求值（去掉逗号节点）。
fn eval_args<'a>(
    f: &Function<'a>,
    src: &'a str,
    scope: &dyn Scope<'a>,
    depth: usize,
) -> Vec<String> {
    f.args
        .iter()
        .filter(|a| !matches!(a, ComponentValue::Delimiter(_)))
        .map(|a| eval(a, src, scope, depth + 1))
        .collect()
}

/// `hsl()`/`hsla()` 的 (h, s, l, a)：`number()` 语义，百分比取 `/100`。
fn hsl_args(args: &[String]) -> Option<(f64, f64, f64, f64)> {
    let h = parse_number(arg_at(args, 0))?.0;
    let s = color_number(arg_at(args, 1))?;
    let l = color_number(arg_at(args, 2))?;
    let a = args.get(3).and_then(|a| color_number(a)).unwrap_or(1.0);
    Some((h, s, l, a))
}

/// `hsl()`/`hsla()` 输出：less 先转 RGB 再转回 HSL（取整发生在 `fround`）。
fn hsl_text(args: &[String]) -> Option<String> {
    // `hsla(color, alpha)`：less 允许把颜色作为第一个参数
    if let Some(c) = args.first().and_then(|a| parse_color(a)) {
        let a = args.get(1).and_then(|a| color_number(a)).unwrap_or(c.a);
        let (h, s, l) = c.to_hsl();
        return Some(hsl_css(h, s, l, a));
    }
    let (h, s, l, a) = hsl_args(args)?;
    let (h, s, l) = Color::from_hsl(h, s, l, a).to_hsl();
    Some(hsl_css(h, s, l, a))
}

fn hsl_css(h: f64, s: f64, l: f64, a: f64) -> String {
    let a = round8(a).clamp(0.0, 1.0);
    let (h, s, l) = (fmt_number(h), fmt_number(s * 100.0), fmt_number(l * 100.0));
    if a >= 1.0 {
        format!("hsl({h}, {s}%, {l}%)")
    } else {
        format!("hsla({h}, {s}%, {l}%, {})", fmt_number(a))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn number_formatting() {
        assert_eq!(fmt_number(10.0), "10");
        assert_eq!(fmt_number(0.5), "0.5");
        assert_eq!(fmt_number(20.0), "20");
        assert_eq!(fmt_number(0.30000000000000004), "0.3");
    }

    #[test]
    fn color_lighten_matches_less() {
        // less: lighten(#333, 10%) => #4d4d4d
        let c = parse_color("#333").unwrap();
        let (h, s, l) = c.to_hsl();
        assert_eq!(Color::from_hsl(h, s, l + 0.1, 1.0).to_css(), "#4d4d4d");
    }

    #[test]
    fn color_darken_matches_less() {
        // less: darken(#333, 10%) => #1a1a1a
        let c = parse_color("#333").unwrap();
        let (h, s, l) = c.to_hsl();
        assert_eq!(
            Color::from_hsl(h, s, (l - 0.1).max(0.0), 1.0).to_css(),
            "#1a1a1a"
        );
    }

    #[test]
    fn unit_conversion_matches_less() {
        // less: 1cm + 10mm => 2cm；1px + 1em => 2px（不可换算时保留左单位）
        assert_eq!(
            operate(&LessOperationOperatorKind::Plus, "1cm", "10mm").as_deref(),
            Some("2cm")
        );
        assert_eq!(
            operate(&LessOperationOperatorKind::Plus, "1px", "1em").as_deref(),
            Some("2px")
        );
        assert_eq!(
            operate(&LessOperationOperatorKind::Plus, "1turn", "90deg").as_deref(),
            Some("1.25turn")
        );
    }

    #[test]
    fn color_arithmetic_matches_less() {
        // less: #333 + #111 => #444444；#888 * 0.5 => #444444
        assert_eq!(
            operate(&LessOperationOperatorKind::Plus, "#333", "#111").as_deref(),
            Some("#444444")
        );
        assert_eq!(
            operate(&LessOperationOperatorKind::Multiply, "#888", "0.5").as_deref(),
            Some("#444444")
        );
        assert_eq!(
            operate(&LessOperationOperatorKind::Minus, "#888", "#111").as_deref(),
            Some("#777777")
        );
    }
}
