//! S3.2 golden 对拍：`@forward` 与 `@use ... with`。
//!
//! 每个目录是一份独立用例：`main.scss` 是入口，`main.css` 是 dart-sass 1.93.2 的
//! expanded 产物（去掉末尾换行），逐字节断言。

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
            .unwrap_or_else(|e| panic!("缺少期望产物 {}: {e}", expected_path.display()));
        let source = std::fs::read_to_string(&path).expect("read scss");
        let out = compile_file(&path, &source, &Options::default())
            .unwrap_or_else(|e| panic!("{}: {e:#}", path.display()));
        assert_eq!(
            out.css,
            expected,
            "产物与 dart-sass 1.93.2 不一致: {}",
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
            "缺少 fixture {required}: {names:?}"
        );
    }
    assert_eq!(checked, 10, "S3.2 fixtures 数量异常: {checked}");
}
