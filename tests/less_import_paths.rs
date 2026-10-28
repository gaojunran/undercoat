//! `paths` semantics for `@import`: mirrors less@4.1.3's `file-manager`
//! (`[""]` plus every `paths` entry joined via node's `path.join(root, href)`,
//! where a leading `/` in the href does not discard the root).

use std::path::PathBuf;

use undercoat::{Options, compile_file};

fn temp_root(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "undercoat-import-paths-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("parts")).expect("mkdir parts");
    std::fs::write(dir.join("parts/vars.less"), ".v { color: red; }\n").expect("write vars");
    dir
}

#[test]
fn absolute_import_resolves_under_paths_root() {
    let root = temp_root("abs");
    let entry = root.join("entry.less");
    let opts = Options {
        paths: vec![root.clone()],
        ..Default::default()
    };
    let abs =
        compile_file(&entry, "@import \"/parts/vars.less\";\n", &opts).expect("absolute import");
    let rel =
        compile_file(&entry, "@import \"parts/vars.less\";\n", &opts).expect("relative import");
    assert_eq!(
        abs.css, rel.css,
        "an absolute href and a relative one must produce the same CSS"
    );
    assert!(abs.css.contains("color: red"), "css = {}", abs.css);
}

#[test]
fn absolute_import_without_extension_gets_less_suffix() {
    let root = temp_root("no-ext");
    let entry = root.join("entry.less");
    let opts = Options {
        paths: vec![root],
        ..Default::default()
    };
    let out =
        compile_file(&entry, "@import \"/parts/vars\";\n", &opts).expect("extension completion");
    assert!(out.css.contains("color: red"), "css = {}", out.css);
}

#[test]
fn absolute_import_without_paths_is_an_error() {
    let root = temp_root("no-paths");
    let entry = root.join("entry.less");
    let err = compile_file(
        &entry,
        "@import \"/parts/vars.less\";\n",
        &Options::default(),
    )
    .expect_err("without paths an absolute href must not hit the filesystem root");
    let msg = format!("{err:#}");
    assert!(msg.contains("import"), "err = {msg}");
}
