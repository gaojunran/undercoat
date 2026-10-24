//! `@extend` 与占位符选择器（`%p`）。
//!
//! 语义按 dart-sass 的 `ExtensionStore`：扩展是**增量**应用的——每条新 `@extend`
//! 只作用在当时已经登记过的规则上，规则在登记时才吸收此刻已有的全部扩展。
//! 输出顺序正是这个过程的副产品（`.b, .e, .d, .c` 而非 `.b, .c, .d, .e`），
//! 所以这里记录「注册/扩展」事件流，最后按顺序回放，而不是一次性求闭包。
//!
//! 选择器模型只覆盖 SCSS 需要的部分：组合符 + 复合选择器（简单选择器切片）。
//! 复合选择器内部不展开 `:not(...)` / `:is(...)` 之类的嵌套选择器。

use oxc_css_parser::pos::Span;

use crate::doc::{Body, Item, selector_lines};

/// 一条复杂选择器。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Sel {
    pub(crate) parts: Vec<Part>,
    pub(crate) line_break: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Part {
    /// 前导组合符（`>`/`+`/`~`），`None` 表示后代或首段。
    pub(crate) combinator: Option<char>,
    pub(crate) compound: Compound,
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub(crate) struct Compound {
    pub(crate) simples: Vec<String>,
}

impl Compound {
    pub(crate) fn render(&self) -> String {
        self.simples.concat()
    }

    /// 复合选择器里有没有 `%p`（不管还有没有别的简单选择器）。
    fn has_placeholder(&self) -> bool {
        self.simples.iter().any(|s| s.starts_with('%'))
    }
}

impl Sel {
    pub(crate) fn render(&self) -> String {
        let mut out = String::new();
        for (i, part) in self.parts.iter().enumerate() {
            if i > 0 {
                match part.combinator {
                    Some(c) => {
                        out.push(' ');
                        out.push(c);
                        out.push(' ');
                    }
                    None => out.push(' '),
                }
            } else if let Some(c) = part.combinator {
                // 引导组合符（`> .b`）：选择器函数允许这种写法。
                out.push(c);
                out.push(' ');
            }
            out.push_str(&part.compound.render());
        }
        out
    }

    /// 末段组合符会被 `@extend` 的宿主组合符拼接，所以单独看「有多段」时忽略它。
    fn single_compound(&self) -> Option<&Compound> {
        match self.parts.as_slice() {
            [part] if part.combinator.is_none() => Some(&part.compound),
            _ => None,
        }
    }
}

/// 一条 `@extend` 登记。
#[derive(Clone)]
pub(crate) struct Extension {
    /// 扩展者（`@extend` 所在规则的选择器，可能有多条）。
    pub(crate) extenders: Vec<Sel>,
    /// 被扩展的复合选择器（`%p`, `.btn-sm`, `h1`）。
    pub(crate) target: Compound,
    pub(crate) optional: bool,
    pub(crate) span: usize,
}

/// 事件流：规则登记 / 扩展登记。回放顺序即 dart 的增量语义。
enum Event {
    Rule,
    Extend(Extension),
}

#[derive(Default)]
pub(crate) struct Engine {
    /// 每条规则的选择器（下标即登记顺序）。
    pub(crate) lists: Vec<Vec<Sel>>,
    events: Vec<Event>,
    /// 已登记规则数（回放时用）。
    registered: usize,
    /// 所有登记过的规则选择器里出现过的简单选择器。`!optional` 校验用它：
    /// dart 的 `_selectors` 表在整轮编译里是累积的，且校验发生在最后
    /// （所以 `@extend %p` 写在 `%p` 定义之前也合法）。
    known: std::collections::HashSet<String>,
    /// 已登记的扩展（按登记顺序）：新规则 / 新扩展者要立刻吸收它们。
    ext_list: Vec<Extension>,
    /// 哪些规则的选择器真的被改写过（没改过的保持 S2 的原文，避免往返渲染差异）。
    pub(crate) dirty: Vec<bool>,
}

impl Engine {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// 登记一条规则，返回它的编号。
    pub(crate) fn add_rule(&mut self, mut selectors: Vec<Sel>) -> usize {
        // 登记时刻一次性吸收已有扩展（整张表一起作用，顺序即登记顺序）。
        let table = self.table();
        let mut dirty = false;
        if !table.is_empty() {
            if let Some(extended) = extend_list(&selectors, &table) {
                selectors = extended;
                dirty = true;
            }
        }
        for sel in &selectors {
            for part in &sel.parts {
                for simple in &part.compound.simples {
                    self.known.insert(simple.clone());
                }
            }
        }
        let id = self.lists.len();
        self.lists.push(selectors);
        self.dirty.push(dirty);
        self.events.push(Event::Rule);
        self.registered += 1;
        id
    }

    /// 登记规则选择器（即使这条规则最终不产出 CSS，比如只写了 `@extend` 的
    /// 嵌套规则）：`!optional` 校验和扩展链都要看得到它。
    pub(crate) fn note_selectors(&mut self, selectors: &[Sel]) {
        for sel in selectors {
            for part in &sel.parts {
                for simple in &part.compound.simples {
                    self.known.insert(simple.clone());
                }
            }
        }
    }

    pub(crate) fn add_extension(&mut self, mut extension: Extension) {
        // 新扩展者本身可能已被既有扩展改写（`.navbar > .container-fluid` 的
        // `.container-fluid` 已被 `.container-*` 扩展）。
        let table = self.table();
        if !table.is_empty() {
            if let Some(extended) = extend_list(&extension.extenders, &table) {
                extension.extenders = extended;
            }
        }
        self.ext_list.push(extension.clone());
        self.events.push(Event::Extend(extension));
    }

    /// 当前扩展表（目标复合选择器 → 扩展），顺序即登记顺序。
    fn table(&self) -> Vec<(Compound, Extension)> {
        self.ext_list
            .iter()
            .map(|extension| (extension.target.clone(), extension.clone()))
            .collect()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// 回放事件流，把扩展结果写回 `lists`。
    pub(crate) fn resolve(&mut self) {
        let mut registered = 0usize;
        for index in 0..self.events.len() {
            match &self.events[index] {
                Event::Rule => registered += 1,
                Event::Extend(extension) => {
                    let extension = extension.clone();
                    let table = [(extension.target.clone(), extension.clone())];
                    for id in 0..registered {
                        let mut changed = false;
                        let updated = extend_list(&self.lists[id], &table);
                        if let Some(updated) = updated {
                            self.lists[id] = updated;
                            if let Some(flag) = self.dirty.get_mut(id) {
                                *flag = true;
                            }
                            changed = true;
                        }
                        let _ = changed;
                    }
                }
            }
            let _ = index;
        }
    }

    pub(crate) fn debug_lists(&self) {
        for (i, list) in self.lists.iter().enumerate() {
            eprintln!(
                "list {i}: {:?}",
                list.iter()
                    .map(|s| (
                        s.render(),
                        s.parts.iter().map(|p| p.combinator).collect::<Vec<_>>()
                    ))
                    .collect::<Vec<_>>()
            );
        }
    }

    /// 第一条没匹配到目标、又没写 `!optional` 的扩展的位置。
    pub(crate) fn unmatched(&self) -> Option<usize> {
        self.events.iter().find_map(|event| match event {
            Event::Extend(extension)
                if !extension.optional
                    && !extension
                        .target
                        .simples
                        .iter()
                        .all(|simple| self.known.contains(simple)) =>
            {
                Some(extension.span)
            }
            _ => None,
        })
    }
}

/// 一条扩展产物连同它的「来源扩展者」文本。
///
/// dart 的 `_trim` 只在**同一来源**的产物之间做覆盖判定：`.column` 与
/// `.column:last-child` 都被 `.columns` 扩展时，`.columns:last-child` 被 `.columns`
/// 覆盖而丢掉；不同 `@extend` 产出的 `.reveal.tiny` 与 `.reveal` 则都保留。
#[derive(Clone)]
struct Produced {
    sel: Sel,
    /// 是不是扩展产物（源码里原样的选择器是 `false`）。
    fresh: bool,
    /// 产物的来源扩展者（只有「单一扩展者参与」的产物才有）。
    origin: Option<String>,
}

impl Produced {
    fn plain(sel: Sel) -> Self {
        Self {
            sel,
            fresh: false,
            origin: None,
        }
    }
}

/// 用 `table`（目标复合选择器 → 扩展）扩展一组选择器；无变化时返回 `None`。
fn extend_list(list: &[Sel], table: &[(Compound, Extension)]) -> Option<Vec<Sel>> {
    let mut out: Vec<Produced> = Vec::with_capacity(list.len());
    let mut changed = false;
    for sel in list {
        match extend_sel(sel, table) {
            Some(result) => {
                changed = true;
                out.extend(result);
            }
            None => out.push(Produced {
                sel: sel.clone(),
                fresh: false,
                origin: Some(sel.render()),
            }),
        }
    }
    changed.then(|| trim(out))
}

/// `wide` 是否覆盖 `narrow`：段数、组合符一致或更宽松，且每段的简单选择器都是
/// `narrow` 的子集（`.b` 覆盖 `.b:hover`、`li` 覆盖 `li.a`）；完全相等也算。
fn subsumes(wide: &Sel, narrow: &Sel) -> bool {
    if wide.parts.len() != narrow.parts.len() {
        return false;
    }
    wide.parts.iter().zip(&narrow.parts).all(|(a, b)| {
        (a.combinator == b.combinator || a.combinator.is_none())
            && a.compound
                .simples
                .iter()
                .all(|s| b.compound.simples.contains(s))
    })
}

/// 扩展一条复杂选择器；`None` 表示没有变化。
fn extend_sel(sel: &Sel, table: &[(Compound, Extension)]) -> Option<Vec<Produced>> {
    // 每个位置的可选方案：`Some(vec![...])` 表示这一位置被展开。
    let mut options: Option<Vec<Vec<Produced>>> = None;
    for (i, part) in sel.parts.iter().enumerate() {
        let extended = extend_compound(&part.compound, table);
        match extended {
            None => {
                // 未命中扩展的位置单独成段（dart 用只含该 component 的复杂选择器）。
                if let Some(options) = options.as_mut() {
                    options.push(vec![Produced::plain(Sel {
                        parts: vec![part.clone()],
                        line_break: sel.line_break,
                    })]);
                }
            }
            Some(mut alternatives) => {
                if options.is_none() {
                    let mut initial = Vec::new();
                    if i != 0 {
                        initial.push(Produced::plain(prefix_sel(&sel.parts[..i], sel.line_break)));
                    }
                    options = Some(if initial.is_empty() {
                        Vec::new()
                    } else {
                        vec![initial]
                    });
                }
                // 扩展者自己没有组合符时，继承被扩展位置的组合符
                // （dart 的 `withAdditionalCombinators`）。
                for alternative in &mut alternatives {
                    if alternative.sel.parts[0].combinator.is_none() {
                        alternative.sel.parts[0].combinator = part.combinator;
                    }
                }
                options.as_mut().expect("刚初始化").push(alternatives);
            }
        }
    }
    let options = options?;
    // 笛卡尔积 → 每个路径做一次织入；第一条路径是「全部原样」，排在前面。
    let mut out = Vec::new();
    for path in paths(&options) {
        let origins = path
            .iter()
            .filter_map(|produced| produced.origin.clone())
            .collect::<Vec<_>>();
        let sels = path
            .iter()
            .map(|produced| produced.sel.clone())
            .collect::<Vec<_>>();
        let fresh = !origins.is_empty();
        let origin = (origins.len() == 1).then(|| origins[0].clone());
        for woven in weave(&sels, sel.line_break) {
            out.push(Produced {
                sel: woven,
                fresh,
                origin: origin.clone(),
            });
        }
    }
    Some(out)
}

fn prefix_sel(parts: &[Part], line_break: bool) -> Sel {
    Sel {
        parts: parts.to_vec(),
        line_break,
    }
}

/// 扩展一个复合选择器；`None` 表示没有扩展命中。
fn extend_compound(compound: &Compound, table: &[(Compound, Extension)]) -> Option<Vec<Produced>> {
    // 每个位置的可选方案：(来源扩展者, 选择器)。源码原样的方案来源是 `None`。
    let mut options: Option<Vec<Vec<Alternative>>> = None;
    for (i, simple) in compound.simples.iter().enumerate() {
        let keep = single_compound_sel(Compound {
            simples: vec![simple.clone()],
        });
        let extenders = extenders_for(simple, table);
        match extenders {
            None => {
                if let Some(options) = options.as_mut() {
                    options.push(vec![(None, keep)]);
                }
            }
            Some(list) => {
                if options.is_none() {
                    let mut initial = Vec::new();
                    if i != 0 {
                        initial.push((
                            None,
                            single_compound_sel(Compound {
                                simples: compound.simples[..i].to_vec(),
                            }),
                        ));
                    }
                    options = Some(if initial.is_empty() {
                        Vec::new()
                    } else {
                        vec![initial]
                    });
                }
                let position = options.as_mut().expect("刚初始化");
                let mut slot = vec![(None, keep)];
                slot.extend(list.into_iter().map(|sel| (Some(sel.clone()), sel)));
                position.push(slot);
            }
        }
    }
    let options = options?;
    let mut out = Vec::new();
    for path in paths_tagged(&options) {
        // 原样保留的简单选择器按原复合选择器的顺序排在前面；被扩展掉的那个不保留
        // （dart：`.column.a` + `.b` → `.a.b`）。
        let mut base: Vec<String> = Vec::new();
        let mut origins: Vec<Sel> = Vec::new();
        let mut extenders: Vec<Sel> = Vec::new();
        for (origin, sel) in &path {
            if let Some(origin) = origin {
                origins.push(origin.clone());
                extenders.push(sel.clone());
                continue;
            }
            let compound = sel.single_compound()?;
            base.extend(compound.simples.iter().cloned());
        }
        let fresh = !origins.is_empty();
        let origin = (origins.len() == 1).then(|| origins[0].render());
        let line_break = origins.iter().any(|sel| sel.line_break);
        if extenders.is_empty() {
            out.push(Produced {
                sel: single_compound_sel(Compound { simples: base }),
                fresh,
                origin,
            });
            continue;
        }
        if extenders.iter().all(|sel| sel.single_compound().is_some()) {
            let mut merged = Compound { simples: base };
            if extenders
                .iter()
                .filter_map(Sel::single_compound)
                .all(|compound| merge_compound(&mut merged, compound))
            {
                let mut sel = single_compound_sel(merged);
                sel.line_break = line_break;
                out.push(Produced { sel, fresh, origin });
            }
            continue;
        }
        // 复杂扩展者（`.a .b { @extend .c }`）：单段扩展者先并进末段，再织入。
        let mut simple = Compound { simples: base };
        if !extenders
            .iter()
            .filter_map(Sel::single_compound)
            .all(|compound| merge_compound(&mut simple, compound))
        {
            continue;
        }
        for sel in unify_extenders(&simple, &extenders) {
            out.push(Produced {
                sel,
                fresh: true,
                origin: None,
            });
        }
    }
    Some(out)
}

/// 简单选择器在复合选择器里的位置类别（dart 的 `SimpleSelector.unify` 靠它定位）。
#[derive(Clone, Copy, PartialEq, Eq)]
enum SimpleKind {
    /// 类型 / 通配选择器：永远在最前。
    Type,
    /// 伪元素（`::after`，以及 `:before` 这类单冒号别名）：永远在最后。
    PseudoElement,
    /// 伪类（`:hover`、`:not(...)`）。
    PseudoClass,
    /// 类 / ID / 属性 / 占位符。
    Other,
}

impl SimpleKind {
    fn of(simple: &str) -> Self {
        if simple.starts_with("::") {
            return Self::PseudoElement;
        }
        if let Some(rest) = simple.strip_prefix(':') {
            let name = rest.split('(').next().unwrap_or(rest).to_ascii_lowercase();
            // dart 的 `_isFakePseudoElement`：单冒号写法在语义上仍是伪元素。
            return if matches!(
                name.as_str(),
                "before" | "after" | "first-line" | "first-letter"
            ) {
                Self::PseudoElement
            } else {
                Self::PseudoClass
            };
        }
        if simple
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '*' || c == '|')
        {
            Self::Type
        } else {
            Self::Other
        }
    }
}

/// 把一个简单选择器并进复合选择器；`false` 表示两者无法共存（dart 会丢弃该产物）。
///
/// 位置规则来自 dart 的 `SimpleSelector.unify` / `PseudoSelector.unify`：
/// 类型选择器在最前，类/ID/属性插在第一个伪类之前，伪类插在第一个伪元素之前，
/// 伪元素永远在最后；复合选择器只能有一个伪元素，冲突即无法合并。
fn unify_simple(merged: &mut Vec<String>, simple: &str) -> bool {
    match SimpleKind::of(simple) {
        // 通配选择器不增加约束：dart 在有其它简单选择器时直接丢掉它。
        SimpleKind::Type if simple == "*" => {
            if merged.is_empty() {
                merged.push(simple.to_string());
            }
            true
        }
        SimpleKind::Type => match merged.first() {
            Some(first) if SimpleKind::of(first) == SimpleKind::Type => first == simple,
            _ => {
                merged.insert(0, simple.to_string());
                true
            }
        },
        kind => {
            if merged.iter().any(|s| s == simple) {
                return true;
            }
            let first_pseudo = |want: SimpleKind, merged: &[String]| {
                merged.iter().position(|s| SimpleKind::of(s) == want)
            };
            let at = match kind {
                SimpleKind::PseudoElement => {
                    if merged
                        .iter()
                        .any(|s| SimpleKind::of(s) == SimpleKind::PseudoElement)
                    {
                        return false;
                    }
                    merged.len()
                }
                SimpleKind::PseudoClass => {
                    first_pseudo(SimpleKind::PseudoElement, merged).unwrap_or(merged.len())
                }
                _ => merged
                    .iter()
                    .position(|s| {
                        matches!(
                            SimpleKind::of(s),
                            SimpleKind::PseudoClass | SimpleKind::PseudoElement
                        )
                    })
                    .unwrap_or(merged.len()),
            };
            merged.insert(at, simple.to_string());
            true
        }
    }
}

/// 把 `extender` 复合选择器并进 `merged`（dart 的 `unifyCompound(merged, extender)`）。
///
/// `merged` 是被扩展选择器去掉目标后剩下的简单选择器，`extender` 是扩展者：
/// `.column:hover` 被 `.z` 扩展 → `.z:hover`（`.column.a:hover` + `.z` → `.a.z:hover`）。
/// 扩展者自己写下的伪元素/伪类相对顺序不会被重排（`::after:hover` 仍是 `::after:hover`）。
fn merge_compound(merged: &mut Compound, extender: &Compound) -> bool {
    // dart 的 `unifyComplex` 只有一个参与选择器时原样返回：被扩展的复合选择器整个
    // 消失（`%p`）时，扩展者保持作者写的顺序，不做任何规范化。
    if merged.simples.is_empty() {
        merged.simples.clone_from(&extender.simples);
        return true;
    }
    let mut pseudo_element_found = false;
    // 扩展者伪元素之后的伪类按原顺序追加在整条复合选择器末尾（dart 的 `pseudoResult`）。
    let mut pseudo: Vec<String> = Vec::new();
    for simple in &extender.simples {
        let kind = SimpleKind::of(simple);
        let target = if pseudo_element_found
            && matches!(kind, SimpleKind::PseudoClass | SimpleKind::PseudoElement)
        {
            &mut pseudo
        } else {
            pseudo_element_found |= kind == SimpleKind::PseudoElement;
            &mut merged.simples
        };
        if !unify_simple(target, simple) {
            return false;
        }
    }
    merged.simples.extend(pseudo);
    true
}

fn single_compound_sel(compound: Compound) -> Sel {
    Sel {
        parts: vec![Part {
            combinator: None,
            compound,
        }],
        line_break: false,
    }
}

/// 命中 `simple` 的所有扩展者（按登记顺序）。
fn extenders_for(simple: &str, table: &[(Compound, Extension)]) -> Option<Vec<Sel>> {
    let mut hit = false;
    let mut out = Vec::new();
    for (target, extension) in table {
        if !target.simples.iter().any(|s| s == simple) {
            continue;
        }
        hit = true;
        out.extend(extension.extenders.iter().cloned());
    }
    if hit { Some(out) } else { None }
}

/// 笛卡尔积：每个位置取一个方案，按字典序遍历（dart 的 `paths`）。
fn paths(options: &[Vec<Produced>]) -> Vec<Vec<Produced>> {
    let mut acc: Vec<Vec<Produced>> = vec![Vec::new()];
    for position in options {
        let mut next = Vec::with_capacity(acc.len() * position.len());
        for option in position {
            for path in &acc {
                let mut merged = path.clone();
                merged.push(option.clone());
                next.push(merged);
            }
        }
        acc = next;
    }
    acc
}

/// 扩展位置的可选方案：`None` 表示源码原样的简单选择器。
type Alternative = (Option<Sel>, Sel);

/// 同上，但每个方案带上「来源扩展者」（源码原样的方案是 `None`）。
fn paths_tagged(options: &[Vec<Alternative>]) -> Vec<Vec<Alternative>> {
    let mut acc: Vec<Vec<Alternative>> = vec![Vec::new()];
    for position in options {
        let mut next = Vec::with_capacity(acc.len() * position.len());
        for option in position {
            for path in &acc {
                let mut merged = path.clone();
                merged.push(option.clone());
                next.push(merged);
            }
        }
        acc = next;
    }
    acc
}

/// 把一条路径（若干扩展者）合并成一条复杂选择器。
///
/// 单段扩展者（原选择器与其等价物）合并成末段的复合选择器；多段扩展者按
/// 「公共前缀去重后拼接」织入，与 dart 的 `weave` 在 bootstrap 这类
/// 「同前缀、末段不同」的选择器上等价。
fn unify_extenders(originals: &Compound, extenders: &[Sel]) -> Vec<Sel> {
    let complexes: Vec<&Sel> = extenders
        .iter()
        .filter(|sel| sel.single_compound().is_none())
        .collect();
    let mut out = Vec::new();
    for base in &complexes {
        let mut merged = (*base).clone();
        let last = merged.parts.last_mut().expect("多段");
        // dart 的 `unifyComplex`：先把各扩展者的末段并进原位复合选择器，再织入父级。
        let mut compound = originals.clone();
        if !merge_compound(&mut compound, &last.compound) {
            continue;
        }
        last.compound = compound;
        out.push(merged);
    }
    out
}

/// 织入多条复杂选择器（`weave`）：公共前缀合并去重后依次拼接。
fn weave(path: &[Sel], line_break: bool) -> Vec<Sel> {
    let mut iter = path.iter();
    let Some(first) = iter.next() else {
        return Vec::new();
    };
    let mut prefixes = vec![first.clone()];
    for complex in iter {
        let mut next = Vec::new();
        for prefix in &prefixes {
            // 单段扩展者直接并到前缀末尾（dart 的 `target` 追加）：两个复合选择器
            // 并列即隐式后代组合符，`weave` 不做前缀去重。
            if complex.parts.len() == 1 {
                let mut merged = prefix.clone();
                merged.line_break |= complex.line_break | line_break;
                merged.parts.push(complex.parts[0].clone());
                next.push(merged);
                continue;
            }
            // 末段与下一段首段相同 → 合并（`.navbar` + `.navbar > .x`）。
            if complex.parts.len() > 1 && prefix.parts.last() == complex.parts.first() {
                let mut merged = prefix.clone();
                merged.line_break |= complex.line_break;
                merged.parts.extend(complex.parts[1..].iter().cloned());
                next.push(merged);
                continue;
            }
            // 公共前缀（相同段）逐段吃掉。
            let shared = prefix
                .parts
                .iter()
                .zip(complex.parts.iter())
                .take_while(|(a, b)| a == b)
                .count();
            let mut merged = Sel {
                parts: prefix.parts[..shared].to_vec(),
                line_break: prefix.line_break || complex.line_break || line_break,
            };
            merged.parts.extend(prefix.parts[shared..].iter().cloned());
            merged.parts.extend(complex.parts[shared..].iter().cloned());
            next.push(merged);
            // dart 的 `_weaveParents` 在两侧都还有未消费的父级时还会产出另一种交错：
            // `.D (.A .B)` → `.D .A .B, .A .D .B`。末段是目标，始终留在最后；
            // 带组合符的段不参与交错（与 dart 的 `_mergeTrailingCombinators` 保持一致）。
            let target = complex.parts.len() - 1;
            if shared < target
                && shared < prefix.parts.len()
                && prefix.parts[shared..]
                    .iter()
                    .chain(&complex.parts[shared..target])
                    .all(|part| part.combinator.is_none())
            {
                let mut alternated = Sel {
                    parts: prefix.parts[..shared].to_vec(),
                    line_break: prefix.line_break || complex.line_break || line_break,
                };
                alternated
                    .parts
                    .extend(complex.parts[shared..target].iter().cloned());
                alternated
                    .parts
                    .extend(prefix.parts[shared..].iter().cloned());
                alternated
                    .parts
                    .extend(complex.parts[target..].iter().cloned());
                next.push(alternated);
            }
        }
        prefixes = next;
    }
    for sel in &mut prefixes {
        sel.line_break |= line_break;
    }
    prefixes
}

/// dart 的 `_trim`：
/// - 重复选择器保留先出现的那条；
/// - 扩展产物被**同一来源**的、更宽松的选择器覆盖时丢掉（`.columns` 覆盖
///   `.columns:last-child`、`.b` 覆盖 `.b:hover`）；
/// - 组合符更严格的选择器被更宽松的覆盖（`.navbar > .x` 之于 `.navbar .x`）。
fn trim(produced: Vec<Produced>) -> Vec<Sel> {
    let mut drop_early = vec![false; produced.len()];
    for i in 0..produced.len() {
        let item = &produced[i];
        drop_early[i] = produced[..i].iter().any(|kept| {
            kept.sel == item.sel
                || (item.fresh && kept.origin == item.origin && subsumes(&kept.sel, &item.sel))
        }) || (item.fresh
            && produced.iter().enumerate().any(|(j, other)| {
                j != i && other.origin == item.origin && subsumes(&other.sel, &item.sel)
            }));
    }
    let mut out: Vec<&Produced> = Vec::new();
    for (i, item) in produced.iter().enumerate().rev() {
        if drop_early[i] {
            continue;
        }
        if out.iter().any(|kept| covers(&kept.sel, &item.sel)) {
            continue;
        }
        out.retain(|kept| !covers(&item.sel, &kept.sel) || kept.sel.parts != item.sel.parts);
        out.push(item);
    }
    out.reverse();
    out.into_iter().map(|item| item.sel.clone()).collect()
}

/// `wide` 是否覆盖 `narrow`：复合选择器完全一致，而 `narrow` 的组合符更严格
/// （`>`/`+`/`~` 相对后代）或完全相同。
fn covers(wide: &Sel, narrow: &Sel) -> bool {
    wide.parts.len() == narrow.parts.len()
        && wide.parts.iter().zip(&narrow.parts).all(|(a, b)| {
            a.compound == b.compound && (a.combinator == b.combinator || a.combinator.is_none())
        })
}

/// 把选择器文本解析成 `Sel`（组合符 + 复合选择器）。
pub(crate) fn parse_sel(text: &str, line_break: bool) -> Option<Sel> {
    let bytes = text.as_bytes();
    let mut parts: Vec<Part> = Vec::new();
    let mut i = 0usize;
    let mut combinator: Option<char> = None;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if c.is_ascii_whitespace() {
            let mut j = i;
            while j < bytes.len() && (bytes[j] as char).is_ascii_whitespace() {
                j += 1;
            }
            if j < bytes.len() && matches!(bytes[j] as char, '>' | '+' | '~') {
                i = j;
                continue;
            }
            if j < bytes.len() && combinator.is_none() {
                combinator = Some(' ');
            }
            i = j;
            continue;
        }
        if matches!(c, '>' | '+' | '~') {
            combinator = Some(c);
            i += 1;
            continue;
        }
        let end = compound_end(text, i);
        let simple_texts = split_simples(&text[i..end]);
        if simple_texts.is_empty() {
            return None;
        }
        parts.push(Part {
            combinator: combinator.filter(|c| *c != ' '),
            compound: Compound {
                simples: simple_texts,
            },
        });
        combinator = None;
        i = end;
    }
    if parts.is_empty() {
        None
    } else {
        Some(Sel { parts, line_break })
    }
}

fn compound_end(text: &str, start: usize) -> usize {
    let bytes = text.as_bytes();
    let mut i = start;
    let mut bracket = 0usize;
    let mut paren = 0usize;
    let mut quote: Option<u8> = None;
    while i < bytes.len() {
        let c = bytes[i];
        match quote {
            Some(q) => {
                if c == b'\\' {
                    i += 1;
                } else if c == q {
                    quote = None;
                }
            }
            None => match c {
                b'"' | b'\'' => quote = Some(c),
                b'[' => bracket += 1,
                b']' => bracket = bracket.saturating_sub(1),
                b'(' => paren += 1,
                b')' => paren = paren.saturating_sub(1),
                b'\\' => i += 1,
                _ if bracket == 0 && paren == 0 => {
                    let ch = c as char;
                    if ch.is_ascii_whitespace() || matches!(ch, '>' | '+' | '~') {
                        return i;
                    }
                }
                _ => {}
            },
        }
        i += 1;
    }
    text.len()
}

fn split_simples(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        let start = i;
        match bytes[i] {
            b'[' => {
                let mut depth = 0usize;
                let mut quote: Option<u8> = None;
                while i < bytes.len() {
                    let b = bytes[i];
                    match quote {
                        Some(q) => {
                            if b == b'\\' {
                                i += 1;
                            } else if b == q {
                                quote = None;
                            }
                        }
                        None => match b {
                            b'"' | b'\'' => quote = Some(b),
                            b'[' => depth += 1,
                            b']' => {
                                depth -= 1;
                                if depth == 0 {
                                    i += 1;
                                    break;
                                }
                            }
                            _ => {}
                        },
                    }
                    i += 1;
                }
            }
            b':' => {
                i += 1;
                if bytes.get(i) == Some(&b':') {
                    i += 1;
                }
                i = consume_ident(bytes, i);
                if bytes.get(i) == Some(&b'(') {
                    let mut depth = 0usize;
                    while i < bytes.len() {
                        match bytes[i] {
                            b'(' => depth += 1,
                            b')' => {
                                depth -= 1;
                                if depth == 0 {
                                    i += 1;
                                    break;
                                }
                            }
                            b'"' | b'\'' => {
                                let q = bytes[i];
                                i += 1;
                                while i < bytes.len() && bytes[i] != q {
                                    if bytes[i] == b'\\' {
                                        i += 1;
                                    }
                                    i += 1;
                                }
                            }
                            _ => {}
                        }
                        i += 1;
                    }
                }
            }
            b'.' | b'#' | b'%' => {
                i += 1;
                i = consume_ident(bytes, i);
            }
            _ => {
                let next = consume_ident(bytes, i);
                if next == i {
                    i += 1;
                } else {
                    i = next;
                    if bytes.get(i) == Some(&b'|') {
                        i += 1;
                        if bytes.get(i) == Some(&b'*') {
                            i += 1;
                        } else {
                            i = consume_ident(bytes, i);
                        }
                    }
                }
            }
        }
        out.push(text[start..i].to_string());
    }
    out
}

fn consume_ident(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() {
        let ch = bytes[i] as char;
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
            i += 1;
        } else if ch == '\\' {
            i += 2;
        } else {
            break;
        }
    }
    i
}

/// 把 `@extend` 的目标文本解析成复合选择器（dart 不允许扩展复杂选择器）。
pub(crate) fn target_compound(text: &str) -> Option<Compound> {
    let sel = parse_sel(text, false)?;
    match sel.parts.as_slice() {
        [part] if part.combinator.is_none() => Some(part.compound.clone()),
        _ => None,
    }
}

/// 回写：按编号把最终选择器文本写进规则，并丢掉只剩占位符的规则。
pub(crate) fn apply(items: &mut Vec<Item>, lists: &[Vec<Sel>], dirty: &[bool]) {
    for item in items.iter_mut() {
        match item {
            Item::Rule(rule) => {
                if let Some(id) = rule.extend_id {
                    if let Some(selectors) = lists.get(id) {
                        let has_placeholder = |sels: &[Sel]| {
                            sels.iter()
                                .any(|sel| sel.parts.iter().any(|p| p.compound.has_placeholder()))
                        };
                        // 没被扩展碰过的规则也要滤掉占位符（`%p {}` 不产出 CSS）。
                        if dirty.get(id).copied() == Some(true) || has_placeholder(selectors) {
                            let kept = selectors
                                .iter()
                                .filter(|sel| {
                                    !sel.parts.iter().any(|p| p.compound.has_placeholder())
                                })
                                .collect::<Vec<_>>();
                            // 回写后仍沿用原选择器列表的锚点（dart 的 `CssValue<SelectorList>`
                            // 换值不换 span）。
                            let span = rule
                                .selector
                                .first()
                                .map_or(Span { start: 0, end: 0 }, |(_, span)| *span);
                            rule.selector = selector_lines(&render_list(&kept), span);
                            rule.placeholder = kept.is_empty();
                        }
                    }
                }
            }
            Item::AtRule(at) => {
                if let Some(Body::Rules(nested)) = at.body.as_mut() {
                    apply(nested, lists, dirty);
                }
            }
            Item::Keyframe(_) | Item::Comment(_) | Item::Raw(_) => {}
        }
    }
    // 扩展后可能只剩一条空规则：整条 at-rule 也一起消失（dart 不会输出 `@media x {}`）。
    items.retain(|item| {
        !matches!(item, Item::AtRule(at)
            if matches!(&at.body, Some(Body::Rules(rules)) if rules.is_empty()))
    });
    items.retain(|item| !matches!(item, Item::Rule(rule) if rule.placeholder));
}

fn render_list(selectors: &[&Sel]) -> String {
    let mut out = String::new();
    for sel in selectors {
        if !out.is_empty() {
            out.push(',');
            out.push(if sel.line_break { '\n' } else { ' ' });
        }
        out.push_str(&sel.render());
    }
    out
}
