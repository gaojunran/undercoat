//! Color fractional channels and `calc()` constant folding.
//!
//! Colors: `SassColor`'s RGB channels are f64 throughout and are only rounded during
//! serialization (dart 1.93.2's `rgbInternal` semantics). Chained derivations
//! (`mix` → `darken`) lose no precision.
//! `calc`: purely numeric subexpressions are merged under Sass unit rules, and inner
//! parentheses that cannot be folded are written as nested `calc()` (old WebKit does
//! not fully support bare parentheses inside calc). All numeric expectations come from
//! dart-sass 1.93.2.

use std::path::Path;

use undercoat::{Options, compile_file};

fn css(source: &str) -> String {
    compile_file(Path::new("t.scss"), source, &Options::default())
        .expect("compile")
        .css
}

fn decl(source: &str, name: &str) -> String {
    let out = css(source);
    let prefix = format!("  {name}: ");
    let line = out
        .lines()
        .find(|line| line.starts_with(&prefix))
        .unwrap_or_else(|| panic!("no {name} in the output:\n{out}"));
    line.trim_start_matches(&prefix)
        .trim_end_matches(';')
        .to_string()
}

#[test]
fn color_chain_keeps_fractional_channels() {
    // dart 1.93.2: mix → rgb(253.04, 253.32, 253.6), darken 5% → rgb(238.165, 240.57, 242.975).
    // Rounded that is #fdfdfe / #eef1f3; rounding first and then going through HSL
    // would yield #ececf6 (off by 3/255).
    let source = "$light: #f8f9fa;
a { mixed: mix(#fff, $light, 72%); shaded: darken(mix(#fff, $light, 72%), 5%); }";
    assert_eq!(decl(source, "mixed"), "#fdfdfe");
    assert_eq!(decl(source, "shaded"), "#eef1f3");
}

#[test]
fn color_functions_return_legacy_rounded_values() {
    // `red()` / `ie-hex-str()` still read the rounded channels (same convention as
    // dart 1.93.2).
    let source = "$c: rgb(253.04, 253.32, 253.6);
a { r: red($c); hex: ie-hex-str($c); inv: invert($c); }";
    assert_eq!(decl(source, "r"), "253");
    assert_eq!(decl(source, "hex"), "#FFFDFDFE");
    assert_eq!(decl(source, "inv"), "#020201");
}

#[test]
fn color_equality_uses_fractional_channels() {
    // `hsl(0, 0%, 50%)` = `rgb(127.5, 127.5, 127.5)` (rounding first would make the
    // comparison a bogus false/true).
    let source = "a { same: hsl(0, 0%, 50%) == rgb(127.5, 127.5, 127.5); }";
    assert_eq!(decl(source, "same"), "true");
}

#[test]
fn calc_folds_numeric_subexpressions() {
    let source = "a {
      sum: calc(1px + 2px);
      scaled: calc(2 * 1px);
      div: calc(100% / 3 * 2);
      mixed-units: calc(1in + 1px);
      nested: calc(50% - (1em * 0.5));
      negative: calc(100% + (-4px));
      parens: calc((1px + 2px) * 2);
    }";
    assert_eq!(decl(source, "sum"), "3px");
    assert_eq!(decl(source, "scaled"), "2px");
    // dart's 10-decimal rule.
    assert_eq!(decl(source, "div"), "66.6666666667%");
    // Convertible units are converted to the left-hand unit (1in = 96px).
    assert_eq!(decl(source, "mixed-units"), "1.0104166667in");
    assert_eq!(decl(source, "nested"), "calc(50% - 0.5em)");
    assert_eq!(decl(source, "negative"), "calc(100% - 4px)");
    assert_eq!(decl(source, "parens"), "6px");
}

#[test]
fn calc_keeps_unfoldable_expressions() {
    let source = "a {
      incompatible: calc(1px + 2em);
      var-mul: calc(var(--x) * 2 + 1px);
      whole: calc(var(--x));
    }";
    assert_eq!(decl(source, "incompatible"), "calc(1px + 2em)");
    assert_eq!(decl(source, "var-mul"), "calc(var(--x) * 2 + 1px)");
    assert_eq!(decl(source, "whole"), "calc(var(--x))");
}

#[test]
fn calc_wraps_required_parentheses_as_nested_calc() {
    // Bare parentheses are not fully supported in old WebKit's calc: inner parentheses
    // are rewritten as nested `calc()`.
    let source = "a {
      grouped: calc((var(--x) + 1px) * 2);
      halved: calc((0px - var(--x)) / 2);
      lone: calc((var(--x)));
    }";
    assert_eq!(decl(source, "grouped"), "calc(calc(var(--x) + 1px) * 2)");
    assert_eq!(decl(source, "halved"), "calc(calc(0px - var(--x)) / 2)");
    // A single opaque value keeps its parentheses (dart keeps them too).
    assert_eq!(decl(source, "lone"), "calc((var(--x)))");
}

#[test]
fn math_functions_fold_their_arguments() {
    // min/max/clamp arguments are folded first as well: numeric arguments are evaluated
    // directly (dart's `_minMax` / `_clamp`).
    let source = "a {
      upper: min(calc(100% / 3), 10px);
      clamped: clamp(calc(1px + 2px), 1px, 9px);
      ordered: clamp(10px, 5px, 1px);
      mixed: min(calc(1em + 1px), 4px);
      lower: max(calc(1px * 2), 1px);
    }";
    assert_eq!(decl(source, "upper"), "min(33.3333333333%, 10px)");
    assert_eq!(decl(source, "clamped"), "3px");
    assert_eq!(decl(source, "ordered"), "10px");
    assert_eq!(decl(source, "mixed"), "min(1em + 1px, 4px)");
    assert_eq!(decl(source, "lower"), "2px");
}

#[test]
fn math_functions_evaluate_sass_calls_in_their_arguments() {
    // A known Sass function is not a CSS-only construct: it evaluates to a number and
    // then takes part in CSS math as usual (dart's default behavior).
    let source = "$c: #4a4a4a;
a {
      widened: lighten($c, max((100% - lightness($c)) - 2%, 0%));
      maxed: max((100% - lightness($c)) - 2%, 0%);
      plain: max(lightness($c), 0%);
      folded: min(percentage(1/3), 50%);
      rounded: min(round(10.6px), 20px);
      kept: min(lightness($c), var(--w));
      empty: min(lightness($c), 20px);
    }";
    // Adding 68.98% to 29.02% gives 98%, which rounds to 250.
    assert_eq!(decl(source, "widened"), "#fafafa");
    assert_eq!(decl(source, "maxed"), "68.9803921569%");
    assert_eq!(decl(source, "plain"), "29.0196078431%");
    assert_eq!(decl(source, "folded"), "33.3333333333%");
    assert_eq!(decl(source, "rounded"), "11px");
    // With `var()` present the whole value is still passed through as plain CSS (but the
    // known functions inside have already been evaluated and inlined).
    assert_eq!(decl(source, "kept"), "min(29.0196078431%, var(--w))");
    // % and px are not comparable: the numbers are evaluated, and the whole value is
    // passed through as CSS.
    assert_eq!(decl(source, "empty"), "min(29.0196078431%, 20px)");
}

#[test]
fn calc_interpolation_is_opaque_but_neighbours_still_fold() {
    // An interpolation result takes no part in evaluation (dart's
    // `CalculationInterpolation`); the numbers around it still fold as usual.
    let source = "$w: 1px;
$c: 2px;
a {
  interp: calc(var(--h) - #{$w} * 2);
  opaque-only: calc(#{$c} + 1px);
  value: calc(var(--h) - $w * 2);
}";
    assert_eq!(decl(source, "interp"), "calc(var(--h) - 1px * 2)");
    assert_eq!(decl(source, "opaque-only"), "calc(2px + 1px)");
    assert_eq!(decl(source, "value"), "calc(var(--h) - 2px)");
}
