//! less 侧 sourcemap 的 `sourcesContent` 策略：**单源不写、多源才写**。
//!
//! 这与 dart 相反（`Style::sources_content`）：lessc 的 `outputSourceFiles` 只在
//! `@import` 内联后生效，单源 `.map` 里根本没有 `sourcesContent` 字段。

use std::path::{Path, PathBuf};

use undercoat::{Options, compile_file};

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn compile(entry: &str) -> (String, serde_json::Value) {
    let path = fixtures_dir().join(entry);
    let source = std::fs::read_to_string(&path).expect("read entry");
    let opts = Options {
        source_map: true,
        ..Default::default()
    };
    let out = compile_file(&path, &source, &opts).unwrap_or_else(|e| panic!("{entry}: {e:#}"));
    let map = serde_json::from_str(out.map.as_deref().expect("map")).expect("map json");
    (out.css, map)
}

/// 单源：没有 `sourcesContent`（lessc 单源 `.map` 的字节里就没有这个字段）。
#[test]
fn single_source_omits_sources_content() {
    let (css, map) = compile("basic.less");
    let expected = std::fs::read_to_string(fixtures_dir().join("basic.css")).expect("basic.css");
    assert_eq!(css, expected, "单源产物也要逐字节不变");
    assert_eq!(map["sources"].as_array().expect("sources").len(), 1);
    assert!(
        map.get("sourcesContent").is_none(),
        "less 单源 map 不该带 sourcesContent：{map}"
    );
}

/// 多源（`@import` 内联）：`sourcesContent` 与 `sources` 一一对应，且是文件原文。
#[test]
fn imported_sources_carry_content() {
    let (css, map) = compile("import-multiple.less");
    let expected =
        std::fs::read_to_string(fixtures_dir().join("import-multiple.css")).expect("expected css");
    assert_eq!(css, expected, "多源产物也要逐字节不变");

    let sources = map["sources"].as_array().expect("sources");
    let contents = map["sourcesContent"].as_array().expect("sourcesContent");
    assert_eq!(contents.len(), sources.len(), "两者必须一一对应");
    // 至少内联进来的 `imports/part.less` 原文能在 map 里找到。
    let part = std::fs::read_to_string(fixtures_dir().join("imports/part.less")).expect("part");
    let found = contents.iter().any(|c| c.as_str() == Some(part.as_str()));
    assert!(
        found,
        "sourcesContent 里找不到被内联文件的原文：{contents:?}"
    );
}
