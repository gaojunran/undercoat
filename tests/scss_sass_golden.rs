//! `.sass` indented-syntax golden comparison: expected output for
//! `tests/fixtures-scss-sass/*` was produced from real dart-sass 1.93.2 runs
//! (`sass --no-source-map --quiet`, with the trailing newline added by the CLI stripped) and
//! is asserted byte for byte; error cases live in `fixtures-scss-sass-errors/`, asserting
//! the message together with line and column.
//!
//! The cases cover variables, nesting, `&`, `=mixin`/`+include`, `@mixin`/`@include`,
//! `@use`/`@import` (including `_partial.sass`), `@if`/`@each`/`@for`/`@while`, multi-line
//! selectors, `//`/`/* */` comments, nested `@media`, `@keyframes` (`from`/`to`),
//! interpolation, nested properties and continuation lines.

use std::path::{Path, PathBuf};

use undercoat::{Options, compile_file};

fn fixture_root(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join(name)
}

fn entry(case: &Path) -> PathBuf {
    let sass = case.join("main.sass");
    if sass.is_file() {
        sass
    } else {
        case.join("main.scss")
    }
}

fn compile(path: &Path) -> Result<undercoat::Output, undercoat::CompileError> {
    let source = std::fs::read_to_string(path).expect("read source");
    compile_file(path, &source, &Options::default())
}

#[test]
fn sass_fixtures_match_dart_sass_byte_for_byte() {
    let dir = fixture_root("fixtures-scss-sass");
    let mut checked = 0;
    for entry in std::fs::read_dir(&dir).expect("read fixtures") {
        let case = entry.expect("dir entry").path();
        if !case.is_dir() {
            continue;
        }
        let path = self::entry(&case);
        let expected_path = case.join("main.css");
        let expected = std::fs::read_to_string(&expected_path)
            .unwrap_or_else(|e| panic!("missing expected output {}: {e}", expected_path.display()));
        let out = compile(&path).unwrap_or_else(|e| panic!("{}: {e:#}", path.display()));
        assert_eq!(
            out.css,
            expected,
            "output does not match dart-sass 1.93.2: {}",
            path.display()
        );
        checked += 1;
    }
    assert!(
        checked >= 11,
        "unexpected number of Sass fixtures: {checked}"
    );
}

#[test]
fn sass_error_fixtures_match_dart_sass() {
    let dir = fixture_root("fixtures-scss-sass-errors");
    let mut checked = 0;
    for entry in std::fs::read_dir(&dir).expect("read fixtures") {
        let case = entry.expect("dir entry").path();
        if !case.is_dir() {
            continue;
        }
        let path = self::entry(&case);
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
        checked >= 3,
        "unexpected number of Sass error fixtures: {checked}"
    );
}

/// `@import` inside `.sass` may mix `.scss` / `.sass` (dart allows it), and the dependency
/// list must contain files from both sides.
#[test]
fn sass_import_mixing_reports_both_dependencies() {
    let path = fixture_root("fixtures-scss-sass").join("import-mix/main.sass");
    let out = compile(&path).expect("compile");
    let deps: Vec<String> = out
        .deps
        .iter()
        .map(|d| {
            Path::new(d)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        })
        .collect();
    assert!(deps.contains(&"legacy.sass".to_string()), "{deps:?}");
    assert!(deps.contains(&"_plain.scss".to_string()), "{deps:?}");
}
