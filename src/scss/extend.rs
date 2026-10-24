//! `@extend` and placeholder selectors (`%p`).
//!
//! Semantics follow dart-sass's `ExtensionStore`: extensions are applied
//! **incrementally** -- each new `@extend` only affects the rules already registered at
//! that point, and a rule absorbs all extensions that exist when it is registered. The
//! output order is a by-product of this process (`.b, .e, .d, .c` rather than
//! `.b, .c, .d, .e`), so we record a stream of "register/extension" events and replay it
//! in order at the end, instead of computing the closure once.
//!
//! The selector model covers only what SCSS needs: combinators + compound selectors
//! (slices of simple selectors). Nested selectors such as `:not(...)` / `:is(...)` are
//! not expanded inside a compound selector.

use oxc_css_parser::pos::Span;

use crate::doc::{Body, Item, selector_lines};

/// One complex selector.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Sel {
    pub(crate) parts: Vec<Part>,
    pub(crate) line_break: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Part {
    /// Leading combinator (`>`/`+`/`~`); `None` means descendant or the first segment.
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

    /// Whether the compound selector contains `%p` (regardless of any other simple
    /// selector).
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
                // Leading combinator (`> .b`): the selector functions allow this form.
                out.push(c);
                out.push(' ');
            }
            out.push_str(&part.compound.render());
        }
        out
    }

    /// The combinator of the last segment is joined with the host combinator of `@extend`,
    /// so it is ignored when looking at "how many segments" there are.
    fn single_compound(&self) -> Option<&Compound> {
        match self.parts.as_slice() {
            [part] if part.combinator.is_none() => Some(&part.compound),
            _ => None,
        }
    }
}

/// One `@extend` registration.
#[derive(Clone)]
pub(crate) struct Extension {
    /// The extender (the selector of the rule containing `@extend`, possibly several).
    pub(crate) extenders: Vec<Sel>,
    /// The extended compound selector (`%p`, `.btn-sm`, `h1`).
    pub(crate) target: Compound,
    pub(crate) optional: bool,
    pub(crate) span: usize,
}

/// Event stream: rule registration / extension registration. The replay order is what
/// implements dart's incremental semantics.
enum Event {
    Rule,
    Extend(Extension),
}

#[derive(Default)]
pub(crate) struct Engine {
    /// Selectors of each rule (the index is the registration order).
    pub(crate) lists: Vec<Vec<Sel>>,
    events: Vec<Event>,
    /// Number of registered rules (used while replaying).
    registered: usize,
    /// Simple selectors that appeared in any registered rule selector. Used by the
    /// `!optional` check: dart's `_selectors` table accumulates over the whole compilation
    /// and the check happens at the end (so `@extend %p` written before `%p` is defined is
    /// legal).
    known: std::collections::HashSet<String>,
    /// Registered extensions (in registration order): new rules / new extenders must
    /// absorb them immediately.
    ext_list: Vec<Extension>,
    /// Which rules had their selectors actually rewritten (untouched ones keep their S2
    /// text, avoiding round-trip rendering differences).
    pub(crate) dirty: Vec<bool>,
}

impl Engine {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Register a rule and return its id.
    pub(crate) fn add_rule(&mut self, mut selectors: Vec<Sel>) -> usize {
        // Absorb the existing extensions once at registration time (the whole table applies
        // together, in registration order).
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

    /// Register a rule selector (even when the rule emits no CSS in the end, such as a
    /// nested rule that only contains `@extend`): both the `!optional` check and the
    /// extension chain must see it.
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
        // The new extender itself may already have been rewritten by existing extensions
        // (`.container-fluid` of `.navbar > .container-fluid` has already been extended by
        // `.container-*`).
        let table = self.table();
        if !table.is_empty() {
            if let Some(extended) = extend_list(&extension.extenders, &table) {
                extension.extenders = extended;
            }
        }
        self.ext_list.push(extension.clone());
        self.events.push(Event::Extend(extension));
    }

    /// The current extension table (target compound selector -> extensions), in
    /// registration order.
    fn table(&self) -> Vec<(Compound, Extension)> {
        self.ext_list
            .iter()
            .map(|extension| (extension.target.clone(), extension.clone()))
            .collect()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// Replay the event stream and write the extension results back into `lists`.
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

    /// Position of the first extension that matched no target and has no `!optional`.
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

/// One extension product together with the text of its "source extender".
///
/// dart's `_trim` only decides overriding between products of the **same source**: when
/// both `.column` and `.column:last-child` are extended by `.columns`,
/// `.columns:last-child` is dropped as covered by `.columns`; `.reveal.tiny` and
/// `.reveal` produced by different `@extend`s are both kept.
#[derive(Clone)]
struct Produced {
    sel: Sel,
    /// Whether this is an extension product (`false` for a selector that is verbatim from
    /// the source).
    fresh: bool,
    /// The source extender of the product (only present for products where "a single
    /// extender" took part).
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

/// Extend a set of selectors with `table` (target compound selector -> extensions);
/// returns `None` when nothing changed.
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

/// Whether `wide` covers `narrow`: the segment count and the combinators are equal or
/// more lenient, and the simple selectors of every segment are a subset of `narrow`'s
/// (`.b` covers `.b:hover`, `li` covers `li.a`); full equality counts as well.
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

/// Extend one complex selector; `None` means no change.
fn extend_sel(sel: &Sel, table: &[(Compound, Extension)]) -> Option<Vec<Produced>> {
    // Alternatives for each position: `Some(vec![...])` means this position is expanded.
    let mut options: Option<Vec<Vec<Produced>>> = None;
    for (i, part) in sel.parts.iter().enumerate() {
        let extended = extend_compound(&part.compound, table);
        match extended {
            None => {
                // A position that matched no extension becomes a segment of its own (dart
                // uses a complex selector containing only that component).
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
                // When the extender itself has no combinator, it inherits the combinator of
                // the extended position (dart's `withAdditionalCombinators`).
                for alternative in &mut alternatives {
                    if alternative.sel.parts[0].combinator.is_none() {
                        alternative.sel.parts[0].combinator = part.combinator;
                    }
                }
                options
                    .as_mut()
                    .expect("just initialized")
                    .push(alternatives);
            }
        }
    }
    let options = options?;
    // Cartesian product -> one weave per path; the first path is "everything as is" and
    // comes first.
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

/// Extend one compound selector; `None` means no extension matched.
fn extend_compound(compound: &Compound, table: &[(Compound, Extension)]) -> Option<Vec<Produced>> {
    // Alternatives for each position: (source extender, selector). The source of a
    // verbatim alternative is `None`.
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
                let position = options.as_mut().expect("just initialized");
                let mut slot = vec![(None, keep)];
                slot.extend(list.into_iter().map(|sel| (Some(sel.clone()), sel)));
                position.push(slot);
            }
        }
    }
    let options = options?;
    let mut out = Vec::new();
    for path in paths_tagged(&options) {
        // Simple selectors kept as is come first in the order of the original compound
        // selector; the one that was extended away is not kept
        // (dart: `.column.a` + `.b` -> `.a.b`).
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
        // A complex extender (`.a .b { @extend .c }`): single-segment extenders are merged
        // into the last segment first, then woven in.
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

/// Position class of a simple selector inside a compound selector (dart's
/// `SimpleSelector.unify` uses it for placement).
#[derive(Clone, Copy, PartialEq, Eq)]
enum SimpleKind {
    /// Type / universal selector: always first.
    Type,
    /// Pseudo-element (`::after`, and single-colon aliases such as `:before`): always last.
    PseudoElement,
    /// Pseudo-class (`:hover`, `:not(...)`).
    PseudoClass,
    /// Class / ID / attribute / placeholder.
    Other,
}

impl SimpleKind {
    fn of(simple: &str) -> Self {
        if simple.starts_with("::") {
            return Self::PseudoElement;
        }
        if let Some(rest) = simple.strip_prefix(':') {
            let name = rest.split('(').next().unwrap_or(rest).to_ascii_lowercase();
            // dart's `_isFakePseudoElement`: the single-colon form is still semantically a
            // pseudo-element.
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

/// Merge one simple selector into a compound selector; `false` means the two cannot
/// coexist (dart drops that product).
///
/// The placement rules come from dart's `SimpleSelector.unify` / `PseudoSelector.unify`:
/// type selectors first, class/ID/attribute inserted before the first pseudo-class,
/// pseudo-classes inserted before the first pseudo-element, and pseudo-elements always
/// last; a compound selector may hold only one pseudo-element, so a conflict cannot be
/// merged.
fn unify_simple(merged: &mut Vec<String>, simple: &str) -> bool {
    match SimpleKind::of(simple) {
        // A universal selector adds no constraint: dart simply drops it when other simple
        // selectors are present.
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

/// Merge the `extender` compound selector into `merged` (dart's
/// `unifyCompound(merged, extender)`).
///
/// `merged` is what remains of the extended selector after removing the target, and
/// `extender` is the extender: `.column:hover` extended by `.z` -> `.z:hover`
/// (`.column.a:hover` + `.z` -> `.a.z:hover`). The relative order of pseudo-elements /
/// pseudo-classes written by the extender itself is not rearranged (`::after:hover`
/// stays `::after:hover`).
fn merge_compound(merged: &mut Compound, extender: &Compound) -> bool {
    // dart's `unifyComplex` returns as is when only one selector takes part: when the
    // extended compound selector disappears entirely (`%p`), the extender keeps the
    // author's order without any normalization.
    if merged.simples.is_empty() {
        merged.simples.clone_from(&extender.simples);
        return true;
    }
    let mut pseudo_element_found = false;
    // Pseudo-classes after the extender's pseudo-element are appended in their original
    // order at the end of the whole compound selector (dart's `pseudoResult`).
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

/// All extenders that match `simple` (in registration order).
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

/// Cartesian product: take one alternative per position and traverse in lexicographic
/// order (dart's `paths`).
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

/// Alternatives of an extension position: `None` means the simple selector as written in
/// the source.
type Alternative = (Option<Sel>, Sel);

/// Same as above, but every alternative carries its "source extender" (`None` for a
/// verbatim alternative).
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

/// Merge one path (a number of extenders) into one complex selector.
///
/// Single-segment extenders (the original selector and its equivalents) are merged into
/// the compound selector of the last segment; multi-segment extenders are woven in by
/// "deduplicating the common prefix and then concatenating", which is equivalent to
/// dart's `weave` for bootstrap-style selectors that share a prefix and differ in the
/// last segment.
fn unify_extenders(originals: &Compound, extenders: &[Sel]) -> Vec<Sel> {
    let complexes: Vec<&Sel> = extenders
        .iter()
        .filter(|sel| sel.single_compound().is_none())
        .collect();
    let mut out = Vec::new();
    for base in &complexes {
        let mut merged = (*base).clone();
        let last = merged.parts.last_mut().expect("multiple segments");
        // dart's `unifyComplex`: first merge the last segment of each extender into the
        // compound selector in place, then weave in the parents.
        let mut compound = originals.clone();
        if !merge_compound(&mut compound, &last.compound) {
            continue;
        }
        last.compound = compound;
        out.push(merged);
    }
    out
}

/// Weave several complex selectors (`weave`): merge and deduplicate the common prefix,
/// then concatenate in order.
fn weave(path: &[Sel], line_break: bool) -> Vec<Sel> {
    let mut iter = path.iter();
    let Some(first) = iter.next() else {
        return Vec::new();
    };
    let mut prefixes = vec![first.clone()];
    for complex in iter {
        let mut next = Vec::new();
        for prefix in &prefixes {
            // A single-segment extender is merged onto the end of the prefix (appending
            // dart's `target`): two juxtaposed compound selectors are an implicit descendant
            // combinator, and `weave` does no prefix deduplication.
            if complex.parts.len() == 1 {
                let mut merged = prefix.clone();
                merged.line_break |= complex.line_break | line_break;
                merged.parts.push(complex.parts[0].clone());
                next.push(merged);
                continue;
            }
            // The last segment equals the first segment of the next part -> merge
            // (`.navbar` + `.navbar > .x`).
            if complex.parts.len() > 1 && prefix.parts.last() == complex.parts.first() {
                let mut merged = prefix.clone();
                merged.line_break |= complex.line_break;
                merged.parts.extend(complex.parts[1..].iter().cloned());
                next.push(merged);
                continue;
            }
            // The common prefix (equal segments) is consumed segment by segment.
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
            // dart's `_weaveParents` also produces another interleaving when both sides
            // still have unconsumed parents: `.D (.A .B)` -> `.D .A .B, .A .D .B`. The last
            // segment is the target and always stays last; segments with a combinator do not
            // take part in the interleaving (consistent with dart's
            // `_mergeTrailingCombinators`).
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

/// dart's `_trim`:
/// - for duplicate selectors, keep the one that appears first;
/// - an extension product is dropped when a more lenient selector from the **same
///   source** covers it (`.columns` covers `.columns:last-child`, `.b` covers `.b:hover`);
/// - a selector with a stricter combinator is covered by a more lenient one
///   (`.navbar > .x` vs `.navbar .x`).
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

/// Whether `wide` covers `narrow`: the compound selectors are identical and `narrow`'s
/// combinator is stricter (`>`/`+`/`~` relative to descendant) or exactly the same.
fn covers(wide: &Sel, narrow: &Sel) -> bool {
    wide.parts.len() == narrow.parts.len()
        && wide.parts.iter().zip(&narrow.parts).all(|(a, b)| {
            a.compound == b.compound && (a.combinator == b.combinator || a.combinator.is_none())
        })
}

/// Parse selector text into `Sel` (combinator + compound selector).
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

/// Parse the target text of `@extend` into a compound selector (dart does not allow
/// extending complex selectors).
pub(crate) fn target_compound(text: &str) -> Option<Compound> {
    let sel = parse_sel(text, false)?;
    match sel.parts.as_slice() {
        [part] if part.combinator.is_none() => Some(part.compound.clone()),
        _ => None,
    }
}

/// Write back: put the final selector text into the rules by id, and drop rules that are
/// left with only placeholders.
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
                        // Rules untouched by extension still filter out placeholders
                        // (`%p {}` emits no CSS).
                        if dirty.get(id).copied() == Some(true) || has_placeholder(selectors) {
                            let kept = selectors
                                .iter()
                                .filter(|sel| {
                                    !sel.parts.iter().any(|p| p.compound.has_placeholder())
                                })
                                .collect::<Vec<_>>();
                            // After writing back, keep the anchor of the original selector list
                            // (dart's `CssValue<SelectorList>` changes the value, not the span).
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
    // Only an empty rule may remain after extension: the whole at-rule disappears with it
    // (dart does not output `@media x {}`).
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
