//! Semantic boundaries of `@forward` / `@use ... with` (verified against dart-sass 1.93.2).
//!
//! Positive output lives in `tests/fixtures-scss-s32`; here we cover visibility, error
//! messages and positions.

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
        Ok(out) => panic!(
            "{} should have failed, but produced:\n{}",
            path.display(),
            out.css
        ),
        Err(e) => e,
    }
}

/// Members of a `@forward` are not visible inside the forwarding module's own scope
/// (the key difference from `@use`).
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

/// `@use ... as *` does not turn members into exports either (the S9 isolation
/// regression).
#[test]
fn star_import_does_not_leak_into_forwards() {
    let path = project(&[
        ("_lib.scss", "$v: 1;\n"),
        ("_relay.scss", "@use 'lib' as *;\n"),
        ("main.scss", "@use 'relay' as *;\n.z { v: $v }\n"),
    ]);
    assert_eq!(compile_err(&path).message, "Undefined variable.");
}

/// Configuring a non-`!default` variable: dart's message and position.
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
    // Points at the start of `$v` (character 0 after `@use "lib" with (`).
    assert_eq!((err.line, err.column), (Some(1), Some(17)));
}

/// An unknown variable in `with` reports "not !default" as well.
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

/// Configuring the same module twice is an error.
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

/// Plain `@use` followed by a configuration: also reports "already loaded".
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

/// Two different modules forwarded into the same scope with a name clash: dart reports
/// at the second `@forward`.
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

/// Same-named functions/mixins conflict too (dart merges both into one namespace, so the
/// message says function).
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

/// Diamond forwarding (the same module reached through two paths) is not a conflict.
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

/// A module's own same-named members win over forwarded ones (dart reports no conflict).
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

/// Private members (`-` / `_` prefix) are not forwarded and cannot be accessed from
/// outside the module.
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

/// `@import` a file that only contains `@forward`: forwarded members are visible at the
/// import site (verified against dart).
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

/// `show` / `hide` lists name **prefixed** members (verified against dart).
#[test]
fn forward_filter_sees_prefixed_names() {
    let path = project(&[
        ("_lib.scss", "$x: 1;\n"),
        ("_fwd.scss", "@forward 'lib' as p-* show $p-x;\n"),
        ("main.scss", "@use 'fwd' as f;\n.z { v: f.$p-x }\n"),
    ]);
    assert_eq!(compile(&path).expect("compile").css, ".z {\n  v: 1;\n}");

    // Writing the original name (without the prefix) matches nothing.
    let path = project(&[
        ("_lib.scss", "$x: 1;\n"),
        ("_fwd.scss", "@forward 'lib' as p-* show $x;\n"),
        ("main.scss", "@use 'fwd' as f;\n.z { v: f.$p-x }\n"),
    ]);
    assert_eq!(compile_err(&path).message, "Undefined variable.");
}

/// The prefix is only prepended to forwarded names; private members are still not
/// forwarded.
#[test]
fn prefix_does_not_revive_private_members() {
    let path = project(&[
        ("_lib.scss", "$-p: 1;\n"),
        ("_fwd.scss", "@forward 'lib' as p-*;\n"),
        ("main.scss", "@use 'fwd' as f;\n.z { v: f.$p--p }\n"),
    ]);
    assert_eq!(compile_err(&path).message, "Undefined variable.");
}

/// `@forward ... with (... !default)`: a downstream `@use ... with` may override the
/// forwarding default.
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

/// A forwarding configuration without `!default` cannot be overridden downstream.
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

/// Configuration values may be arbitrary expressions referring to variables of this
/// file, and are evaluated exactly once, when the module loads.
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

/// Expressions in configuration values are evaluated even when `!default` cancels them
/// (verified against dart: an undefined variable still errors).
#[test]
fn config_expressions_are_evaluated_eagerly() {
    let path = project(&[
        ("_lib.scss", "$v: 1 !default;\n"),
        ("main.scss", "@use 'lib' with ($v: $undefined);\n"),
    ]);
    assert_eq!(compile_err(&path).message, "Undefined variable.");
}

/// The configuration is first passed down to forwarded modules; when the target does not
/// declare that variable, the configuration is still left for this module's later
/// `!default` declaration (verified against dart: `x: 2`).
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

/// The other way round: once the forwarded module has consumed the configuration, this
/// module's own same-named `!default` uses the default value.
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
