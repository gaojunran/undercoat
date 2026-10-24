//! `@forward` / `@use ... with` 的语义边界（dart-sass 1.93.2 实测）。
//!
//! 正向产物在 `tests/fixtures-scss-s32`；这里覆盖可见性、报错文案与位置。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use undercoat::{CompileError, Options, compile_file};

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn project(files: &[(&str, &str)]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "minibuild-scss-forward-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).expect("mkdir");
    for (name, text) in files {
        std::fs::write(dir.join(name), text).expect("write fixture");
    }
    dir.join("main.scss")
}

fn compile(path: &Path) -> Result<undercoat::Output, CompileError> {
    let source = std::fs::read_to_string(path).expect("read scss");
    compile_file(path, &source, &Options::default())
}

fn compile_err(path: &Path) -> CompileError {
    match compile(path) {
        Ok(out) => panic!("{} 应当报错，实际产出:\n{}", path.display(), out.css),
        Err(e) => e,
    }
}

/// `@forward` 的成员不在本模块作用域内可见（与 `@use` 的关键区别）。
#[test]
fn forwarded_members_are_not_visible_in_the_forwarding_module() {
    let path = project(&[
        ("_lib.scss", "$v: 1;\n"),
        ("_relay.scss", "@forward 'lib';\n.r { v: $v }\n"),
        ("main.scss", "@use 'relay';\n"),
    ]);
    let err = compile_err(&path);
    assert_eq!(err.message, "Undefined variable.");
    assert_eq!((err.line, err.column), (Some(2), Some(8)));
}

/// `@use ... as *` 也不让成员变成对外导出（S9 的隔离，回归）。
#[test]
fn star_import_does_not_leak_into_forwards() {
    let path = project(&[
        ("_lib.scss", "$v: 1;\n"),
        ("_relay.scss", "@use 'lib' as *;\n"),
        ("main.scss", "@use 'relay' as *;\n.z { v: $v }\n"),
    ]);
    assert_eq!(compile_err(&path).message, "Undefined variable.");
}

/// 配置非 `!default` 变量：dart 的文案与位置。
#[test]
fn configuring_a_non_default_variable_is_an_error() {
    let path = project(&[
        ("_lib.scss", "$v: 1;\n$w: 2 !default;\n"),
        ("main.scss", "@use 'lib' with ($v: 3);\n"),
    ]);
    let err = compile_err(&path);
    assert_eq!(
        err.message,
        "This variable was not declared with !default in the @used module."
    );
    // 指向 `$v` 的起点（`@use "lib" with (` 之后第 0 个字符）。
    assert_eq!((err.line, err.column), (Some(1), Some(17)));
}

/// `with` 里的未知变量同样报「不是 !default」。
#[test]
fn configuring_an_unknown_variable_is_an_error() {
    let path = project(&[
        ("_lib.scss", "$v: 1 !default;\n"),
        ("main.scss", "@use 'lib' with ($nope: 3);\n"),
    ]);
    assert_eq!(
        compile_err(&path).message,
        "This variable was not declared with !default in the @used module."
    );
}

/// 同一模块二次配置报错。
#[test]
fn configuring_a_module_twice_is_an_error() {
    let path = project(&[
        ("_lib.scss", "$v: 1 !default;\n"),
        (
            "main.scss",
            "@use 'lib' with ($v: 1);\n@use 'lib' as b with ($v: 2);\n",
        ),
    ]);
    let err = compile_err(&path);
    assert_eq!(
        err.message,
        "This module was already loaded, so it can't be configured using \"with\"."
    );
    assert_eq!(err.line, Some(2));
}

/// 先普通 `@use` 再配置：同样报「已装载」。
#[test]
fn configuring_an_already_loaded_module_is_an_error() {
    let path = project(&[
        ("_lib.scss", "$v: 1 !default;\n"),
        ("main.scss", "@use 'lib';\n@use 'lib' as b with ($v: 2);\n"),
    ]);
    assert_eq!(
        compile_err(&path).message,
        "This module was already loaded, so it can't be configured using \"with\"."
    );
}

/// 两个不同的模块被转发到同一作用域且重名：dart 报在第二个 `@forward`。
#[test]
fn two_forwards_defining_the_same_variable_conflict() {
    let path = project(&[
        ("_a.scss", "$x: 1;\n"),
        ("_b.scss", "$x: 2;\n"),
        ("_relay.scss", "@forward 'a';\n@forward 'b';\n"),
        ("main.scss", "@use 'relay' as r;\n"),
    ]);
    let err = compile_err(&path);
    assert_eq!(
        err.message,
        "Two forwarded modules both define a variable named $x."
    );
    assert_eq!(err.line, Some(2));
}

/// 同名函数/mixin 也冲突（dart 把两者并成一个命名空间，文案说 function）。
#[test]
fn two_forwards_defining_the_same_function_conflict() {
    let path = project(&[
        ("_a.scss", "@function ff() { @return 1; }\n"),
        ("_b.scss", "@function ff() { @return 2; }\n"),
        ("_relay.scss", "@forward 'a';\n@forward 'b';\n"),
        ("main.scss", "@use 'relay' as r;\n"),
    ]);
    assert_eq!(
        compile_err(&path).message,
        "Two forwarded modules both define a function named ff."
    );
}

/// 菱形转发（同一模块经两条路径到达）不算冲突。
#[test]
fn diamond_forwards_do_not_conflict() {
    let path = project(&[
        ("_a.scss", "$x: 1;\n"),
        ("_c.scss", "@forward 'a';\n"),
        ("_d.scss", "@forward 'a';\n"),
        ("_relay.scss", "@forward 'c';\n@forward 'd';\n"),
        ("main.scss", "@use 'relay' as r;\n.z { v: r.$x }\n"),
    ]);
    let out = compile(&path).expect("compile");
    assert_eq!(out.css, ".z {\n  v: 1;\n}");
}

/// 模块自己的同名成员优先于转发来的（dart 不报冲突）。
#[test]
fn own_members_shadow_forwarded_ones_without_conflict() {
    let path = project(&[
        ("_a.scss", "$x: 1;\n"),
        ("_relay.scss", "@forward 'a';\n$x: 9;\n"),
        ("main.scss", "@use 'relay' as r;\n.z { v: r.$x }\n"),
    ]);
    let out = compile(&path).expect("compile");
    assert_eq!(out.css, ".z {\n  v: 9;\n}");
}

/// 私有成员（`-` / `_` 开头）不转发，也不允许从模块外访问。
#[test]
fn private_members_are_not_forwarded() {
    let path = project(&[
        ("_lib.scss", "$-p: 1;\n"),
        ("_relay.scss", "@forward 'lib';\n"),
        ("main.scss", "@use 'relay' as r;\n.z { v: r.$-p }\n"),
    ]);
    let err = compile_err(&path);
    assert_eq!(
        err.message,
        "Private members can't be accessed from outside their modules."
    );
}

/// `@import` 一个只写了 `@forward` 的文件：转发成员在导入处可见（dart 实测）。
#[test]
fn import_of_a_forwarder_exposes_members_locally() {
    let path = project(&[
        (
            "_lib.scss",
            "$v: 1;\n@mixin mm { m: 2 }\n@function ff() { @return 3; }\n",
        ),
        ("_fwd.scss", "@forward 'lib';\n"),
        (
            "main.scss",
            "@import 'fwd';\n.z { v: $v; @include mm; f: ff() }\n",
        ),
    ]);
    let out = compile(&path).expect("compile");
    assert_eq!(out.css, ".z {\n  v: 1;\n  m: 2;\n  f: 3;\n}");
}

/// `show` / `hide` 的名单写的是**加前缀之后**的名字（dart 实测）。
#[test]
fn forward_filter_sees_prefixed_names() {
    let path = project(&[
        ("_lib.scss", "$x: 1;\n"),
        ("_fwd.scss", "@forward 'lib' as p-* show $p-x;\n"),
        ("main.scss", "@use 'fwd' as f;\n.z { v: f.$p-x }\n"),
    ]);
    assert_eq!(compile(&path).expect("compile").css, ".z {\n  v: 1;\n}");

    // 写原名（未加前缀）则什么也匹配不到。
    let path = project(&[
        ("_lib.scss", "$x: 1;\n"),
        ("_fwd.scss", "@forward 'lib' as p-* show $x;\n"),
        ("main.scss", "@use 'fwd' as f;\n.z { v: f.$p-x }\n"),
    ]);
    assert_eq!(compile_err(&path).message, "Undefined variable.");
}

/// 前缀只拼在转发名字上，私有成员照样不转发。
#[test]
fn prefix_does_not_revive_private_members() {
    let path = project(&[
        ("_lib.scss", "$-p: 1;\n"),
        ("_fwd.scss", "@forward 'lib' as p-*;\n"),
        ("main.scss", "@use 'fwd' as f;\n.z { v: f.$p--p }\n"),
    ]);
    assert_eq!(compile_err(&path).message, "Undefined variable.");
}

/// `@forward ... with (... !default)`：下游 `@use ... with` 可以覆盖转发默认值。
#[test]
fn forward_with_default_is_overridable() {
    let path = project(&[
        ("_base.scss", "$c: red !default;\n.b { c: $c }\n"),
        (
            "_theme.scss",
            "@forward 'base' with ($c: green !default);\n",
        ),
        (
            "main.scss",
            "@use 'theme' with ($c: purple);\n.z { c: theme.$c }\n",
        ),
    ]);
    let out = compile(&path).expect("compile");
    assert_eq!(out.css, ".b {\n  c: purple;\n}\n\n.z {\n  c: purple;\n}");
}

/// 不带 `!default` 的转发配置不能被下游覆盖。
#[test]
fn forward_without_default_cannot_be_overridden() {
    let path = project(&[
        ("_base.scss", "$c: red !default;\n"),
        ("_theme.scss", "@forward 'base' with ($c: green);\n"),
        ("main.scss", "@use 'theme' with ($c: purple);\n"),
    ]);
    assert_eq!(
        compile_err(&path).message,
        "This variable was not declared with !default in the @used module."
    );
}

/// 配置值可以是引用本文件变量的任意表达式，且只在模块加载时求值一次。
#[test]
fn config_values_are_sass_expressions() {
    let path = project(&[
        ("_lib.scss", "$w: 2px !default;\n.l { w: $w }\n"),
        (
            "main.scss",
            "$base: 1px;\n@use 'lib' with ($w: $base * 3);\n.z { w: lib.$w }\n",
        ),
    ]);
    let out = compile(&path).expect("compile");
    assert_eq!(out.css, ".l {\n  w: 3px;\n}\n\n.z {\n  w: 3px;\n}");
}

/// 配置值里的表达式即使被 `!default` 抵消也会求值（dart 实测：未定义变量照样报错）。
#[test]
fn config_expressions_are_evaluated_eagerly() {
    let path = project(&[
        ("_lib.scss", "$v: 1 !default;\n"),
        ("main.scss", "@use 'lib' with ($v: $undefined);\n"),
    ]);
    assert_eq!(compile_err(&path).message, "Undefined variable.");
}

/// 配置先下传给被转发的模块；目标不声明这个变量时，配置仍留给本模块后面的
/// `!default` 声明（dart 实测：`x: 2`）。
#[test]
fn config_falls_through_to_the_forwarding_modules_default() {
    let path = project(&[
        ("_e.scss", "$y: 0;\n.a { y: $y }\n"),
        (
            "_fwd.scss",
            "@forward 'e';\n$x: 9 !default;\n.f { x: $x }\n",
        ),
        ("main.scss", "@use 'fwd' with ($x: 2);\n.z { v: fwd.$x }\n"),
    ]);
    let out = compile(&path).expect("compile");
    assert_eq!(
        out.css,
        ".a {\n  y: 0;\n}\n\n.f {\n  x: 2;\n}\n\n.z {\n  v: 2;\n}"
    );
}

/// 反过来：被转发的模块先消费了配置，本模块自己的同名 `!default` 就用默认值。
#[test]
fn config_consumed_by_the_forwarded_module_does_not_reach_the_forwarder() {
    let path = project(&[
        ("_a.scss", "$x: 1 !default;\n.a { x: $x }\n"),
        (
            "_fwd.scss",
            "@forward 'a';\n$x: 9 !default;\n.f { x: $x }\n",
        ),
        ("main.scss", "@use 'fwd' with ($x: 2);\n.z { v: fwd.$x }\n"),
    ]);
    let out = compile(&path).expect("compile");
    assert_eq!(
        out.css,
        ".a {\n  x: 2;\n}\n\n.f {\n  x: 9;\n}\n\n.z {\n  v: 9;\n}"
    );
}
