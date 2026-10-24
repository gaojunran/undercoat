//! S3.1 golden comparison: three plain-CSS printing bugs found in real mini-program
//! corpora (keyframe selector lists on one line, block-comment continuation reindenting,
//! double-quote normalization of strings in custom property values), plus weaving for
//! single-part extenders and nested properties (`prop: { sub: v }`).
//!
//! Each directory is one independent case: `main.scss` is the entry and `main.css` is the
//! dart-sass 1.35.2 expanded output, asserted byte for byte.

use std::path::Path;

use undercoat::{Options, compile_file};

fn fixture_root() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures-scss-s31")
}

#[test]
fn scss_s31_fixtures_match_dart_sass_byte_for_byte() {
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
            "output does not match dart-sass 1.35.2: {}",
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
        "keyframe-list",
        "comments-reindent",
        "custom-prop-quotes",
        "custom-prop-whitespace",
        "nested-properties",
        "extend-weave",
    ] {
        assert!(
            names.iter().any(|n| n == required),
            "missing fixture {required}: {names:?}"
        );
    }
    assert_eq!(checked, 6, "unexpected number of S3.1 fixtures: {checked}");
}

/// Details of bug A: a keyframe selector list is always one line joined with `, `, while
/// ordinary selectors keep the source line breaks.
#[test]
fn keyframe_selector_list_is_single_line_but_selector_list_is_not() {
    let dir = std::env::temp_dir().join("minibuild-scss-s31-keyframe");
    std::fs::create_dir_all(&dir).expect("mkdir");
    let path = dir.join("main.scss");
    std::fs::write(
        &path,
        "@keyframes d {\n  0%,\n  50% { x: 1 }\n}\n\n.a,\n.b { y: 2 }\n",
    )
    .expect("write");
    let source = std::fs::read_to_string(&path).expect("read");
    let css = compile_file(&path, &source, &Options::default())
        .expect("compile")
        .css;
    assert!(
        css.contains("@keyframes d {\n  0%, 50% {\n    x: 1;\n  }\n}"),
        "{css}"
    );
    assert!(
        css.contains(".a,\n.b {"),
        "ordinary selectors must keep their line breaks: {css}"
    );
}

/// A nested property block only allows declarations and `@include`/control flow: any
/// other at-rule reports dart's message.
#[test]
fn nested_property_block_rejects_other_at_rules() {
    let dir = std::env::temp_dir().join("minibuild-scss-s31-nested-at");
    std::fs::create_dir_all(&dir).expect("mkdir");
    let path = dir.join("main.scss");
    std::fs::write(
        &path,
        ".a {\n  margin: 1px {\n    top: 2px;\n    @extend .z;\n  }\n}\n\n.z { q: 1 }\n",
    )
    .expect("write");
    let source = std::fs::read_to_string(&path).expect("read");
    let err = compile_file(&path, &source, &Options::default()).expect_err("should fail");
    assert_eq!(err.message, "This at-rule is not allowed here.");
}

/// Bug F: a whitespace run is collapsed to "the run's last character", and continuation
/// indentation is shifted per character while tabs are preserved.
#[test]
fn custom_property_whitespace_runs_keep_their_last_char() {
    let dir = std::env::temp_dir().join("minibuild-scss-s31-custom-ws");
    std::fs::create_dir_all(&dir).expect("mkdir");
    let path = dir.join("main.scss");
    std::fs::write(
        &path,
        ".t {\n\t--a:  \t#111;\n\t--b: \t  #222;\n\t--c:   #333;\n\t--d:\t#444;\n\t--e:\n\t\t#555;\n}\n",
    )
    .expect("write");
    let source = std::fs::read_to_string(&path).expect("read");
    let css = compile_file(&path, &source, &Options::default())
        .expect("compile")
        .css;
    // Run `space + tab` → keep the tab; `tab + space` → keep the space; all-space / all-tab
    // behave the same way.
    assert!(css.contains("  --a:\t#111;"), "{css:?}");
    assert!(css.contains("  --b: #222;"), "{css:?}");
    assert!(css.contains("  --c: #333;"), "{css:?}");
    assert!(css.contains("  --d:\t#444;"), "{css:?}");
    // Continuation line: output indent of 2 spaces + max(0, source indent 2 tabs − declaration
    // column 1) = 1 tab.
    assert!(css.contains("  --e:\n  \t#555;"), "{css:?}");
}

/// Details of bug B: tabs are preserved per character (continuation = output indent +
/// source indent − comment column).
#[test]
fn comment_continuation_keeps_tabs_and_blanks() {
    let dir = std::env::temp_dir().join("minibuild-scss-s31-comment");
    std::fs::create_dir_all(&dir).expect("mkdir");
    let path = dir.join("main.scss");
    std::fs::write(
        &path,
        ".a {\n\t/* note\n\t\ttransform: none; */\n\tx: 1;\n}\n\n/* top\n\n  below */\n",
    )
    .expect("write");
    let source = std::fs::read_to_string(&path).expect("read");
    let css = compile_file(&path, &source, &Options::default())
        .expect("compile")
        .css;
    // Indent of 2 spaces on output + (2 tabs on the source continuation − comment column 1)
    // → 1 tab is kept.
    assert!(css.contains("  /* note\n  \ttransform: none; */"), "{css}");
    // An empty line is a truly empty line, with no indent spaces.
    assert!(css.contains("/* top\n\n  below */"), "{css}");
}
