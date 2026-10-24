//! S3.2 golden comparison: `@forward` and `@use ... with`.
//!
//! Each directory is one independent case: `main.scss` is the entry and `main.css` is the
//! dart-sass 1.93.2 expanded output (trailing newline stripped), asserted byte for byte.

use std::path::Path;

use undercoat::{Options, compile_file};

fn fixture_root() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures-scss-s32")
}

#[test]
fn scss_s32_fixtures_match_dart_sass_byte_for_byte() {
    let dir = fixture_root();
    let mut checked = 0;
    let mut names: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(&dir).expect("read fixtures") {
        let case = entry.expect("dir entry").path();
        if !case.is_dir() {
            continue;
        }
        let path = case.join("main.scss");
        let expected_path = case.join("main.css");
        let expected = std::fs::read_to_string(&expected_path)
            .unwrap_or_else(|e| panic!("missing expected output {}: {e}", expected_path.display()));
        let source = std::fs::read_to_string(&path).expect("read scss");
        let out = compile_file(&path, &source, &Options::default())
            .unwrap_or_else(|e| panic!("{}: {e:#}", path.display()));
        assert_eq!(
            out.css,
            expected,
            "output does not match dart-sass 1.93.2: {}",
            path.display()
        );
        names.push(
            case.file_name()
                .expect("name")
                .to_string_lossy()
                .into_owned(),
        );
        checked += 1;
    }
    for required in [
        "forward-chain",
        "forward-prefix",
        "forward-filter",
        "forward-prefix-show",
        "forward-private",
        "forward-with",
        "forward-config-chain",
        "forward-and-use",
        "use-with",
        "import-forwarder",
    ] {
        assert!(
            names.iter().any(|n| n == required),
            "missing fixture {required}: {names:?}"
        );
    }
    assert_eq!(checked, 10, "unexpected number of S3.2 fixtures: {checked}");
}
