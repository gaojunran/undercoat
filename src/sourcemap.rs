//! sourcemap v3 生成（VLQ 编码）。less/sass 两个引擎共用同一套 builder。

use std::collections::BTreeMap;

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn vlq(v: i64, out: &mut String) {
    let mut v = if v < 0 {
        ((-v) as u64) << 1 | 1
    } else {
        (v as u64) << 1
    };
    loop {
        let mut digit = (v & 0x1f) as usize;
        v >>= 5;
        if v > 0 {
            digit |= 0x20;
        }
        out.push(B64[digit] as char);
        if v == 0 {
            break;
        }
    }
}

/// 一条映射：生成位置（0-based 行列）+ 源位置（0-based 行列）+ 源文件下标。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Mapping {
    pub(crate) gen_line: u32,
    pub(crate) gen_col: u32,
    pub(crate) src_file: u32,
    pub(crate) src_line: u32,
    pub(crate) src_col: u32,
}

#[derive(Debug, Default)]
pub(crate) struct Builder {
    pub(crate) sources: Vec<String>,
    /// 与 `sources` 平行；全为 `None` 时 JSON 不输出 `sourcesContent`（单文件 map 字节不变）。
    contents: Vec<Option<String>>,
    mappings: BTreeMap<(u32, u32), (u32, u32, u32)>,
}

impl Builder {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn add_source(&mut self, path: &str) -> u32 {
        if let Some(i) = self.sources.iter().position(|s| s == path) {
            return i as u32;
        }
        self.sources.push(path.to_string());
        self.contents.push(None);
        (self.sources.len() - 1) as u32
    }

    /// 登记某个 source 的原文（`sourcesContent`）。
    pub(crate) fn set_content(&mut self, index: u32, content: &str) {
        if let Some(slot) = self.contents.get_mut(index as usize) {
            *slot = Some(content.to_string());
        }
    }

    pub(crate) fn add(&mut self, m: Mapping) {
        // 同一生成位置只保留第一条（后到的通常是空白补齐，不覆盖实质映射）
        self.mappings
            .entry((m.gen_line, m.gen_col))
            .or_insert((m.src_file, m.src_line, m.src_col));
    }

    fn encode_mappings(&self) -> String {
        let mut out = String::new();
        let mut last_gen_line = 0u32;
        let mut prev_gen_col = 0i64;
        let mut prev_src_file = 0i64;
        let mut prev_src_line = 0i64;
        let mut prev_src_col = 0i64;
        let mut first_in_line = true;

        for (&(gen_line, gen_col), &(src_file, src_line, src_col)) in &self.mappings {
            while last_gen_line < gen_line {
                out.push(';');
                last_gen_line += 1;
                prev_gen_col = 0;
                first_in_line = true;
            }
            if !first_in_line {
                out.push(',');
            }
            first_in_line = false;
            vlq(gen_col as i64 - prev_gen_col, &mut out);
            prev_gen_col = gen_col as i64;
            vlq(src_file as i64 - prev_src_file, &mut out);
            prev_src_file = src_file as i64;
            vlq(src_line as i64 - prev_src_line, &mut out);
            prev_src_line = src_line as i64;
            vlq(src_col as i64 - prev_src_col, &mut out);
            prev_src_col = src_col as i64;
        }
        out
    }

    /// 序列化成 sourcemap v3 JSON（字段序与 less/sass 的产出一致：
    /// version / sources / names / mappings，无 `file`；有原文时附 `sourcesContent`）。
    pub(crate) fn to_json(
        &self,
        source_root: Option<&str>,
        rewrite: impl Fn(&str) -> String,
    ) -> String {
        let sources: Vec<String> = self.sources.iter().map(|s| rewrite(s)).collect();
        let mut obj = serde_json::Map::new();
        obj.insert("version".into(), serde_json::json!(3));
        if let Some(root) = source_root {
            obj.insert("sourceRoot".into(), serde_json::json!(root));
        }
        obj.insert("sources".into(), serde_json::json!(sources));
        if self.contents.iter().any(Option::is_some) {
            obj.insert("sourcesContent".into(), serde_json::json!(self.contents));
        }
        obj.insert("names".into(), serde_json::json!(Vec::<String>::new()));
        obj.insert("mappings".into(), serde_json::json!(self.encode_mappings()));
        serde_json::to_string(&serde_json::Value::Object(obj)).expect("serialize sourcemap")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vlq_known_values() {
        // source-map 规范示例：0 -> "A", 1 -> "C", -1 -> "D", 16 -> "gB"
        let mut s = String::new();
        vlq(0, &mut s);
        assert_eq!(s, "A");
        s.clear();
        vlq(1, &mut s);
        assert_eq!(s, "C");
        s.clear();
        vlq(-1, &mut s);
        assert_eq!(s, "D");
        s.clear();
        vlq(16, &mut s);
        assert_eq!(s, "gB");
    }

    #[test]
    fn encode_two_lines() {
        let mut b = Builder::new();
        let f = b.add_source("a.less");
        b.add(Mapping {
            gen_line: 0,
            gen_col: 0,
            src_file: f,
            src_line: 0,
            src_col: 0,
        });
        b.add(Mapping {
            gen_line: 1,
            gen_col: 2,
            src_file: f,
            src_line: 2,
            src_col: 4,
        });
        assert_eq!(b.encode_mappings(), "AAAA;EAEI");
    }
}
