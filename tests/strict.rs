//! Semantic strictness: inputs that upset less@4.1.3 must upset us too; we must not
//! silently degrade them into a wrong artifact.
//!
//! Each `tests/fixtures/strict/<case>.less` comes with a `<case>.err` holding the error
//! text less 4.1.3 produces for the same input (with type prefixes such as `NameError:`
//! removed).
//!
//! At the same time this pins down "what must not error stays quiet": an unmatched guard,
//! `isdefined()`, definitions inside a namespace, `each`, detached ruleset calls and other
//! already-correct scenarios.

use std::path::Path;

use undercoat::{Options, compile_file};

fn strict_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/strict")
}

#[test]
fn less_errors_are_errors() {
    let dir = strict_dir();
    let mut checked = 0;
    for entry in std::fs::read_dir(&dir).expect("read strict fixtures") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|s| s.to_str()) != Some("less") {
            continue;
        }
        let expected_path = path.with_extension("err");
        let expected = std::fs::read_to_string(&expected_path)
            .unwrap_or_else(|e| panic!("missing expected error {}: {e}", expected_path.display()));
        let source = std::fs::read_to_string(&path).expect("read less");
        let err = match compile_file(&path, &source, &Options::default()) {
            Ok(out) => panic!(
                "{} should have failed, but produced:\n{}",
                path.display(),
                out.css
            ),
            Err(e) => format!("{e:#}"),
        };
        assert_eq!(
            err.trim(),
            expected.trim(),
            "error message does not match less@4.1.3: {}",
            path.display()
        );
        checked += 1;
    }
    assert!(checked >= 8, "unexpected number of strict cases: {checked}");
}

#[test]
fn legit_lookups_stay_silent() {
    // less reports no error on any of these inputs; they were already correct before the
    // change and must not be broken by "strictness".
    let cases = [
        // mixin call whose guard does not match: silent
        (".m(@a) when (@a > 5) { x: @a; }\n.n { .m(1); }\n", ""),
        // definition inside a namespace + namespaced call
        (
            "#ns { .m() { x: 1; } }\n.a { #ns > .m(); }\n",
            ".a {\n  x: 1;\n}\n",
        ),
        // `isdefined()` probing an undefined variable
        (".a { v: isdefined(@nope); }\n", ".a {\n  v: false;\n}\n"),
        // detached ruleset call
        ("@dr: { a: 1; };\n.n { @dr(); }\n", ".n {\n  a: 1;\n}\n"),
        // statement-level expansion of `each`
        (
            "each(1 2, { .x@{value} { y: @value; } });\n",
            ".x1 {\n  y: 1;\n}\n.x2 {\n  y: 2;\n}\n",
        ),
        // lazy evaluation: variables declared later are visible in a rule body
        (".a { v: @later; }\n@later: 1;\n", ".a {\n  v: 1;\n}\n"),
        // `@@name` indirect reference
        (
            "@name: 'x';\n@x: 1;\n.a { v: @@name; }\n",
            ".a {\n  v: 1;\n}\n",
        ),
        // `@arguments` / `@rest...` are mixin implicit variables
        (
            ".m(@a; @rest...) { v: @arguments; w: @rest; }\n.x { .m(1, 2, 3); }\n",
            ".x {\n  v: 1 2 3;\n  w: 2 3;\n}\n",
        ),
        // `@value` / `@key` / `@index` of `each`
        (
            "@l: a b;\n.x { each(@l, { k@{index}: @value; }); }\n",
            ".x {\n  k1: a;\n  k2: b;\n}\n",
        ),
        // `isdefined()` probing a lookup expression (less's evalArgs=false)
        (
            "@dr: { x: 1; };\n.a { v: isdefined(@dr[x]); w: isdefined(@nope[x]); }\n",
            ".a {\n  v: true;\n  w: false;\n}\n",
        ),
        // a body whose guard does not match is not evaluated, so undefined references
        // inside it do not error
        (".m(@a) when (@a > 5) { v: @nope; }\n.a { .m(1); }\n", ""),
        // references inside an uncalled detached ruleset do not error
        ("@dr: { a: @nope; };\n.n { x: 1; }\n", ".n {\n  x: 1;\n}\n"),
    ];
    let path = strict_dir().join("inline.less");
    for (source, expected) in cases {
        let out = compile_file(&path, source, &Options::default())
            .unwrap_or_else(|e| panic!("{source} should not fail: {e:#}"));
        assert_eq!(out.css, expected, "output differs: {source}");
    }
}
