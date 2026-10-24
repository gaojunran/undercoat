//! SassScript arithmetic inside `@media` preludes.
//!
//! In dart, `MediaQuery.value` is an expression: `(max-width: $bp - 1px)` evaluates to
//! `768px`. Our old implementation only did textual substitution of a bare `$var`, so the
//! arithmetic leaked into the output verbatim — media feature values do not accept
//! arithmetic, so the whole query was dropped (responsive breakpoints silently vanished).
//! All expectations come from dart-sass 1.93.2.

use std::path::Path;

use undercoat::{Options, compile_file};

fn css(source: &str) -> String {
    compile_file(Path::new("t.scss"), source, &Options::default())
        .expect("compile")
        .css
}

/// The `@media` lines of the output (with the block opener stripped).
fn media(prelude: &str) -> String {
    css(prelude)
        .lines()
        .filter(|line| line.starts_with("@media"))
        .map(|line| line.trim_end_matches(" {"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn media_values_evaluate_sass_arithmetic() {
    let source = "$bp: 769px;
@media (max-width: $bp - 1px) { .a { x: 1 } }
@media (min-width: $bp + 1px) { .b { y: 2 } }
@media (max-width: $bp / 2) { .c { z: 3 } }
@media (min-width: $bp * 2 - 1px) { .d { w: 4 } }";
    assert_eq!(
        media(source),
        "@media (max-width: 768px)\n\
         @media (min-width: 770px)\n\
         @media (max-width: 384.5px)\n\
         @media (min-width: 1537px)"
    );
}

#[test]
fn media_queries_keep_their_css_structure() {
    // Media types, `and`/`not`, multiple queries and range syntax all keep their CSS
    // structure.
    let source = "$bp: 769px;
@media screen and (max-width: $bp - 1px) and (min-width: $bp * 0 + 400px) { .a { x: 1 } }
@media not print and (min-width: $bp) { .b { y: 2 } }
@media (max-width: 700px), print and (min-width: $bp + 1px) { .c { z: 3 } }
@media (width >= $bp) { .d { w: 4 } }";
    assert_eq!(
        media(source),
        "@media screen and (max-width: 768px) and (min-width: 400px)\n\
         @media not print and (min-width: 769px)\n\
         @media (max-width: 700px), print and (min-width: 770px)\n\
         @media (width >= 769px)"
    );
}

#[test]
fn media_values_evaluate_sass_functions() {
    let source = "@use \"sass:map\";
@use \"sass:math\";
$gutter: 24px;
$m: (k: 700px);
@media (max-width: map.get($m, k)) { .a { x: 1 } }
@media (max-width: $gutter / 2) { .b { y: 2 } }
@media (min-width: math.div($gutter, 3) * 2) { .c { z: 3 } }";
    assert_eq!(
        media(source),
        "@media (max-width: 700px)\n\
         @media (max-width: 12px)\n\
         @media (min-width: 16px)"
    );
}

#[test]
fn media_values_that_cannot_be_evaluated_stay_as_written() {
    // `var()` and other non-evaluable forms stay as written (including the textual
    // substitution of a bare `$var`).
    let source = "$bp: 769px;
@media (max-width: var(--x)) { .a { x: 1 } }
@media (min-width: $bp) { .b { y: 2 } }
@media (max-width: calc(#{$bp} + 1px)) { .c { z: 3 } }";
    assert_eq!(
        media(source),
        "@media (max-width: var(--x))\n\
         @media (min-width: 769px)\n\
         @media (max-width: calc(769px + 1px))"
    );
}
