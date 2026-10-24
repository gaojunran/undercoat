//! less `:extend` 求值。
//!
//! 语义对齐 less@4.1.3 `visitors/extend-visitor.js`：
//! - 精确 extend 要求整条选择器逐元素相等（组合子也要相等），`all` 才允许前后有余量；
//! - 元素级匹配（`.a.b` 是两个元素），`all` 命中后按元素区间替换；
//! - 链式传递（`doExtendChaining`）用 `parent_ids` 剪断环，最多 100 轮；
//! - 媒体块自成作用域：块内 extend 只在块内生效，外层 extend 继承进去。

use oxc_css_parser::pos::Span;

/// 选择器元素：组合子 + 简单选择器文本。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Element {
    /// 自身前面的组合子：`""` / `" "` / `">"` / `"+"` / `"~"` / `"|"`
    pub(crate) combinator: String,
    pub(crate) value: String,
}

impl Element {
    fn new(combinator: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            combinator: combinator.into(),
            value: value.into(),
        }
    }

    fn is_parent(&self) -> bool {
        self.value == "&"
    }
}

/// 展平后的复杂选择器：`&` 联结与嵌套在组合阶段已经消解。
pub(crate) type Sel = Vec<Element>;

/// 一条 `:extend(目标 [all])`。
#[derive(Clone, Debug)]
pub(crate) struct Need {
    pub(crate) target: Sel,
    pub(crate) all: bool,
}

fn value_eq(a: &Element, b: &Element) -> bool {
    a.value == b.value || norm(&a.value) == norm(&b.value)
}

/// 比较用的规范化形式：去空白；属性选择器再按 less 的 `Attribute` 分支去引号
/// （`[data-x="1"]` 与 `[data-x=1]` 相等）。
///
/// `:nth-*(An+B)` 例外：An+B 在 less 里是表达式节点，`2n + 1`（加法）与
/// `2n+ 1`（`2n` 后跟 `+1`）结构不同，不能整段去空白后比较。
fn norm(value: &str) -> String {
    if let Some(canonical) = anb_canonical(value) {
        return canonical;
    }
    let mut out = String::with_capacity(value.len());
    let attr = value.starts_with('[');
    for ch in value.chars() {
        if ch.is_whitespace() {
            continue;
        }
        if attr && (ch == '"' || ch == '\'') {
            continue;
        }
        out.push(ch);
    }
    out
}

/// `:nth-child()` / `:nth-of-type()` / `:nth-last-*()` 的 An+B 是 less 的表达式节点，
/// 比较时用其源文本：`2n+1` 命中，而 `2n + 1` / `2n+ 1` / ` 2n+1 ` 都不命中
/// （实测 less@4.1.3）。整段去空白会把它们误判为相等，所以这里原样返回。
fn anb_canonical(value: &str) -> Option<String> {
    const NTH: [&str; 4] = [
        ":nth-child(",
        ":nth-of-type(",
        ":nth-last-child(",
        ":nth-last-of-type(",
    ];
    let lower = value.to_ascii_lowercase();
    NTH.iter()
        .any(|p| lower.starts_with(p))
        .then(|| value.to_string())
}

/// 把一条复杂选择器文本切成元素（与 less 的 `Selector.elements` 同构）。
pub(crate) fn parse(text: &str) -> Sel {
    let bytes = text.as_bytes();
    let mut out: Sel = Vec::new();
    let mut combinator = String::new();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if c.is_ascii_whitespace() {
            if combinator.is_empty() {
                combinator.push(' ');
            }
            i += 1;
            continue;
        }
        if matches!(c, '>' | '+' | '~' | '|') {
            combinator = c.to_string();
            i += 1;
            continue;
        }
        let start = i;
        if c == '[' {
            i = skip_balanced(bytes, i, b'[', b']');
        } else if c == ':' {
            i += 1;
            if bytes.get(i) == Some(&b':') {
                i += 1;
            }
            i = skip_ident(bytes, i);
            if bytes.get(i) == Some(&b'(') {
                i = skip_balanced(bytes, i, b'(', b')');
            }
        } else if c == '&' {
            // `&-suffix` 在 less 里是「`&` 元素 + 标签后缀」，不能并成一个元素
            i += 1;
        } else {
            if matches!(c, '.' | '#' | '*' | '%') {
                i += 1;
            }
            i = skip_ident(bytes, i);
            // 命名空间前缀（`svg|rect`、`*|el`）是同一个元素，`||` 才是组合子
            while bytes.get(i) == Some(&b'|') && bytes.get(i + 1) != Some(&b'|') {
                i = skip_ident(bytes, i + 1);
            }
            if i == start {
                i += 1;
            }
        }
        let value = text[start..i].to_string();
        out.push(Element {
            combinator: std::mem::take(&mut combinator),
            value,
        });
    }
    out
}

/// 跳过转义与标识符字符。
fn skip_ident(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'\\' {
            i += 2;
        } else if c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-') || c >= 0x80 {
            i += 1;
        } else {
            break;
        }
    }
    i.min(bytes.len())
}

/// 跳到配对的右括号（跳过引号内内容）。
fn skip_balanced(bytes: &[u8], start: usize, open: u8, close: u8) -> usize {
    let mut depth = 0usize;
    let mut quote = 0u8;
    let mut i = start;
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
        } else if c == b'"' || c == b'\'' {
            quote = c;
        } else if c == open {
            depth += 1;
        } else if c == close {
            depth -= 1;
            if depth == 0 {
                return i + 1;
            }
        }
        i += 1;
    }
    bytes.len()
}

/// 元素序列 → CSS 文本（组合子按 less `Combinator.genCSS` 的间距规则）。
pub(crate) fn render(sel: &Sel) -> String {
    let mut out = String::new();
    for (i, e) in sel.iter().enumerate() {
        match e.combinator.as_str() {
            " " => {
                if i > 0 {
                    out.push(' ');
                }
            }
            "" | "|" => out.push_str(&e.combinator),
            other => {
                out.push(' ');
                out.push_str(other);
                out.push(' ');
            }
        }
        out.push_str(&e.value);
    }
    out
}

/// 父路径 × 子选择器：`&` 逐次与父路径做笛卡尔积（less `joinSelector`）。
pub(crate) fn combine(parents: &[Sel], child: &Sel) -> Vec<Sel> {
    if !child.iter().any(Element::is_parent) {
        if parents.is_empty() {
            return vec![child.clone()];
        }
        return parents
            .iter()
            .map(|p| {
                let mut sel = p.clone();
                let mut tail = child.clone();
                // 父选择器与子选择器之间的连接等价于 genCSS 补的那个空格
                if let Some(first) = tail.first_mut() {
                    if first.combinator.is_empty() {
                        first.combinator.push(' ');
                    }
                }
                sel.extend(tail);
                sel
            })
            .collect();
    }
    let mut acc: Vec<Sel> = vec![Vec::new()];
    for el in child {
        if !el.is_parent() {
            for sel in &mut acc {
                sel.push(el.clone());
            }
            continue;
        }
        let mut next: Vec<Sel> = Vec::new();
        for sel in &acc {
            if parents.is_empty() {
                // 没有父选择器时 `&` 被抹掉；作为首个元素时连它后面的空白也不留
                if sel.is_empty() {
                    next.push(sel.clone());
                    continue;
                }
                let mut s = sel.clone();
                s.push(Element::new(&el.combinator, ""));
                next.push(s);
                continue;
            }
            for parent in parents {
                let mut s = sel.clone();
                let parent_first = parent.first();
                let combinator = match parent_first {
                    Some(p) if el.combinator.is_empty() || el.combinator == " " => {
                        if p.combinator.is_empty() || p.combinator == " " {
                            el.combinator.clone()
                        } else {
                            p.combinator.clone()
                        }
                    }
                    _ => el.combinator.clone(),
                };
                if let Some(p) = parent_first {
                    s.push(Element::new(combinator, &p.value));
                }
                s.extend(parent.iter().skip(1).cloned());
                next.push(s);
            }
        }
        acc = next;
    }
    acc.retain(|s| !s.is_empty());
    acc
}

/// 一次命中：haystack 的元素区间 `[start, end)`。
struct Hit {
    start: usize,
    end: usize,
}

/// less `findMatch`：needle 在 haystack 里滑动匹配（`all` 允许前后有余量、可多次命中）。
fn find_match(needle: &Sel, hay: &Sel, all: bool) -> Vec<Hit> {
    if needle.is_empty() {
        return Vec::new();
    }
    let mut pending: Vec<(usize, usize)> = Vec::new();
    let mut hits = Vec::new();
    for (idx, element) in hay.iter().enumerate() {
        if all || idx == 0 {
            pending.push((idx, 0));
        }
        // 路径里换到下一个选择器时 less 会补一个空格组合子
        let target_combinator = if idx == 0 && element.combinator.is_empty() {
            " "
        } else {
            element.combinator.as_str()
        };
        let mut i = 0;
        while i < pending.len() {
            let (start, matched) = pending[i];
            let ok = value_eq(&needle[matched], element)
                && (matched == 0 || needle[matched].combinator == target_combinator);
            if !ok {
                pending.remove(i);
                continue;
            }
            let next = matched + 1;
            if next < needle.len() {
                pending[i].1 = next;
                i += 1;
                continue;
            }
            if !all && idx + 1 < hay.len() {
                pending.remove(i);
                continue;
            }
            // 命中区间不重叠：从这里重新开始找
            pending.clear();
            hits.push(Hit {
                start,
                end: idx + 1,
            });
            break;
        }
    }
    hits
}

/// less `extendSelector`：把命中区间换成替换选择器。
fn extend_selector(hits: &[Hit], hay: &Sel, replacement: &Sel) -> Sel {
    let Some(first_value) = replacement.first() else {
        return hay.clone();
    };
    let mut out: Sel = Vec::with_capacity(hay.len());
    let mut cursor = 0;
    for hit in hits {
        out.extend(hay[cursor..hit.start].iter().cloned());
        out.push(Element::new(&hay[hit.start].combinator, &first_value.value));
        out.extend(replacement.iter().skip(1).cloned());
        cursor = hit.end;
    }
    out.extend(hay[cursor..].iter().cloned());
    out
}

/// 一个媒体/at-rule 作用域。
struct Scope {
    parent: Option<usize>,
    /// 本作用域内定义的 extend（定义顺序）
    extends: Vec<usize>,
}

/// 一条 extend 声明。
struct Extend {
    target: Sel,
    all: bool,
    /// 替换用的自身选择器（已展平）
    self_sel: Sel,
    /// `self_sel` 所在选择器路径的源区间（extend 生成的选择器带它做映射）
    self_span: Span,
    /// 是否为该选择器路径上的第一条 extend（决定链式产物是否落到该规则上）
    first: bool,
    /// 该 extend 是否可见（`(reference)` 导入体里的 extend 不可见，命中也不产出）
    visible: bool,
    /// 自身 + 祖先 extend 的 id（环检测）
    ancestors: Vec<usize>,
    ruleset: usize,
}

/// 一条已输出（或将要输出）的规则。
struct Rule {
    sels: Vec<Sel>,
    /// 与 `sels` 等长：每条路径的来源区间（映射用；`0..0` 表示无来源）
    spans: Vec<Span>,
    /// 与 `sels` 等长：该路径是否可见（`(reference)` 导入体里的路径默认不可见）
    visible: Vec<bool>,
    /// 与 `sels` 等长：该路径自己带 `:extend(...)`，不参与被扩展
    skip: Vec<bool>,
    /// 规则里有 `&:extend(...)`：整条规则都不参与被扩展
    skip_all: bool,
    scope: usize,
}

/// 展平选择器 + extend 的求值器。
pub(crate) struct Engine {
    scopes: Vec<Scope>,
    extends: Vec<Extend>,
    rules: Vec<Rule>,
    /// 每个作用域生效的 extend 列表（含链式产物）
    applied: Vec<Vec<usize>>,
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}

impl Engine {
    pub(crate) fn new() -> Self {
        Self {
            scopes: vec![Scope {
                parent: None,
                extends: Vec::new(),
            }],
            extends: Vec::new(),
            rules: Vec::new(),
            applied: Vec::new(),
        }
    }

    /// 进入一个新的媒体/at-rule 作用域。
    pub(crate) fn push_scope(&mut self, parent: Option<usize>) -> usize {
        self.scopes.push(Scope {
            parent,
            extends: Vec::new(),
        });
        self.scopes.len() - 1
    }

    /// 登记一条规则（`spans` 与 `sels` 等长），返回索引。
    ///
    /// `hidden` 表示规则来自 `(reference)` 导入体：路径默认不输出，只有被 extend
    /// 命中（less 的 `findMatch` 会给派生选择器 `ensureVisibility`）后才可见。
    pub(crate) fn add_rule(
        &mut self,
        sels: Vec<Sel>,
        spans: Vec<Span>,
        scope: usize,
        hidden: bool,
    ) -> usize {
        let skip = vec![false; sels.len()];
        let visible = vec![!hidden; sels.len()];
        self.rules.push(Rule {
            sels,
            spans,
            visible,
            skip,
            skip_all: false,
            scope,
        });
        self.rules.len() - 1
    }

    pub(crate) fn add_need(
        &mut self,
        need: &Need,
        self_sel: Sel,
        self_span: Span,
        ruleset: usize,
        first: bool,
        visible: bool,
    ) {
        let id = self.extends.len();
        self.extends.push(Extend {
            target: need.target.clone(),
            all: need.all,
            self_sel,
            self_span,
            first,
            visible,
            ancestors: vec![id],
            ruleset,
        });
        let scope = self.rules[ruleset].scope;
        self.scopes[scope].extends.push(id);
    }

    /// 规则里出现了 `&:extend(...)`：整条规则都不再被扩展。
    pub(crate) fn mark_skip_all(&mut self, ruleset: usize) {
        self.rules[ruleset].skip_all = true;
    }

    /// 该路径自身带 `:extend(...)`：不参与被扩展。
    pub(crate) fn mark_skip(&mut self, ruleset: usize, path: usize) {
        if let Some(skip) = self.rules[ruleset].skip.get_mut(path) {
            *skip = true;
        }
    }

    /// 规则最终可见的选择器及其来源区间（新增路径带它自己路径的来源）。
    ///
    /// `(reference)` 导入体的路径未命中 extend 时不可见，整条规则不输出。
    pub(crate) fn visible_selectors(&self, ruleset: usize) -> Vec<(Sel, Span)> {
        let rule = &self.rules[ruleset];
        rule.sels
            .iter()
            .zip(rule.spans.iter())
            .zip(rule.visible.iter())
            .filter(|(_, visible)| **visible)
            .map(|((sel, span), _)| (sel.clone(), *span))
            .collect()
    }

    /// 链式与主扫描（less `ProcessExtendsVisitor.run`）。
    pub(crate) fn run(&mut self) {
        let scope_count = self.scopes.len();
        self.applied = vec![Vec::new(); scope_count];
        for scope in 0..scope_count {
            let own = self.scopes[scope].extends.clone();
            let mut list = match self.scopes[scope].parent {
                Some(parent) => {
                    let mut list = own.clone();
                    list.extend(self.applied[parent].iter().cloned());
                    list
                }
                None => own.clone(),
            };
            let chained = self.chain(&list, &own, 0);
            list.extend(chained);
            self.applied[scope] = list;
        }
        for index in 0..self.rules.len() {
            let scope = self.rules[index].scope;
            let extends = self.applied[scope].clone();
            if self.rules[index].skip_all {
                continue;
            }
            let mut added: Vec<(Sel, Span, bool)> = Vec::new();
            let paths = self.rules[index].sels.len();
            for &extend in &extends {
                for path in 0..paths {
                    if self.rules[index].skip[path] {
                        continue;
                    }
                    let hay = &self.rules[index].sels[path];
                    let (target, all) = (&self.extends[extend].target, self.extends[extend].all);
                    let hits = find_match(target, hay, all);
                    if hits.is_empty() {
                        continue;
                    }
                    added.push((
                        extend_selector(&hits, hay, &self.extends[extend].self_sel),
                        self.extends[extend].self_span,
                        self.extends[extend].visible,
                    ));
                }
            }
            self.rules[index]
                .sels
                .extend(added.iter().map(|(s, _, _)| s.clone()));
            self.rules[index]
                .spans
                .extend(added.iter().map(|(_, s, _)| *s));
            self.rules[index]
                .visible
                .extend(added.iter().map(|(_, _, v)| *v));
        }
        // 新增路径默认参与被扩展（less 只在链式产物上挂 extendList）
        for rule in &mut self.rules {
            rule.skip.resize(rule.sels.len(), false);
            rule.visible.resize(rule.sels.len(), true);
            rule.spans
                .resize(rule.sels.len(), Span { start: 0, end: 0 });
        }
    }

    /// less `doExtendChaining`：A extend B、B extend C 时闭包求值。
    fn chain(&mut self, list: &[usize], targets: &[usize], depth: usize) -> Vec<usize> {
        if depth > 100 {
            return Vec::new();
        }
        let mut added: Vec<usize> = Vec::new();
        for &source in list {
            for &target in targets {
                if self.extends[source]
                    .ancestors
                    .contains(&self.extends[target].ancestors[0])
                {
                    continue;
                }
                let (needle, all) = (
                    self.extends[source].target.clone(),
                    self.extends[source].all,
                );
                let target_self = self.extends[target].self_sel.clone();
                let hits = find_match(&needle, &target_self, all);
                if hits.is_empty() {
                    continue;
                }
                let replacement = self.extends[source].self_sel.clone();
                let new_sel = extend_selector(&hits, &target_self, &replacement);
                let new_target = self.extends[target].target.clone();
                let new_all = self.extends[target].all;
                let ruleset = self.extends[target].ruleset;
                let first = self.extends[target].first;
                let self_span = self.extends[target].self_span;
                let id = self.extends.len();
                let mut ancestors = vec![id];
                ancestors.extend(self.extends[target].ancestors.iter().cloned());
                ancestors.extend(self.extends[source].ancestors.iter().cloned());
                // less：新 extend 继承 target 的可见性，新路径继承 source 的可见性
                let visible = self.extends[target].visible;
                let path_visible = self.extends[source].visible;
                self.extends.push(Extend {
                    target: new_target,
                    all: new_all,
                    self_sel: new_sel.clone(),
                    self_span,
                    first,
                    visible,
                    ancestors,
                    ruleset,
                });
                // 链式产物作为该规则的一条新路径（不再参与被扩展）
                if first {
                    self.rules[ruleset].sels.push(new_sel);
                    self.rules[ruleset].spans.push(self_span);
                    self.rules[ruleset].visible.push(path_visible);
                    self.rules[ruleset].skip.push(true);
                }
                added.push(id);
            }
        }
        if added.is_empty() {
            return added;
        }
        let more = self.chain(&added, targets, depth + 1);
        added.extend(more);
        added
    }
}
