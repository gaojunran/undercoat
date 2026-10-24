//! S3 特性 golden 对拍：`@function` / `@return` / `@each` / `@for` / `@while`
//! 以及纯 CSS 函数透传（`rgb(var(--x))`、`min()`/`max()`）。
//!
//! 每个目录是一份独立用例：`main.scss` 是入口，`main.css` 是 dart-sass 1.35.2 的
//! expanded 产物，逐字节断言。报错用例在 `fixtures-scss-s3-errors/`，断言文案与行列。

use std::path::Path;
use std::process::Command;

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
fn scss_s3_fixtures_match_dart_sass_byte_for_byte() {
    let dir = fixture_root("fixtures-scss-s3");
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
    assert!(checked >= 12, "S3 fixtures 数量异常: {checked}");
}

#[test]
fn scss_s3_error_fixtures_match_dart_sass() {
    let dir = fixture_root("fixtures-scss-s3-errors");
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
    assert!(checked >= 9, "S3 报错 fixtures 数量异常: {checked}");
}

/// `@debug` / `@warn` 走 stderr，形态与 dart 一致（`@debug` 用 inspect，
/// 顶层带引号字符串去掉引号，`@warn` 用 CSS 形态）。
#[test]
fn scss_s3_debug_warn_use_dart_message_form() {
    let path = fixture_root("fixtures-scss-s3").join("debug-warn/main.scss");
    let out = Command::new(env!("CARGO_BIN_EXE_undercoat"))
        .arg(&path)
        .output()
        .expect("run undercoat");
    assert!(out.status.success(), "编译应当成功: {:?}", out.status);
    let stderr = String::from_utf8_lossy(&out.stderr);
    for expected in [
        "DEBUG: 1, 2",
        "DEBUG: null",
        "DEBUG: #fff",
        "DEBUG: quoted string",
        "DEBUG: (a: \"q\")",
        "WARNING: careful with dark",
        "root stylesheet",
    ] {
        assert!(
            stderr.contains(expected),
            "stderr 缺少 {expected:?}:\n{stderr}"
        );
    }
}

/// 递归函数必须有明确上限，不能把栈打爆。
#[test]
fn scss_s3_function_recursion_limit_is_explicit() {
    let dir = std::env::temp_dir().join("minibuild-scss-s3-recursion");
    std::fs::create_dir_all(&dir).expect("mkdir");
    let path = dir.join("main.scss");
    std::fs::write(
        &path,
        "@function loop($n) {\n  @return loop($n + 1);\n}\n\n.a {\n  v: loop(1);\n}\n",
    )
    .expect("write");
    let err = compile(&path).expect_err("应当报错");
    assert!(
        err.message.contains("Function nesting depth exceeded"),
        "{}",
        err.message
    );
}
