//! 语义严格性：less@4.1.3 会报错的输入，我们也要报错，不能静默降级成错误产物。
//!
//! 每个 `tests/fixtures/strict/<case>.less` 配一个 `<case>.err`，内容是 less 4.1.3
//! 对同一输入的报错文案（去掉 `NameError:` 之类的类型前缀）。
//!
//! 同时锁定「不该报错的必须保持安静」：guard 未命中、`isdefined()`、命名空间
//! 内定义、`each`、detached ruleset 调用等既有正确场景。

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
            .unwrap_or_else(|e| panic!("缺少期望报错 {}: {e}", expected_path.display()));
        let source = std::fs::read_to_string(&path).expect("read less");
        let err = match compile_file(&path, &source, &Options::default()) {
            Ok(out) => panic!("{} 应当报错，实际输出:\n{}", path.display(), out.css),
            Err(e) => format!("{e:#}"),
        };
        assert_eq!(
            err.trim(),
            expected.trim(),
            "报错文案与 less@4.1.3 不一致: {}",
            path.display()
        );
        checked += 1;
    }
    assert!(checked >= 8, "strict 用例数量异常: {checked}");
}

#[test]
fn legit_lookups_stay_silent() {
    // less 在这些输入上都不报错；改动前它们就正确，不能被「严格化」误伤。
    let cases = [
        // guard 未命中的 mixin 调用：静默
        (".m(@a) when (@a > 5) { x: @a; }\n.n { .m(1); }\n", ""),
        // 命名空间内定义 + 命名空间调用
        (
            "#ns { .m() { x: 1; } }\n.a { #ns > .m(); }\n",
            ".a {\n  x: 1;\n}\n",
        ),
        // `isdefined()` 探测未定义变量
        (".a { v: isdefined(@nope); }\n", ".a {\n  v: false;\n}\n"),
        // detached ruleset 调用
        ("@dr: { a: 1; };\n.n { @dr(); }\n", ".n {\n  a: 1;\n}\n"),
        // `each` 语句级展开
        (
            "each(1 2, { .x@{value} { y: @value; } });\n",
            ".x1 {\n  y: 1;\n}\n.x2 {\n  y: 2;\n}\n",
        ),
        // 惰性求值：后声明的变量在规则体里可见
        (".a { v: @later; }\n@later: 1;\n", ".a {\n  v: 1;\n}\n"),
        // `@@name` 间接引用
        (
            "@name: 'x';\n@x: 1;\n.a { v: @@name; }\n",
            ".a {\n  v: 1;\n}\n",
        ),
        // `@arguments` / `@rest...` 是 mixin 的隐式变量
        (
            ".m(@a; @rest...) { v: @arguments; w: @rest; }\n.x { .m(1, 2, 3); }\n",
            ".x {\n  v: 1 2 3;\n  w: 2 3;\n}\n",
        ),
        // `each` 的 `@value` / `@key` / `@index`
        (
            "@l: a b;\n.x { each(@l, { k@{index}: @value; }); }\n",
            ".x {\n  k1: a;\n  k2: b;\n}\n",
        ),
        // `isdefined()` 探测查找式（less 的 evalArgs=false）
        (
            "@dr: { x: 1; };\n.a { v: isdefined(@dr[x]); w: isdefined(@nope[x]); }\n",
            ".a {\n  v: true;\n  w: false;\n}\n",
        ),
        // guard 未命中的体不求值，体内的未定义引用不报错
        (".m(@a) when (@a > 5) { v: @nope; }\n.a { .m(1); }\n", ""),
        // 未调用的 detached ruleset 体内的引用不报错
        ("@dr: { a: @nope; };\n.n { x: 1; }\n", ".n {\n  x: 1;\n}\n"),
    ];
    let path = strict_dir().join("inline.less");
    for (source, expected) in cases {
        let out = compile_file(&path, source, &Options::default())
            .unwrap_or_else(|e| panic!("{source} 不应报错: {e:#}"));
        assert_eq!(out.css, expected, "产物不一致: {source}");
    }
}
