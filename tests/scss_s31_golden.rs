//! S3.1 golden 对拍：真实小程序语料里的三类纯 CSS 打印缺陷
//! （keyframe 选择器列表单行、块注释续行重排、自定义属性值里的字符串归一双引号），
//! 外加 `@extend` 单段扩展者的织入与嵌套属性（`prop: { sub: v }`）。
//!
//! 每个目录是一份独立用例：`main.scss` 是入口，`main.css` 是 dart-sass 1.35.2 的
//! expanded 产物，逐字节断言。

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
            .unwrap_or_else(|e| panic!("缺少期望产物 {}: {e}", expected_path.display()));
        let source = std::fs::read_to_string(&path).expect("read scss");
        let out = compile_file(&path, &source, &Options::default())
            .unwrap_or_else(|e| panic!("{}: {e:#}", path.display()));
        assert_eq!(
            out.css,
            expected,
            "产物与 dart-sass 1.35.2 不一致: {}",
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
            "缺少 fixture {required}: {names:?}"
        );
    }
    assert_eq!(checked, 6, "S3.1 fixtures 数量异常: {checked}");
}

/// 缺陷 A 的细节：keyframe 选择器列表恒为单行 `, ` 分隔，普通选择器仍保留源码换行。
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
    assert!(css.contains(".a,\n.b {"), "普通选择器要保留换行: {css}");
}

/// 嵌套属性块里只允许声明与 `@include`/控制流：其它 at-rule 报 dart 的文案。
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
    let err = compile_file(&path, &source, &Options::default()).expect_err("应当报错");
    assert_eq!(err.message, "This at-rule is not allowed here.");
}

/// 缺陷 F：空白游程压成「游程的最后一个字符」，续行缩进按字符平移并保留 tab。
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
    // 游程 `空格 + tab` → 留 tab；`tab + 空格` → 留空格；纯空白/纯 tab 同理。
    assert!(css.contains("  --a:\t#111;"), "{css:?}");
    assert!(css.contains("  --b: #222;"), "{css:?}");
    assert!(css.contains("  --c: #333;"), "{css:?}");
    assert!(css.contains("  --d:\t#444;"), "{css:?}");
    // 续行：输出缩进 2 空格 + max(0, 源缩进 2 tab − 声明列 1) = 1 个 tab。
    assert!(css.contains("  --e:\n  \t#555;"), "{css:?}");
}

/// 缺陷 B 的细节：制表符按字符保留（续行 = 输出缩进 + 源缩进 − 注释列）。
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
    // 2 空格输出缩进 + (源续行 2 tab − 注释列 1) → 保留 1 个 tab。
    assert!(css.contains("  /* note\n  \ttransform: none; */"), "{css}");
    // 空行是真空行，不留缩进空格。
    assert!(css.contains("/* top\n\n  below */"), "{css}");
}
