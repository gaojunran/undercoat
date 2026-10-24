//! SCSS sourcemap 的**逐行锚点**回归：期望值来自 dart-sass 1.35.2（WDT 同款）。
//!
//! 只对拍「覆盖率」——dart 覆盖的每条生成行，我们也必须覆盖，且首段的源行列相同。
//! **不**对拍整份 JSON：dart 对一条声明只写一个 segment，我们另外记属性名/值的
//! 起止，segment 密度是实现细节（锁死它会把实现细节写成规范）。
//!
//! 期望文件由 `scripts/style-diff/scss-map-oracle.cjs` 生成（CI 不依赖 node）。

use std::path::{Path, PathBuf};

use undercoat::{Options, compile_file};

/// 一条映射：`(生成列, 源文件下标, 源行, 源列)`；四元组缺失时只有生成列。
type Seg = (u32, Option<(u32, u32, u32)>);

fn decode(mappings: &str) -> Vec<Vec<Seg>> {
    const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let (mut file, mut line, mut col) = (0i64, 0i64, 0i64);
    mappings
        .split(';')
        .map(|row| {
            let mut gen_col = 0i64;
            let mut segs = Vec::new();
            for seg in row.split(',').filter(|s| !s.is_empty()) {
                let mut values = Vec::new();
                let (mut shift, mut value) = (0u32, 0i64);
                for byte in seg.bytes() {
                    let digit = B64.iter().position(|&b| b == byte).expect("vlq 字符") as i64;
                    value |= (digit & 0x1f) << shift;
                    shift += 5;
                    if digit & 0x20 == 0 {
                        let neg = value & 1 != 0;
                        value >>= 1;
                        values.push(if neg { -value } else { value });
                        (shift, value) = (0, 0);
                    }
                }
                gen_col += values[0];
                if values.len() >= 4 {
                    file += values[1];
                    line += values[2];
                    col += values[3];
                    segs.push((gen_col as u32, Some((file as u32, line as u32, col as u32))));
                } else {
                    segs.push((gen_col as u32, None));
                }
            }
            segs
        })
        .collect()
}

fn map_of(compiled: &str) -> serde_json::Value {
    serde_json::from_str(compiled).expect("parse source map")
}

/// `dart-sass` 的首段（`gen_col` 最小的带源 segment）。
fn first_source(segs: &[Seg]) -> Option<(u32, u32, u32)> {
    segs.iter().find_map(|(_, src)| *src)
}

/// UTF-16 code unit 长度（sourcemap v3 的列语义）。
fn utf16_len(text: &str) -> usize {
    text.chars().map(char::len_utf16).sum()
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures-scss-map")
        .join(name)
}

fn compile_with_map(dir: &Path) -> (String, serde_json::Value) {
    let entry = dir.join("main.scss");
    let source = std::fs::read_to_string(&entry).expect("read fixture");
    let out = compile_file(
        &entry,
        &source,
        &Options {
            source_map: true,
            project_root: Some(dir.to_string_lossy().into_owned()),
            ..Default::default()
        },
    )
    .expect("compile fixture");
    (out.css, map_of(&out.map.expect("source map")))
}

#[test]
fn anchors_cover_every_dart_sass_mapped_line() {
    let dir = fixture("anchors");
    let (css, map) = compile_with_map(&dir);
    let expected_css = std::fs::read_to_string(dir.join("main.css")).expect("main.css");
    assert_eq!(
        css,
        expected_css,
        "逐行锚点的改动不允许改变 CSS 文本（{}）",
        dir.display()
    );

    let expected =
        map_of(&std::fs::read_to_string(dir.join("main.map.json")).expect("main.map.json"));
    let ours = decode(map["mappings"].as_str().expect("mappings"));
    let reference = decode(expected["mappings"].as_str().expect("mappings"));
    let lines: Vec<&str> = css.split('\n').collect();
    // 末尾全未映射的生成行不写 `;`（sourcemap 允许省略），所以行数只可能更少。
    assert!(ours.len() <= lines.len(), "mappings 行数不应超过产物行数");

    let mut checked = 0;
    for (i, ref_segs) in reference.iter().enumerate() {
        let Some((ref_line, ref_col)) = first_source(ref_segs).map(|(_, l, c)| (l, c)) else {
            continue;
        };
        let ours_first = ours
            .get(i)
            .and_then(|segs| first_source(segs))
            .unwrap_or_else(|| panic!("L{} 缺映射: {:?}", i + 1, lines.get(i)));
        assert_eq!(
            (ours_first.1, ours_first.2),
            (ref_line, ref_col),
            "L{} 的锚点与 dart-sass 不一致: {:?}",
            i + 1,
            lines[i]
        );
        checked += 1;
    }
    // 用例本身要够大：多行选择器列表 / 插值选择器 / 注释续行 / keyframes / extend
    assert!(
        checked >= 25,
        "用例覆盖的生成行太少（{checked}）：多行选择器列表 / 插值 / 注释 / keyframes / extend 都应覆盖到"
    );
}

#[test]
fn map_is_structurally_valid() {
    let dir = fixture("anchors");
    let (css, map) = compile_with_map(&dir);
    let sources: Vec<&str> = map["sources"]
        .as_array()
        .expect("sources")
        .iter()
        .map(|s| s.as_str().expect("source"))
        .collect();
    assert_eq!(
        sources,
        ["main.scss"],
        "sources 应是相对 project_root 的形态"
    );

    // sourcesContent 与 sources 一一对应且不缺
    let contents: Vec<&str> = map["sourcesContent"]
        .as_array()
        .expect("sourcesContent")
        .iter()
        .map(|c| c.as_str().expect("content"))
        .collect();
    assert_eq!(contents.len(), sources.len(), "sourcesContent 必须完整");
    for (source, content) in sources.iter().zip(&contents) {
        assert!(content.contains("$name: accent;"), "{source} 原文不完整");
    }

    let lines: Vec<&str> = css.split('\n').collect();
    let source_lines: Vec<Vec<&str>> = contents.iter().map(|c| c.split('\n').collect()).collect();
    let mappings = decode(map["mappings"].as_str().expect("mappings"));
    assert!(
        mappings.len() <= lines.len(),
        "mappings 行数不应超过产物行数"
    );
    let mut total = 0;
    for (i, segs) in mappings.iter().enumerate() {
        let mut last_col = 0;
        for (gen_col, src) in segs {
            assert!(*gen_col >= last_col, "L{} 的 segment 列未递增", i + 1);
            assert!(
                *gen_col as usize <= utf16_len(lines[i]),
                "L{} 生成列 {} 越界（行宽 {}）",
                i + 1,
                gen_col,
                utf16_len(lines[i])
            );
            last_col = *gen_col;
            if let Some((file, line, col)) = src {
                let file_lines = source_lines
                    .get(*file as usize)
                    .unwrap_or_else(|| panic!("L{} 的源文件下标 {file} 越界", i + 1));
                let text = file_lines
                    .get(*line as usize)
                    .unwrap_or_else(|| panic!("L{} 的源行 {line} 越界", i + 1));
                assert!(
                    *col as usize <= utf16_len(text),
                    "L{} 的源列 {col} 越界（源行宽 {}）",
                    i + 1,
                    utf16_len(text)
                );
            }
            total += 1;
        }
    }
    assert!(total > 0, "map 不能是空的");
    assert!(map["names"].as_array().expect("names").is_empty());
    assert_eq!(map["version"], 3);
}

/// 多行选择器列表：每个输出行都要有自己的 segment（dart 给所有行同一个源位置）。
#[test]
fn selector_list_continuation_lines_are_mapped() {
    let dir = temp_dir("sel-cont");
    write(&dir, "main.scss", ".a,\n.b,\n.c > .d {\n  color: red;\n}\n");
    let (css, map) = compile_with_map(&dir);
    assert_eq!(css, ".a,\n.b,\n.c > .d {\n  color: red;\n}");
    let mappings = decode(map["mappings"].as_str().expect("mappings"));
    for (i, expected_col) in [(0usize, 0u32), (1, 0), (2, 0)] {
        let first =
            first_source(&mappings[i]).unwrap_or_else(|| panic!("选择器第 {} 行没有映射", i + 1));
        assert_eq!((first.1, first.2), (0, expected_col), "第 {} 行锚点", i + 1);
    }
    std::fs::remove_dir_all(&dir).ok();
}

/// 插值选择器：整条列表锚到源码里「列表起点」（也就是 `#{` 所在的行）。
#[test]
fn interpolated_selector_line_is_mapped() {
    let dir = temp_dir("sel-interp");
    write(
        &dir,
        "main.scss",
        "$n: q;\n.#{$n}-a,\n.#{$n}-b { color: red; }\n",
    );
    let (css, map) = compile_with_map(&dir);
    assert_eq!(css, ".q-a,\n.q-b {\n  color: red;\n}");
    let mappings = decode(map["mappings"].as_str().expect("mappings"));
    for i in [0usize, 1] {
        let (line, col) = first_source(&mappings[i])
            .map(|(_, l, c)| (l, c))
            .expect("映射");
        assert_eq!(
            (line, col),
            (1, 0),
            "插值选择器第 {} 行应锚到 `#{{` 所在行",
            i + 1
        );
    }
    std::fs::remove_dir_all(&dir).ok();
}

/// 块注释的每一行（含 `/*!` 许可证块的续行）都要有 segment。
#[test]
fn comment_continuation_lines_are_mapped() {
    let dir = temp_dir("comment-map");
    write(
        &dir,
        "main.scss",
        "/*!\n * one\n * two\n */\n.a {\n  color: red;\n}\n",
    );
    let (css, map) = compile_with_map(&dir);
    assert_eq!(css, "/*!\n * one\n * two\n */\n.a {\n  color: red;\n}");
    let mappings = decode(map["mappings"].as_str().expect("mappings"));
    assert!(mappings.len() >= 4, "注释的每一行都要有 segment");
    for segs in mappings.iter().take(4) {
        let (line, col) = first_source(segs)
            .map(|(_, l, c)| (l, c))
            .expect("注释行映射");
        assert_eq!((line, col), (0, 0), "注释续行都应锚到 `/*` 起点");
    }
    std::fs::remove_dir_all(&dir).ok();
}

/// at-rule 锚到 `@` 所在列（`name_span` 指向名字，序列化时左移一列）。
#[test]
fn at_rule_maps_to_the_at_sign() {
    let dir = temp_dir("at-map");
    write(
        &dir,
        "main.scss",
        ".w {\n  @media (min-width: 1px) {\n    color: red;\n  }\n}\n",
    );
    let (css, map) = compile_with_map(&dir);
    assert_eq!(
        css,
        "@media (min-width: 1px) {\n  .w {\n    color: red;\n  }\n}"
    );
    let mappings = decode(map["mappings"].as_str().expect("mappings"));
    let (line, col) = first_source(&mappings[0])
        .map(|(_, l, c)| (l, c))
        .expect("映射");
    assert_eq!((line, col), (1, 2), "`@media` 应锚到第 2 行的 `@`（列 2）");
    std::fs::remove_dir_all(&dir).ok();
}

/// keyframes 的选择器列表不换行（dart 用 `, ` 连接），所以只有一段。
#[test]
fn keyframe_selector_list_stays_on_one_line() {
    let dir = temp_dir("kf-map");
    write(
        &dir,
        "main.scss",
        "@keyframes k {\n  0%,\n  50% {\n    opacity: 0;\n  }\n}\n",
    );
    let (css, map) = compile_with_map(&dir);
    assert_eq!(css, "@keyframes k {\n  0%, 50% {\n    opacity: 0;\n  }\n}");
    let mappings = decode(map["mappings"].as_str().expect("mappings"));
    let (line, col) = first_source(&mappings[1])
        .map(|(_, l, c)| (l, c))
        .expect("映射");
    assert_eq!((line, col), (1, 2), "keyframe 选择器锚到列表起点");
    assert!(mappings.len() <= 5, "mappings 行数不应超过产物行数");
    std::fs::remove_dir_all(&dir).ok();
}

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "undercoat-map-{tag}-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("mkdir temp");
    dir
}

fn write(dir: &Path, name: &str, text: &str) {
    std::fs::write(dir.join(name), text).expect("write fixture");
}
