//! `.sass` 缩进语法 golden 对拍：`tests/fixtures-scss-sass/*` 的期望产物由
//! dart-sass 1.93.2 实测生成（`sass --no-source-map --quiet`，去掉 CLI 补的尾随换行），
//! 逐字节断言；报错用例在 `fixtures-scss-sass-errors/`，断言文案与行列。
//!
//! 用例覆盖变量、嵌套、`&`、`=mixin`/`+include`、`@mixin`/`@include`、`@use`/`@import`
//! （含 `_partial.sass`）、`@if`/`@each`/`@for`/`@while`、多行选择器、`//`/`/* */`
//! 注释、`@media` 嵌套、`@keyframes`（`from`/`to`）、插值、嵌套属性、续行。

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
            .unwrap_or_else(|e| panic!("缺少期望产物 {}: {e}", expected_path.display()));
        let out = compile(&path).unwrap_or_else(|e| panic!("{}: {e:#}", path.display()));
        assert_eq!(
            out.css,
            expected,
            "产物与 dart-sass 1.93.2 不一致: {}",
            path.display()
        );
        checked += 1;
    }
    assert!(checked >= 11, "Sass fixtures 数量异常: {checked}");
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
    assert!(checked >= 3, "Sass 报错 fixtures 数量异常: {checked}");
}

/// `.sass` 里的 `@import` 支持 `.scss` / `.sass` 混用（dart 允许），依赖表两边的文件都要在。
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
