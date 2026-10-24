//! `sourcesContent` policy of the less-side sourcemap: **omitted for a single source,
//! written only for multiple sources**.
//!
//! This is the opposite of dart (`Style::sources_content`): lessc's `outputSourceFiles`
//! only takes effect after `@import` inlining, so a single-source `.map` has no
//! `sourcesContent` field at all.

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

/// Single source: no `sourcesContent` (the bytes of a single-source lessc `.map`
/// simply don't contain this field).
#[test]
fn single_source_omits_sources_content() {
    let (css, map) = compile("basic.less");
    let expected = std::fs::read_to_string(fixtures_dir().join("basic.css")).expect("basic.css");
    assert_eq!(
        css, expected,
        "single-source output must stay byte-identical too"
    );
    assert_eq!(map["sources"].as_array().expect("sources").len(), 1);
    assert!(
        map.get("sourcesContent").is_none(),
        "a single-source less map should not carry sourcesContent: {map}"
    );
}

/// Multiple sources (`@import` inlined): `sourcesContent` corresponds one-to-one with
/// `sources` and holds the raw file text.
#[test]
fn imported_sources_carry_content() {
    let (css, map) = compile("import-multiple.less");
    let expected =
        std::fs::read_to_string(fixtures_dir().join("import-multiple.css")).expect("expected css");
    assert_eq!(
        css, expected,
        "multi-source output must stay byte-identical too"
    );

    let sources = map["sources"].as_array().expect("sources");
    let contents = map["sourcesContent"].as_array().expect("sourcesContent");
    assert_eq!(
        contents.len(),
        sources.len(),
        "the two must correspond one-to-one"
    );
    // At least the inlined `imports/part.less` text must be findable in the map.
    let part = std::fs::read_to_string(fixtures_dir().join("imports/part.less")).expect("part");
    let found = contents.iter().any(|c| c.as_str() == Some(part.as_str()));
    assert!(
        found,
        "cannot find the inlined file's text in sourcesContent: {contents:?}"
    );
}
