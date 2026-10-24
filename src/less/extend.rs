//! less `:extend` evaluation.
//!
//! Semantics follow less@4.1.3 `visitors/extend-visitor.js`:
//! - An exact extend requires the whole selector to match element by element (combinators included); only `all` allows slack before and after;
//! - Matching is element-level (`.a.b` is two elements); an `all` hit is replaced by the element range;
//! - Chained propagation (`doExtendChaining`) cuts cycles with `parent_ids`, at most 100 rounds;
//! - A media block forms its own scope: extends inside it apply only there, while outer extends are inherited into it.

use oxc_css_parser::pos::Span;

/// Selector element: combinator + simple selector text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Element {
    /// Combinator in front of this element: `""` / `" "` / `">"` / `"+"` / `"~"` / `"|"`
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

/// Flattened complex selector: `&` joins and nesting are already resolved during combination.
pub(crate) type Sel = Vec<Element>;

/// A single `:extend(target [all])`.
#[derive(Clone, Debug)]
pub(crate) struct Need {
    pub(crate) target: Sel,
    pub(crate) all: bool,
}

fn value_eq(a: &Element, b: &Element) -> bool {
    a.value == b.value || norm(&a.value) == norm(&b.value)
}

/// Normalized form for comparison: whitespace removed; attribute selectors additionally have quotes removed as in less's `Attribute` branch
/// (`[data-x="1"]` equals `[data-x=1]`).
///
/// `:nth-*(An+B)` is the exception: An+B is an expression node in less, so `2n + 1` (addition) and
/// `2n+ 1` (`2n` followed by `+1`) have different structures and cannot be compared after stripping all whitespace.
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

/// The An+B of `:nth-child()` / `:nth-of-type()` / `:nth-last-*()` is a less expression node,
/// so its source text is used for comparison: `2n+1` matches, while `2n + 1` / `2n+ 1` / ` 2n+1 ` do not
/// (measured against less@4.1.3). Stripping all whitespace would misjudge them as equal, so the value is returned as-is.
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

/// Splits complex selector text into elements (isomorphic to less's `Selector.elements`).
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
            // `&-suffix` is "a `&` element + a tag suffix" in less and must not merge into one element
            i += 1;
        } else {
            if matches!(c, '.' | '#' | '*' | '%') {
                i += 1;
            }
            i = skip_ident(bytes, i);
            // A namespace prefix (`svg|rect`, `*|el`) is part of the same element; only `||` is a combinator
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

/// Skips escapes and identifier characters.
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

/// Jumps to the matching closing bracket (skipping quoted content).
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

/// Element sequence → CSS text (combinators follow less `Combinator.genCSS` spacing rules).
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

/// Parent path × child selector: each `&` forms a Cartesian product with the parent paths (less `joinSelector`).
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
                // The join between the parent and child selectors is equivalent to the space added by genCSS
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
                // Without parent selectors `&` is erased; as the first element even the whitespace after it is dropped
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

/// One hit: element range `[start, end)` in the haystack.
struct Hit {
    start: usize,
    end: usize,
}

/// less `findMatch`: slides needle over haystack (`all` allows slack before and after and multiple hits).
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
        // When moving to the next selector in a path, less adds a space combinator
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
            // Hit ranges do not overlap: restart the search from here
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

/// less `extendSelector`: replaces the hit ranges with the replacement selector.
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

/// A media/at-rule scope.
struct Scope {
    parent: Option<usize>,
    /// Extends defined in this scope (in definition order)
    extends: Vec<usize>,
}

/// A single extend declaration.
struct Extend {
    target: Sel,
    all: bool,
    /// The replacing self selector (already flattened)
    self_sel: Sel,
    /// Source span of the selector path containing `self_sel` (selectors produced by extend carry it for mapping)
    self_span: Span,
    /// Whether this is the first extend on that selector path (determines whether chained output lands on that rule)
    first: bool,
    /// Whether this extend is visible (extends in a `(reference)` import body are invisible and produce nothing even on a hit)
    visible: bool,
    /// Ids of this and ancestor extends (cycle detection)
    ancestors: Vec<usize>,
    ruleset: usize,
}

/// A rule that is (or will be) emitted.
struct Rule {
    sels: Vec<Sel>,
    /// Same length as `sels`: source span of each path (for mapping; `0..0` means no source)
    spans: Vec<Span>,
    /// Same length as `sels`: whether the path is visible (paths in a `(reference)` import body are invisible by default)
    visible: Vec<bool>,
    /// Same length as `sels`: the path has its own `:extend(...)` and does not take part in being extended
    skip: Vec<bool>,
    /// The rule has `&:extend(...)`: the whole rule does not take part in being extended
    skip_all: bool,
    scope: usize,
}

/// Evaluator for flattened selectors + extend.
pub(crate) struct Engine {
    scopes: Vec<Scope>,
    extends: Vec<Extend>,
    rules: Vec<Rule>,
    /// Extends in effect per scope (including chained output)
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

    /// Enters a new media/at-rule scope.
    pub(crate) fn push_scope(&mut self, parent: Option<usize>) -> usize {
        self.scopes.push(Scope {
            parent,
            extends: Vec::new(),
        });
        self.scopes.len() - 1
    }

    /// Registers a rule (`spans` has the same length as `sels`) and returns its index.
    ///
    /// `hidden` means the rule comes from a `(reference)` import body: its paths are not emitted by default and
    /// become visible only after an extend hit (less's `findMatch` calls `ensureVisibility` on derived selectors).
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

    /// `&:extend(...)` appeared in the rule: the whole rule is no longer extended.
    pub(crate) fn mark_skip_all(&mut self, ruleset: usize) {
        self.rules[ruleset].skip_all = true;
    }

    /// The path itself has `:extend(...)`: it does not take part in being extended.
    pub(crate) fn mark_skip(&mut self, ruleset: usize, path: usize) {
        if let Some(skip) = self.rules[ruleset].skip.get_mut(path) {
            *skip = true;
        }
    }

    /// The rule's finally visible selectors and their source spans (added paths carry the source of their own path).
    ///
    /// Paths in a `(reference)` import body are invisible unless hit by an extend, so the whole rule is not emitted.
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

    /// Chaining and the main scan (less `ProcessExtendsVisitor.run`).
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
        // Added paths take part in being extended by default (less attaches extendList only to chained output)
        for rule in &mut self.rules {
            rule.skip.resize(rule.sels.len(), false);
            rule.visible.resize(rule.sels.len(), true);
            rule.spans
                .resize(rule.sels.len(), Span { start: 0, end: 0 });
        }
    }

    /// less `doExtendChaining`: closure evaluation when A extends B and B extends C.
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
                // less: the new extend inherits the target's visibility and the new path inherits the source's visibility
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
                // Chained output becomes a new path of that rule (it no longer takes part in being extended)
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
