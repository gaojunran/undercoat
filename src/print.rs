//! 带源映射的 CSS 打印器。
//!
//! 打印器是 sourcemap 的唯一出口：每个从源里来的 token 都必须走 [`Printer::token`]，
//! 生成的分隔符（缩进、换行、逗号）走 [`Printer::raw`]。这样「能不能反查原始位置」
//! 由打印时是否带 span 决定，而不是事后修补。
//!
//! 多文件（@import 内联）通过 [`Printer::add_file`] 登记、[`Printer::set_segments`]
//! 给出「合并串 → 各文件」的段表：token 的 span 先从合并串坐标二分回
//! `(file, 原始偏移)`，再按该文件的行表换算。打印器不持有外部字符串的借用。
//!
//! 打印器只追加、不回改：extend 之类的收尾改写发生在 IR 上（`doc`），不在这里动已写出的文本。

use oxc_css_parser::pos::Span;

use crate::sourcemap::{Builder, Mapping};
use crate::{LineIndex, Options};

struct SourceFile {
    src: String,
    index: LineIndex,
    /// 磁盘原文（`sourcesContent`）；单文件场景保持 `None`，不改变既有 map 字节。
    content: Option<String>,
}

/// 合并串的一段：`[start, end)` 来自 `file` 的 `orig_start` 起。
///
/// `@import` 文本内联后，打印器收到的 span 全是合并串坐标，收尾时先经这张表
/// 二分回 `(file, 原始偏移)`，再按该文件的行表换算行列。
#[derive(Debug, Clone, Copy)]
pub(crate) struct SourceSegment {
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) file: u32,
    pub(crate) orig_start: usize,
}

/// 一次源 token 写入：先记输出偏移，收尾时换算成行列。
struct Pending {
    offset: usize,
    file: u32,
    span: Span,
}

pub(crate) struct Printer {
    sources: Vec<String>,
    files: Vec<SourceFile>,
    out: String,
    pending: Vec<Pending>,
    current: u32,
    want_map: bool,
    segments: Vec<SourceSegment>,
}

impl Printer {
    pub(crate) fn new(file: &str, src: &str, want_map: bool) -> Self {
        Self {
            sources: vec![file.to_string()],
            files: vec![SourceFile {
                src: src.to_string(),
                index: LineIndex::new(src),
                content: None,
            }],
            out: String::new(),
            pending: Vec::new(),
            current: 0,
            want_map,
            segments: Vec::new(),
        }
    }

    /// 登记一个附加源文件（@import 内联），返回 file id。
    pub(crate) fn add_file(&mut self, file: &str, src: &str) -> u32 {
        if let Some(i) = self.sources.iter().position(|s| s == file) {
            return i as u32;
        }
        self.sources.push(file.to_string());
        self.files.push(SourceFile {
            src: src.to_string(),
            index: LineIndex::new(src),
            content: None,
        });
        (self.sources.len() - 1) as u32
    }

    /// 登记某文件的磁盘原文（多文件 map 的 `sourcesContent`）。
    pub(crate) fn set_content(&mut self, file: u32, content: &str) {
        if let Some(f) = self.files.get_mut(file as usize) {
            f.content = Some(content.to_string());
        }
    }

    /// 合并串 → 源文件的段表（升序、互不重叠、覆盖整个合并串）。
    pub(crate) fn set_segments(&mut self, segments: Vec<SourceSegment>) {
        self.segments = segments;
    }

    /// 纯生成文本（缩进、分隔符、换行），不产生映射。
    pub(crate) fn raw(&mut self, s: &str) {
        self.out.push_str(s);
    }

    /// 源 token（使用当前文件）：推进输出并记录一条映射。
    pub(crate) fn token(&mut self, s: &str, span: Span) {
        let file = self.current;
        self.token_in(file, s, span);
    }

    /// 指定文件的源 token。
    pub(crate) fn token_in(&mut self, file: u32, s: &str, span: Span) {
        if self.want_map && !s.is_empty() && self.files.get(file as usize).is_some() {
            self.pending.push(Pending {
                offset: self.out.len(),
                file,
                span,
            });
        }
        self.out.push_str(s);
    }

    pub(crate) fn newline(&mut self) {
        self.out.push('\n');
    }

    pub(crate) fn indent(&mut self, level: usize) {
        for _ in 0..level {
            self.out.push_str("  ");
        }
    }

    /// 收尾：返回 (css, map)。
    pub(crate) fn finish(mut self, opts: &Options) -> (String, Option<String>) {
        let out = std::mem::take(&mut self.out);
        if !opts.source_map || self.pending.is_empty() {
            return (out, None);
        }
        let line_starts = line_starts(&out);
        let mut map = Builder::new();
        for (i, source) in self.sources.iter().enumerate() {
            let id = map.add_source(source);
            if let Some(content) = self.files.get(i).and_then(|f| f.content.as_deref()) {
                map.set_content(id, content);
            }
        }
        let mut mapped_line = None;
        for p in &self.pending {
            let offset = p.offset;
            let (src_file, src_offset) =
                self.locate(p.span.start).unwrap_or((p.file, p.span.start));
            let file = self
                .files
                .get(src_file as usize)
                .or_else(|| self.files.first());
            let Some(file) = file else {
                continue;
            };
            let (line, col) = file.index.line_col(src_offset, &file.src);
            let (gen_line, gen_col) = line_col(&line_starts, offset);
            // 行首（col 0）补一条映射：IDE 按整行/行首查询时也能反查
            if mapped_line != Some(gen_line) {
                if gen_col > 0 {
                    map.add(Mapping {
                        gen_line,
                        gen_col: 0,
                        src_file,
                        src_line: line,
                        src_col: col,
                    });
                }
                mapped_line = Some(gen_line);
            }
            map.add(Mapping {
                gen_line,
                gen_col,
                src_file,
                src_line: line,
                src_col: col,
            });
        }
        let root = opts.project_root.as_deref();
        let json = map.to_json(None, |s| rewrite_source(s, root));
        (out, Some(json))
    }

    /// 合并串偏移 → `(文件, 原始偏移)`；无段表（单文件）时返回 `None`，沿用旧路径。
    fn locate(&self, merged: usize) -> Option<(u32, usize)> {
        let i = self.segments.partition_point(|s| s.start <= merged);
        let segment = self.segments.get(i.checked_sub(1)?)?;
        (merged < segment.end)
            .then(|| (segment.file, segment.orig_start + (merged - segment.start)))
    }
}

fn line_starts(text: &str) -> Vec<usize> {
    let mut starts = vec![0];
    for (i, b) in text.bytes().enumerate() {
        if b == b'\n' {
            starts.push(i + 1);
        }
    }
    starts
}

fn line_col(starts: &[usize], offset: usize) -> (u32, u32) {
    let line = starts.partition_point(|&s| s <= offset).saturating_sub(1);
    let col = offset.saturating_sub(starts[line]);
    (line as u32, col as u32)
}

/// `sources` 重写：命中 projectRoot 前缀则改成项目相对（WDT `path.posix.relative` 语义）。
fn rewrite_source(source: &str, project_root: Option<&str>) -> String {
    let Some(root) = project_root else {
        return source.to_string();
    };
    let src = source.replace('\\', "/");
    let root = root.replace('\\', "/");
    let root = root.trim_end_matches('/');
    match src.strip_prefix(root) {
        Some(rest) => rest.trim_start_matches('/').to_string(),
        None => src,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracks_offsets() {
        let mut p = Printer::new("a.less", ".a {}\n.b {}", true);
        p.raw(".a");
        p.newline();
        p.indent(1);
        p.token(".b", Span { start: 7, end: 9 });
        let (css, _) = p.finish(&Options::default());
        assert_eq!(css, ".a\n  .b");
    }

    #[test]
    fn token_records_mapping() {
        let src = ".a {}";
        let mut p = Printer::new("a.less", src, true);
        p.token(".a", Span { start: 0, end: 2 });
        let (css, map) = p.finish(&Options {
            source_map: true,
            ..Default::default()
        });
        assert_eq!(css, ".a");
        let map: serde_json::Value = serde_json::from_str(&map.unwrap()).unwrap();
        assert_eq!(map["version"], 3);
        assert_eq!(map["sources"][0], "a.less");
        assert_eq!(map["mappings"], "AAAA");
    }
}
