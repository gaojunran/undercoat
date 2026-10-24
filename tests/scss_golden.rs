//! SCSS golden 对拍：`tests/fixtures-scss/*.scss`（纯 CSS 子集）的期望产物由
//! dart-sass 1.35.2 生成（`scripts/style-diff/scss-oracle.cjs`），这里逐字节断言，
//! CI 不依赖 node。

use std::path::Path;

use undercoat::{Options, compile_file};

fn fixtures_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures-scss")
}

#[test]
fn scss_fixtures_match_dart_sass_byte_for_byte() {
    let dir = fixtures_dir();
    let mut checked = 0;
    for entry in std::fs::read_dir(&dir).expect("read fixtures") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|s| s.to_str()) != Some("scss") {
            continue;
        }
        let source = std::fs::read_to_string(&path).expect("read scss");
        let expected_path = path.with_extension("css");
        let expected = std::fs::read_to_string(&expected_path)
            .unwrap_or_else(|e| panic!("缺少期望产物 {}: {e}", expected_path.display()));

        let out = compile_file(&path, &source, &Options::default())
            .unwrap_or_else(|e| panic!("{}: {e:#}", path.display()));
        assert_eq!(
            out.css,
            expected,
            "产物与 dart-sass 1.35.2 不一致: {}",
            path.display()
        );
        checked += 1;
    }
    assert!(checked >= 4, "fixtures 数量异常: {checked}");
}
