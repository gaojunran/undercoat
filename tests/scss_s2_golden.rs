//! S2 特性 golden 对拍：`@mixin` / `@include` / `@content` / `@if`。
//!
//! 每个目录是一份独立用例：`main.scss` 是入口，`main.css` 是 dart-sass 1.35.2 的
//! expanded 产物（`scripts/style-diff/scss-oracle.cjs` 同款渲染），逐字节断言。
//! 报错用例在 `fixtures-scss-s2-errors/`，断言文案与行列。

use std::path::Path;

use undercoat::{Options, compile_file};

fn fixture_root(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join(name)
}

fn compile(path: &Path) -> Result<undercoat::Output, undercoat::CompileError> {
    let source = std::fs::read_to_string(path).expect("read scss");
    compile_file(path, &source, &Options::default())
}

#[test]
fn scss_s2_fixtures_match_dart_sass_byte_for_byte() {
    let dir = fixture_root("fixtures-scss-s2");
    let mut checked = 0;
    for entry in std::fs::read_dir(&dir).expect("read fixtures") {
        let case = entry.expect("dir entry").path();
        if !case.is_dir() {
            continue;
        }
        let path = case.join("main.scss");
        let expected_path = case.join("main.css");
        let expected = std::fs::read_to_string(&expected_path)
            .unwrap_or_else(|e| panic!("缺少期望产物 {}: {e}", expected_path.display()));
        let out = compile(&path).unwrap_or_else(|e| panic!("{}: {e:#}", path.display()));
        assert_eq!(
            out.css,
            expected,
            "产物与 dart-sass 1.35.2 不一致: {}",
            path.display()
        );
        checked += 1;
    }
    assert!(checked >= 16, "S2 fixtures 数量异常: {checked}");
}

/// 命名空间 mixin 来自 `@use` 的模块，模块文件要登记进 deps。
#[test]
fn scss_s2_namespace_include_records_dependency() {
    let out = compile(&fixture_root("fixtures-scss-s2").join("mixin-namespace/main.scss"))
        .expect("compile");
    let deps = out
        .deps
        .iter()
        .map(|d| {
            Path::new(d)
                .file_name()
                .expect("file name")
                .to_string_lossy()
                .into_owned()
        })
        .collect::<Vec<_>>();
    assert_eq!(deps, ["_shared.scss"]);
}

#[test]
fn scss_s2_error_fixtures_match_dart_sass() {
    let dir = fixture_root("fixtures-scss-s2-errors");
    let mut checked = 0;
    for entry in std::fs::read_dir(&dir).expect("read fixtures") {
        let case = entry.expect("dir entry").path();
        if !case.is_dir() {
            continue;
        }
        let path = case.join("main.scss");
        let raw = std::fs::read_to_string(case.join("main.err")).expect("read main.err");
        let expected: serde_json::Value = serde_json::from_str(&raw).expect("parse main.err");
        let err = match compile(&path) {
            Ok(out) => panic!("{} 应当报错，实际产出:\n{}", path.display(), out.css),
            Err(e) => e,
        };
        assert_eq!(
            err.message,
            expected["message"].as_str().expect("message"),
            "{}",
            path.display()
        );
        assert_eq!(
            err.line,
            Some(expected["line"].as_u64().expect("line") as u32),
            "{} 行号",
            path.display()
        );
        assert_eq!(
            err.column,
            Some(expected["column"].as_u64().expect("column") as u32),
            "{} 列号",
            path.display()
        );
        checked += 1;
    }
    assert!(checked >= 19, "S2 报错 fixtures 数量异常: {checked}");
}

/// 递归 mixin 必须有明确上限，不能把栈打爆。
#[test]
fn scss_s2_recursion_limit_is_explicit() {
    let dir = std::env::temp_dir().join("minibuild-scss-s2-recursion");
    std::fs::create_dir_all(&dir).expect("mkdir");
    let path = dir.join("main.scss");
    std::fs::write(
        &path,
        "@mixin loop {\n  @include loop;\n}\n\n.a {\n  @include loop;\n}\n",
    )
    .expect("write");
    let err = compile(&path).expect_err("应当报错");
    assert!(
        err.message.contains("Mixin nesting depth exceeded"),
        "{}",
        err.message
    );
}
