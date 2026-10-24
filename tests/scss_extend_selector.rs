//! Selector merging for `@extend`: a compound selector is a "sequence of simple
//! selectors", merging only inserts at the corresponding position, and a pseudo-element
//! always stays last (dart-sass 1.93.2's `unifyCompound` / `weave`).
//!
//! Expected output is taken byte-for-byte from dart-sass 1.93.2 expanded output.

use undercoat::{Options, compile_file};

fn compile(name: &str, source: &str) -> String {
    let dir = std::env::temp_dir().join(format!("minibuild-scss-extend-{name}"));
    std::fs::create_dir_all(&dir).expect("mkdir");
    let path = dir.join("main.scss");
    std::fs::write(&path, source).expect("write");
    compile_file(&path, source, &Options::default())
        .unwrap_or_else(|e| panic!("{name}: {e:#}"))
        .css
}

/// Bug repro: a pseudo-element cannot be followed by a pseudo-class, so the extended
/// selectors must still be valid CSS.
#[test]
fn pseudo_element_stays_last_when_extending() {
    assert_eq!(
        compile(
            "pseudo-element-last",
            "%p { color: red; }\n\
             .a { &:not(.b)::after { @extend %p; } }\n\
             .c { &::after { @extend %p; } }\n\
             .d:not(.e) { &::before { @extend %p; } }\n",
        ),
        ".d:not(.e)::before, .c::after, .a:not(.b)::after {\n  color: red;\n}",
    );
}

/// Pseudo-classes that authors write after a pseudo-element are not reordered
/// (`::before:hover` stays as written).
#[test]
fn author_written_pseudo_element_order_is_kept() {
    assert_eq!(
        compile(
            "author-order",
            "%p { color: red; }\n\
             .a::before:hover { @extend %p; }\n\
             .b:hover { @extend %p; }\n\
             .c::after:hover { @extend %p; }\n",
        ),
        ".c::after:hover, .b:hover, .a::before:hover {\n  color: red;\n}",
    );
}

/// Relative positions of multiple pseudo-class chains, attribute selectors and type
/// selectors are all unchanged.
#[test]
fn pseudo_class_chain_and_attribute_selector_keep_their_places() {
    assert_eq!(
        compile(
            "chain-attr",
            "%q { color: blue; }\n\
             .a:hover:focus::before { @extend %q; }\n\
             input[type=text]::placeholder { @extend %q; }\n\
             .f:hover { @extend %q; }\n",
        ),
        ".f:hover, input[type=text]::placeholder, .a:hover:focus::before {\n  color: blue;\n}",
    );
}

/// The ordering of the transitive chain `A extends B, B extends C` is still correct.
#[test]
fn transitive_extends_preserve_pseudo_element_order() {
    assert_eq!(
        compile(
            "transitive",
            "%c { color: red; }\n\
             %b { @extend %c; }\n\
             %a { @extend %b; }\n\
             .x::after:hover { @extend %a; }\n",
        ),
        ".x::after:hover {\n  color: red;\n}",
    );
}

/// When the extended selector itself carries a pseudo-element, the extender's simple
/// selectors are inserted before it.
#[test]
fn extending_a_compound_with_pseudo_element_inserts_before_it() {
    assert_eq!(
        compile(
            "extend-into-element",
            ".a:hover { color: red; }\n\
             .b:not(.c)::after { @extend .a; }\n\
             .d::after { @extend .a; }\n",
        ),
        ".a:hover, .d:hover::after, .b:hover:not(.c)::after {\n  color: red;\n}",
    );
}

/// Weaving multi-part extenders also produces the other interleaving, with parents
/// first (dart's `weave`).
#[test]
fn weave_emits_extender_parents_before_the_extended_selector() {
    assert_eq!(
        compile(
            "weave-parents",
            "%p { fieldset[disabled] & { color: red; } }\n\
             .select select { @extend %p; }\n",
        ),
        "fieldset[disabled] .select select, .select fieldset[disabled] select {\n  color: red;\n}",
    );
}

/// Two different pseudo-elements cannot be merged into one compound selector, so dart
/// drops the extended result.
#[test]
fn conflicting_pseudo_elements_drop_the_extended_selector() {
    assert_eq!(
        compile(
            "element-conflict",
            ".a::before { color: red; }\n.b::after { @extend .a; }\n",
        ),
        ".a::before {\n  color: red;\n}",
    );
}

/// `url(...)` arguments of `@import` are uniformly double-quoted as dart does (single
/// quotes are kept when the string contains `"`).
#[test]
fn import_url_strings_are_double_quoted() {
    assert_eq!(
        compile(
            "import-url",
            "@import url('x.css') screen and (min-width: 100px);\n",
        ),
        "@import url(\"x.css\") screen and (min-width: 100px);",
    );
    assert_eq!(
        compile("import-url-inner", "@import url('a\"b.css');\n"),
        "@import url('a\"b.css');",
    );
}
