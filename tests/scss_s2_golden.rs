//! S2 golden comparison: `@mixin` / `@include` / `@content` / `@if`.
//!
//! Each directory is one independent case: `main.scss` is the entry and `main.css` is the
//! dart-sass 1.35.2 expanded output (rendered by the same setup as
//! `scripts/style-diff/scss-oracle.cjs`), asserted byte for byte. Error cases live in
//! `fixtures-scss-s2-errors/`, asserting the message together with line and column.

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
    assert!(checked >= 16, "unexpected number of S2 fixtures: {checked}");
}

/// A namespaced mixin comes from a module loaded with `@use`, so the module file must be
/// recorded in deps.
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
        checked >= 19,
        "unexpected number of S2 error fixtures: {checked}"
    );
}

/// A recursive mixin must have an explicit limit and must not blow the stack.
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
    let err = compile(&path).expect_err("should fail");
    assert!(
        err.message.contains("Mixin nesting depth exceeded"),
        "{}",
        err.message
    );
}
