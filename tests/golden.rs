//! Golden 对拍：`tests/fixtures/*.less` 的期望产物由真实 less@4.1.3 生成
//! （见 `scripts/style-diff/`），这里只做逐字节断言，CI 不依赖 node。

use std::path::Path;

use undercoat::{Options, compile_file};

fn fixtures_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

#[test]
fn fixtures_match_less_byte_for_byte() {
    let dir = fixtures_dir();
    let mut checked = 0;
    for entry in std::fs::read_dir(&dir).expect("read fixtures") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|s| s.to_str()) != Some("less") {
            continue;
        }
        let source = std::fs::read_to_string(&path).expect("read less");
        let expected_path = path.with_extension("css");
        let expected = std::fs::read_to_string(&expected_path)
            .unwrap_or_else(|e| panic!("缺少期望产物 {}: {e}", expected_path.display()));

        let out = compile_file(&path, &source, &Options::default())
            .unwrap_or_else(|e| panic!("{}: {e:#}", path.display()));
        assert_eq!(
            out.css,
            expected,
            "产物与 less@4.1.3 不一致: {}",
            path.display()
        );
        checked += 1;
    }
    assert!(checked >= 6, "fixtures 数量异常: {checked}");
}

#[test]
fn source_map_is_reversible() {
    let dir = fixtures_dir();
    let path = dir.join("basic.less");
    let source = std::fs::read_to_string(&path).expect("read basic.less");
    let out = compile_file(
        &path,
        &source,
        &Options {
            source_map: true,
            project_root: Some(dir.to_string_lossy().to_string()),
            ..Default::default()
        },
    )
    .expect("compile");

    let map = out.map.expect("source map");
    let v: serde_json::Value = serde_json::from_str(&map).expect("parse map");
    assert_eq!(v["version"], 3);
    // sources 已重写为 project_root 相对
    assert_eq!(v["sources"][0], "basic.less");
    // 选择器与声明各至少一条映射
    let mappings = v["mappings"].as_str().expect("mappings");
    assert!(mappings.matches(';').count() >= 1, "mappings = {mappings}");
}
