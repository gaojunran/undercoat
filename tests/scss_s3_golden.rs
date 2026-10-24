//! S3 golden comparison: `@function` / `@return` / `@each` / `@for` / `@while` plus pure
//! CSS function pass-through (`rgb(var(--x))`, `min()`/`max()`).
//!
//! Each directory is one independent case: `main.scss` is the entry and `main.css` is the
//! dart-sass 1.35.2 expanded output, asserted byte for byte. Error cases live in
//! `fixtures-scss-s3-errors/`, asserting the message together with line and column.

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
            .unwrap_or_else(|e| panic!("missing expected output {}: {e}", expected_path.display()));
        let out = compile(&path).unwrap_or_else(|e| panic!("{}: {e:#}", path.display()));
        assert_eq!(
            out.css,
            expected,
            "output does not match dart-sass 1.35.2: {}",
            path.display()
        );
        checked += 1;
    }
    assert!(checked >= 12, "unexpected number of S3 fixtures: {checked}");
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
            Ok(out) => panic!(
                "{} should have failed, but produced:\n{}",
                path.display(),
                out.css
            ),
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
            "{} line",
            path.display()
        );
        assert_eq!(
            err.column,
            Some(expected["column"].as_u64().expect("column") as u32),
            "{} column",
            path.display()
        );
        checked += 1;
    }
    assert!(
        checked >= 9,
        "unexpected number of S3 error fixtures: {checked}"
    );
}

/// `@debug` / `@warn` go to stderr in the same form as dart (`@debug` uses inspect,
/// stripping quotes from a top-level quoted string; `@warn` uses the CSS form).
#[test]
fn scss_s3_debug_warn_use_dart_message_form() {
    let path = fixture_root("fixtures-scss-s3").join("debug-warn/main.scss");
    let out = Command::new(env!("CARGO_BIN_EXE_undercoat"))
        .arg(&path)
        .output()
        .expect("run undercoat");
    assert!(
        out.status.success(),
        "compilation should succeed: {:?}",
        out.status
    );
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
            "stderr is missing {expected:?}:\n{stderr}"
        );
    }
}

/// A recursive function must have an explicit limit and must not blow the stack.
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
    let err = compile(&path).expect_err("should fail");
    assert!(
        err.message.contains("Function nesting depth exceeded"),
        "{}",
        err.message
    );
}
