//! `@extend` 的选择器合并：复合选择器是「简单选择器序列」，合并只在对应位置插入，
//! 伪元素永远保持在最后（dart-sass 1.93.2 的 `unifyCompound` / `weave`）。
//!
//! 期望产物逐字节取自 dart-sass 1.93.2 expanded 输出。

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

/// 缺陷复现：伪元素后面不能再跟伪类，扩展出来的选择器必须仍是合法 CSS。
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

/// 作者写在伪元素后面的伪类不重排（`::before:hover` 保持原样）。
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

/// 多个伪类链、属性选择器、类型选择器的相对位置都不变。
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

/// `A extends B, B extends C` 的传递链顺序仍正确。
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

/// 被扩展选择器本身带伪元素时，扩展者的简单选择器插在伪元素之前。
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

/// 多段扩展者织入时同时产出「父级在前」的另一种交错（dart 的 `weave`）。
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

/// 两个不同的伪元素无法合并成一个复合选择器，dart 丢弃扩展产物。
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

/// `@import` 的 `url(...)` 参数按 dart 的写法统一双引号（字符串含 `"` 时保留单引号）。
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
