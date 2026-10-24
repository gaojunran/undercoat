//! `@import` 文本内联后的多源 sourcemap 断言。

use std::path::{Path, PathBuf};

use undercoat::{Options, compile_file};

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn vlq(segment: &str) -> Vec<i64> {
    let (mut shift, mut value, mut out) = (0u32, i64::from(0), Vec::new());
    for ch in segment.bytes() {
        let digit = B64.iter().position(|&b| b == ch).expect("vlq 字符") as i64;
        let cont = digit & 0x20 != 0;
        value |= (digit & 0x1f) << shift;
        shift += 5;
        if !cont {
            let neg = value & 1 != 0;
            value >>= 1;
            out.push(if neg { -value } else { value });
            value = 0;
            shift = 0;
        }
    }
    out
}

/// 解码 mappings：每行第一条映射的 `(source, line, col)`。
fn line_mappings(map: &serde_json::Value) -> Vec<Option<(usize, u32, u32)>> {
    let (mut file, mut line, mut col) = (0i64, 0i64, 0i64);
    map["mappings"]
        .as_str()
        .expect("mappings")
        .split(';')
        .map(|row| {
            let mut first = None;
            for seg in row.split(',').filter(|s| !s.is_empty()) {
                let v = vlq(seg);
                if v.len() >= 4 {
                    file += v[1];
                    line += v[2];
                    col += v[3];
                    first.get_or_insert((file as usize, line as u32, col as u32));
                }
            }
            first
        })
        .collect()
}

fn generated_line(css: &str, selector: &str) -> usize {
    let head = format!("{selector} {{");
    css.lines()
        .position(|l| l.starts_with(&head))
        .unwrap_or_else(|| panic!("产物里没有 {selector}:\n{css}"))
}

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("undercoat-{tag}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir temp");
    dir
}

fn write(dir: &Path, name: &str, text: &str) {
    std::fs::write(dir.join(name), text).expect("write fixture");
}

fn compile(dir: &Path, entry: &str) -> (String, serde_json::Value) {
    let path = dir.join(entry);
    let source = std::fs::read_to_string(&path).expect("read entry");
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
    (out.css, serde_json::from_str(&map).expect("parse map"))
}

#[test]
fn extend_generated_selector_keeps_source_line() {
    let dir = temp_dir("extend-map");
    // 多源（b.less 被内联）：extend 追加的选择器要映射回 b.less 的 extend 行
    write(&dir, "main.less", "@import \"b.less\";\n");
    write(&dir, "b.less", ".a { color: red; }\n.b { &:extend(.a); }\n");

    let (css, map) = compile(&dir, "main.less");
    assert_eq!(css, ".a,\n.b {\n  color: red;\n}\n", "CSS = {css}");
    let mappings = line_mappings(&map);
    // 原选择器位置不变
    assert_eq!(mappings[0].expect("映射"), (1, 0, 0));
    // `.b` 是 extend 追加出来的选择器，映射到含 `:extend` 的源行（b.less 0-based 第 1 行）。
    assert_eq!(mappings[1].expect("映射"), (1, 1, 0));

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn import_maps_back_to_each_source_file() {
    let dir = temp_dir("multi-map");
    write(
        &dir,
        "main.less",
        ".a { x: 1 }\n@import \"b.less\";\n@import (multiple) \"b.less\";\n.c { z: 3 }\n",
    );
    write(&dir, "b.less", ".b1 { b: 2 }\n@import \"c.less\";\n");
    write(&dir, "c.less", ".c1 { c: 3 }\n");

    let (css, map) = compile(&dir, "main.less");
    let sources: Vec<&str> = map["sources"]
        .as_array()
        .expect("sources")
        .iter()
        .map(|s| s.as_str().expect("source"))
        .collect();
    assert_eq!(sources, ["main.less", "b.less", "c.less"], "按首次出现顺序");

    let contents = map["sourcesContent"].as_array().expect("sourcesContent");
    assert_eq!(contents.len(), sources.len());
    assert!(contents[1].as_str().expect("b 原文").contains(".b1"));

    let mappings = line_mappings(&map);
    let at = |selector: &str| mappings[generated_line(&css, selector)].expect("映射");

    // 宿主文件规则仍在宿主坐标
    assert_eq!(at(".a"), (0, 0, 0));
    assert_eq!(at(".c"), (0, 3, 0));
    // 导入文件规则映射回导入文件自己的行（`.b1` 在 b.less 第 1 行）
    assert_eq!(at(".b1"), (1, 0, 0));
    // 嵌套导入同理
    assert_eq!(at(".c1"), (2, 0, 0));
    // `(multiple)` 重复导入：两处 `.b1` 都映射到同一 source 的同一行
    let b1: Vec<usize> = css
        .lines()
        .enumerate()
        .filter(|(_, l)| l.starts_with(".b1"))
        .map(|(i, _)| i)
        .collect();
    assert_eq!(b1.len(), 2, "产物应有两份 .b1:\n{css}");
    for line in b1 {
        assert_eq!(mappings[line].expect("映射"), (1, 0, 0));
    }

    std::fs::remove_dir_all(&dir).ok();
}
