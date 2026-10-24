//! `@media` prelude 里的 SassScript 算术。
//!
//! dart 的 `MediaQuery.value` 是表达式：`(max-width: $bp - 1px)` 求值成 `768px`。
//! 我们旧实现只做裸 `$var` 文本替换，算术原样漏进产物 —— 媒体特性值不接受算术，
//! 整条查询会失效（响应式断点静默消失）。期望值全部来自 dart-sass 1.93.2。

use std::path::Path;

use undercoat::{Options, compile_file};

fn css(source: &str) -> String {
    compile_file(Path::new("t.scss"), source, &Options::default())
        .expect("compile")
        .css
}

/// 产物里的 `@media` 行（去掉块首）。
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
    // 媒体类型、`and`/`not`、多条查询、范围语法都保持 CSS 结构。
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
    // `var()` 与其它不可求值形态保持原样（含裸 `$var` 的文本替换口径）。
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
