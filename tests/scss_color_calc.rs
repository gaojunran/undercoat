//! 颜色分数通道与 `calc()` 常数化简。
//!
//! 颜色：`SassColor` 的 RGB 通道全程是 f64，只在序列化时取整（dart 1.93.2 的
//! `rgbInternal` 语义）。链式推导（`mix` → `darken`）不丢精度。
//! `calc`：纯数字子式按 Sass 单位规则合并，不可化简的内层括号写成嵌套 `calc()`
//! （老 WebKit 对 calc 内裸括号支持不完整）。用例数值全部来自 dart-sass 1.93.2。

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
        .unwrap_or_else(|| panic!("没有 {name}：\n{out}"));
    line.trim_start_matches(&prefix)
        .trim_end_matches(';')
        .to_string()
}

#[test]
fn color_chain_keeps_fractional_channels() {
    // dart 1.93.2：mix → rgb(253.04, 253.32, 253.6)、darken 5% → rgb(238.165, 240.57, 242.975)。
    // 取整后是 #fdfdfe / #eef1f3；先取整再进 HSL 会得到 #ececf6（差 3/255）。
    let source = "$light: #f8f9fa;
a { mixed: mix(#fff, $light, 72%); shaded: darken(mix(#fff, $light, 72%), 5%); }";
    assert_eq!(decl(source, "mixed"), "#fdfdfe");
    assert_eq!(decl(source, "shaded"), "#eef1f3");
}

#[test]
fn color_functions_return_legacy_rounded_values() {
    // `red()` / `ie-hex-str()` 仍按取整后的通道取值（dart 1.93.2 同一口径）。
    let source = "$c: rgb(253.04, 253.32, 253.6);
a { r: red($c); hex: ie-hex-str($c); inv: invert($c); }";
    assert_eq!(decl(source, "r"), "253");
    assert_eq!(decl(source, "hex"), "#FFFDFDFE");
    assert_eq!(decl(source, "inv"), "#020201");
}

#[test]
fn color_equality_uses_fractional_channels() {
    // `hsl(0, 0%, 50%)` = `rgb(127.5, 127.5, 127.5)`（取整后就成了假的 false/true）。
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
    // dart 的 10 位小数规则。
    assert_eq!(decl(source, "div"), "66.6666666667%");
    // 可换算单位按左侧单位换算（1in = 96px）。
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
    // 裸括号在老 WebKit 的 calc 里支持不完整：内层括号改写成嵌套 `calc()`。
    let source = "a {
      grouped: calc((var(--x) + 1px) * 2);
      halved: calc((0px - var(--x)) / 2);
      lone: calc((var(--x)));
    }";
    assert_eq!(decl(source, "grouped"), "calc(calc(var(--x) + 1px) * 2)");
    assert_eq!(decl(source, "halved"), "calc(calc(0px - var(--x)) / 2)");
    // 单独一个不透明值保留原括号（dart 同样保留）。
    assert_eq!(decl(source, "lone"), "calc((var(--x)))");
}

#[test]
fn math_functions_fold_their_arguments() {
    // min/max/clamp 的实参同样先化简：数字实参直接求值（dart 的 `_minMax` / `_clamp`）。
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
    // 已知 Sass 函数不是 CSS 专有构造：求值成数字后照常参与 CSS 数学（dart 默认口径）。
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
    // 68.98% 加到 29.02% 上 = 98%，取整后是 250。
    assert_eq!(decl(source, "widened"), "#fafafa");
    assert_eq!(decl(source, "maxed"), "68.9803921569%");
    assert_eq!(decl(source, "plain"), "29.0196078431%");
    assert_eq!(decl(source, "folded"), "33.3333333333%");
    assert_eq!(decl(source, "rounded"), "11px");
    // 含 `var()` 时整条仍按纯 CSS 透传（但已知函数已经求值内联）。
    assert_eq!(decl(source, "kept"), "min(29.0196078431%, var(--w))");
    // % 与 px 不可比：数值已求值，整条按 CSS 透传。
    assert_eq!(decl(source, "empty"), "min(29.0196078431%, 20px)");
}

#[test]
fn calc_interpolation_is_opaque_but_neighbours_still_fold() {
    // 插值结果不参与求值（dart 的 `CalculationInterpolation`），旁边的数字照常合并。
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
