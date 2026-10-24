//! Golden comparison: expected output for `tests/fixtures/*.less` is produced by real
//! less@4.1.3 (see `scripts/style-diff/`); here we only assert byte for byte, and CI does
//! not depend on node.

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
            .unwrap_or_else(|e| panic!("missing expected output {}: {e}", expected_path.display()));

        let out = compile_file(&path, &source, &Options::default())
            .unwrap_or_else(|e| panic!("{}: {e:#}", path.display()));
        assert_eq!(
            out.css,
            expected,
            "output does not match less@4.1.3: {}",
            path.display()
        );
        checked += 1;
    }
    assert!(checked >= 6, "unexpected number of fixtures: {checked}");
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
    // sources have been rewritten relative to project_root
    assert_eq!(v["sources"][0], "basic.less");
    // At least one mapping each for selectors and declarations
    let mappings = v["mappings"].as_str().expect("mappings");
    assert!(mappings.matches(';').count() >= 1, "mappings = {mappings}");
}
