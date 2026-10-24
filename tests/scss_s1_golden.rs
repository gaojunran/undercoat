//! S1 特性 golden 对拍：变量/作用域、嵌套 + `&`、插值、`@import`、`@use`。
//!
//! 每个目录是一份独立用例：`main.scss` 是入口，`main.css` 是 dart-sass 1.35.2 的
//! expanded 产物（`scripts/style-diff/scss-oracle.cjs` 生成），逐字节断言。
//! 报错用例在 `fixtures-scss-s1-errors/`，断言文案与行列。

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
fn scss_s1_fixtures_match_dart_sass_byte_for_byte() {
    let dir = fixture_root("fixtures-scss-s1");
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
    assert!(checked >= 6, "S1 fixtures 数量异常: {checked}");
}

/// `@import` / `@use` 都要把加载到的文件登记进 `deps`（sourcemap 与 watch 依赖它）。
#[test]
fn scss_s1_records_dependencies() {
    let dir = fixture_root("fixtures-scss-s1");
    let deps_of = |case: &str| {
        let out = compile(&dir.join(case).join("main.scss")).expect("compile");
        out.deps
            .iter()
            .map(|d| {
                Path::new(d)
                    .file_name()
                    .expect("file name")
                    .to_string_lossy()
                    .into_owned()
            })
            .collect::<Vec<_>>()
    };

    let import = deps_of("import-basic");
    assert_eq!(
        import,
        ["_tokens.scss", "_button.scss", "_index.scss", "_part.scss"],
        "@import 的 deps 应当按加载顺序、去重"
    );

    let use_default = deps_of("use-default");
    assert_eq!(use_default, ["_theme.scss"]);

    let use_alias = deps_of("use-alias");
    assert_eq!(
        use_alias,
        ["_theme.scss", "_palette.scss"],
        "同一模块 `@use` 两次只登记一次"
    );
}

#[test]
fn scss_s1_error_fixtures_match_dart_sass() {
    let dir = fixture_root("fixtures-scss-s1-errors");
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
            Some(expected["line"].as_u64().expect("line") as u32)
        );
        assert_eq!(
            err.column,
            Some(expected["column"].as_u64().expect("column") as u32)
        );
        checked += 1;
    }
    assert!(checked >= 4, "S1 报错 fixtures 数量异常: {checked}");
}
