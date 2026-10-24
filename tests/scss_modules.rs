//! `@use ... as *` 的作用域边界：成员只属于**本模块**（可无限定名使用、可被本模块
//! `@forward` 显式转发），但**不进入**本模块的对外成员表——下游 `@use this as *`
//! 与 `this.$x` 都读不到。dart-sass 1.93.2 实测语义。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use undercoat::{CompileError, Options, compile_file};

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// 在临时目录里铺一组文件，返回 `main.scss` 的路径。
fn project(files: &[(&str, &str)]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "minibuild-scss-modules-{}-{}",
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

#[test]
fn star_module_variable_is_not_exported() {
    let path = project(&[
        ("_two.scss", "$v: two;\n"),
        ("_relay.scss", "@use 'two' as *;\n"),
        ("main.scss", "@use 'relay' as *;\n.z { v: $v }\n"),
    ]);
    let err = compile_err(&path);
    assert_eq!(err.message, "Undefined variable.");
    assert_eq!((err.line, err.column), (Some(2), Some(8)));

    // 命名空间读同样读不到。
    let path = project(&[
        ("_two.scss", "$v: two;\n"),
        ("_relay.scss", "@use 'two' as *;\n"),
        ("main.scss", "@use 'relay';\n.z { v: relay.$v }\n"),
    ]);
    assert_eq!(compile_err(&path).message, "Undefined variable.");
}

#[test]
fn star_module_mixin_and_function_are_not_exported() {
    let path = project(&[
        (
            "_m.scss",
            "@mixin mm { x: 1 }\n@function ff() { @return 42; }\n",
        ),
        ("_relay.scss", "@use 'm' as *;\n"),
        ("main.scss", "@use 'relay' as *;\n.z { @include mm; }\n"),
    ]);
    assert_eq!(compile_err(&path).message, "Undefined mixin.");

    // 函数名不被认识时按 CSS 函数透传（dart 同样输出 `f: ff()`）。
    let path = project(&[
        (
            "_m.scss",
            "@mixin mm { x: 1 }\n@function ff() { @return 42; }\n",
        ),
        ("_relay.scss", "@use 'm' as *;\n"),
        ("main.scss", "@use 'relay' as *;\n.z { f: ff() }\n"),
    ]);
    let out = compile(&path).expect("compile");
    assert!(out.css.contains("f: ff()"), "{}", out.css);
}

#[test]
fn star_module_builtin_variable_is_not_exported() {
    let path = project(&[
        ("_relay.scss", "@use 'sass:math' as *;\n"),
        ("main.scss", "@use 'relay' as *;\n.z { v: $pi }\n"),
    ]);
    assert_eq!(compile_err(&path).message, "Undefined variable.");
}

#[test]
fn star_members_stay_usable_inside_the_module() {
    let path = project(&[
        (
            "_m.scss",
            "$v: two;\n@mixin mm { x: 1 }\n@function ff() { @return 42; }\n",
        ),
        (
            "_relay.scss",
            "@use 'm' as *;\n.from-relay { v: $v; @include mm; f: ff() }\n",
        ),
        ("main.scss", "@use 'relay';\n"),
    ]);
    let out = compile(&path).expect("compile");
    let expected = ".from-relay {\n  v: two;\n  x: 1;\n  f: 42;\n}";
    assert_eq!(out.css, expected);
}

#[test]
fn meta_existence_sees_star_members_only_in_scope() {
    // 模块内：`as *` 的成员算「存在」（dart 实测为 true）。
    let path = project(&[
        (
            "_m.scss",
            "$v: two;\n@mixin mm { x: 1 }\n@function ff() { @return 42; }\n",
        ),
        (
            "_relay.scss",
            "@use 'm' as *;\n.from-relay { v: variable-exists(v); m: mixin-exists(mm); f: function-exists(ff) }\n",
        ),
        ("main.scss", "@use 'relay';\n"),
    ]);
    let out = compile(&path).expect("compile");
    assert!(
        out.css.contains("v: true") && out.css.contains("m: true") && out.css.contains("f: true"),
        "{}",
        out.css
    );

    // 下游：同名成员不存在（dart 实测为 false）。
    let path = project(&[
        (
            "_m.scss",
            "$v: two;\n@mixin mm { x: 1 }\n@function ff() { @return 42; }\n",
        ),
        ("_relay.scss", "@use 'm' as *;\n.relay { v: $v }\n"),
        (
            "main.scss",
            "@use 'relay' as *;\n.z { m: mixin-exists(mm); f: function-exists(ff); v: variable-exists(v) }\n",
        ),
    ]);
    let out = compile(&path).expect("compile");
    assert!(
        out.css.contains("m: false")
            && out.css.contains("f: false")
            && out.css.contains("v: false"),
        "{}",
        out.css
    );
}

#[test]
fn forward_still_exports_members() {
    // `@forward` 才是导出：`relay` 显式转发后，下游能无限定名读到。
    let path = project(&[
        (
            "_one.scss",
            "$v: one;\n@mixin mm { x: 1 }\n@function ff() { @return 42; }\n",
        ),
        ("_relay.scss", "@forward 'one';\n"),
        (
            "main.scss",
            "@use 'relay' as *;\n.z { v: $v; @include mm; f: ff() }\n",
        ),
    ]);
    let out = compile(&path).expect("compile");
    assert_eq!(out.css, ".z {\n  v: one;\n  x: 1;\n  f: 42;\n}");
}

#[test]
fn forward_and_star_coexist_in_one_module() {
    // `@forward 'one'` 导出 one，`@use 'two' as *` 只在 relay 内部可见。
    let path = project(&[
        ("_one.scss", "$v: one;\n"),
        ("_two.scss", "$v: two;\n$w: only-in-relay;\n"),
        (
            "_relay.scss",
            "@forward 'one';\n@use 'two' as *;\n.in-relay { w: $w }\n",
        ),
        ("main.scss", "@use 'relay' as *;\n.z { v: $v }\n"),
    ]);
    let out = compile(&path).expect("compile");
    assert_eq!(
        out.css,
        ".in-relay {\n  w: only-in-relay;\n}\n\n.z {\n  v: one;\n}"
    );

    let path = project(&[
        ("_one.scss", "$v: one;\n"),
        ("_two.scss", "$v: two;\n$w: only-in-relay;\n"),
        ("_relay.scss", "@forward 'one';\n@use 'two' as *;\n"),
        ("main.scss", "@use 'relay' as *;\n.z { w: $w }\n"),
    ]);
    assert_eq!(compile_err(&path).message, "Undefined variable.");
}

#[test]
fn star_members_do_not_shadow_forwarded_exports() {
    // 下游同时 `@use 'one' as *` 与 `@use 'relay' as *`（relay 只泄漏 one 的值时才是
    // `two`）；修好后 relay 不导出任何东西，第一段 `one` 胜出。
    let path = project(&[
        ("_one.scss", "$v: one;\n"),
        ("_two.scss", "$v: two;\n"),
        ("_relay.scss", "@use 'two' as *;\n"),
        (
            "main.scss",
            "@use 'one' as *;\n@use 'relay' as *;\n.z { v: $v }\n",
        ),
    ]);
    let out = compile(&path).expect("compile");
    assert_eq!(out.css, ".z {\n  v: one;\n}");
}
