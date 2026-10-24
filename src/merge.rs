//! `@import` 内联后的合并串与段表。
//!
//! less 与 scss 都把 `@import` 展开成一段**合并文本**：后续解析、求值、打印只看
//! 合并串坐标；出错定位与 sourcemap 再经段表回到「哪个文件 + 文件内偏移」。
//! 本模块只负责这份映射，不关心语法。
//!
//! # 坐标契约
//!
//! - [`Seg`] 描述合并串 `[start, end)` 来自 `files[file]` 的 `[orig, orig + len)`，
//!   即 `end - start == 段内覆盖的原文长度`。段表按 `start` 升序、互不重叠；
//!   [`Merged::normalize`] 会把相邻且同文件连续的段并成一段。
//! - [`Merged::locate_exact`] 只做**精确**命中：偏移不落在任何段内时返回 `None`，
//!   越界怎么回退由调用方决定（less 回退到入口文件同偏移，scss 回退到最后一个段）。
//! - 生成文本（包装 `@media` / `(reference)`、`@use` 合成语句、插值插入）也登记成段，
//!   映射到一个固定原始偏移，因此段表始终覆盖整段合并文本。
//!
//! # 为什么 less 有两个文本
//!
//! less 在解析前对每个文件做 `split_signed_dimensions` 词法修补（把 `1px+2` 拆成
//! 独立 token），所以 span 指向修补后的 [`File::text`]；而 sourcemap 的
//! `sourcesContent` 必须是磁盘原文 [`File::original`]。scss 不做这类改写，两者相同。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::{CompileError, LineIndex};

/// 参与合并的源文件；下标即 sourcemap `sources` 的序号。
pub(crate) struct File {
    pub path: PathBuf,
    /// span 所在坐标（less 已做 `split_signed_dimensions`）。
    pub text: String,
    /// 磁盘原文（`sourcesContent`）；scss 与 [`File::text`] 相同。
    pub original: String,
    /// 行表在登记时建一次，供定位复用。
    index: LineIndex,
}

impl File {
    fn new(path: PathBuf, text: String, original: String) -> Self {
        Self {
            index: LineIndex::new(&text),
            path,
            text,
            original,
        }
    }
}

/// 合并串的一段：`[start, end)` 来自 `files[file]` 的 `orig` 起。
#[derive(Debug, Clone, Copy)]
pub(crate) struct Seg {
    pub start: usize,
    pub end: usize,
    pub file: u32,
    pub orig: usize,
}

/// 一次性拼装的子片（原 less `Expanded` / scss `Piece`）：文本 + 平移前的段表。
#[derive(Default)]
pub(crate) struct Part {
    pub text: String,
    pub segs: Vec<Seg>,
}

impl Part {
    pub(crate) fn empty() -> Self {
        Self::default()
    }

    /// 一个文件的完整文本（段覆盖 `[0, len)`）。
    pub(crate) fn whole(file: u32, text: String) -> Self {
        Self::source(file, text, 0)
    }

    /// 一个文件内的连续片段，`orig` 是片段在原文里的起点。
    pub(crate) fn source(file: u32, text: String, orig: usize) -> Self {
        let end = text.len();
        Self {
            text,
            segs: vec![Seg {
                start: 0,
                end,
                file,
                orig,
            }],
        }
    }

    /// 追加整个子片（段整体平移）。
    pub(crate) fn append(&mut self, other: &Part) {
        let base = self.text.len();
        self.text.push_str(&other.text);
        self.segs.extend(other.segs.iter().map(|seg| Seg {
            start: seg.start + base,
            end: seg.end + base,
            ..*seg
        }));
    }

    /// 把 `other[from..to)` 追加进来，被截取的段平移进新坐标。
    pub(crate) fn append_range(&mut self, other: &Part, from: usize, to: usize) {
        if from >= to {
            return;
        }
        let base = self.text.len();
        self.text.push_str(&other.text[from..to]);
        for seg in &other.segs {
            let start = seg.start.max(from);
            let end = seg.end.min(to);
            if start < end {
                self.segs.push(Seg {
                    start: base + (start - from),
                    end: base + (end - from),
                    file: seg.file,
                    orig: seg.orig + (start - seg.start),
                });
            }
        }
    }

    /// 用生成的包装文本夹住导入体（`@media` / `(reference)`）：包装部分归到宿主文件。
    pub(crate) fn wrap(prefix: &str, body: &Part, suffix: &str, host: u32, at: usize) -> Part {
        let mut part = Self::empty();
        part.push_mapped(prefix, host, at);
        part.append(body);
        part.push_mapped(suffix, host, at);
        part
    }

    fn push_mapped(&mut self, chunk: &str, file: u32, at: usize) {
        if chunk.is_empty() {
            return;
        }
        let start = self.text.len();
        self.text.push_str(chunk);
        self.segs.push(Seg {
            start,
            end: self.text.len(),
            file,
            orig: at,
        });
    }
}

/// 合并文本 + 文件表 + 段表。
#[derive(Default)]
pub(crate) struct Merged {
    pub text: String,
    pub files: Vec<File>,
    pub segs: Vec<Seg>,
    /// canonical path → 文件 id（[`Merged::add_file`] 去重）。
    index: HashMap<PathBuf, u32>,
}

impl Merged {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// 登记源文件；同一路径（canonicalize 后）返回既有 id。
    pub(crate) fn add_file(&mut self, path: &Path, text: String, original: String) -> u32 {
        let key = canonical(path);
        if let Some(&id) = self.index.get(&key) {
            return id;
        }
        let id = self.files.len() as u32;
        self.index.insert(key, id);
        self.files
            .push(File::new(path.to_path_buf(), text, original));
        id
    }

    pub(crate) fn file_id(&self, path: &Path) -> Option<u32> {
        let key = canonical(path);
        self.index.get(&key).copied()
    }

    /// 追加一个子片（原 less `push_expanded` / scss `Loader::run` 的拼装）。
    pub(crate) fn append_part(&mut self, part: Part) {
        let base = self.text.len();
        self.text.push_str(&part.text);
        self.segs.extend(part.segs.into_iter().map(|seg| Seg {
            start: seg.start + base,
            end: seg.end + base,
            ..seg
        }));
    }

    /// 精确命中：偏移必须落在某个段内。
    pub(crate) fn locate_exact(&self, at: usize) -> Option<(u32, usize)> {
        let i = self.segs.partition_point(|s| s.start <= at);
        let seg = self.segs.get(i.checked_sub(1)?)?;
        (at < seg.end).then(|| (seg.file, seg.orig + (at - seg.start)))
    }

    /// 合并偏移 → (文件路径, 1-based 行, 0-based 列)；未命中回退到最后一个段。
    pub(crate) fn position(&self, at: usize) -> (String, u32, u32) {
        let (file, orig) = self.locate_or_last(at.min(self.text.len()));
        let entry = &self.files[file as usize];
        let (line, column) = entry
            .index
            .line_col(orig.min(entry.text.len()), &entry.text);
        (entry.path.to_string_lossy().into_owned(), line + 1, column)
    }

    /// 指定文件内的偏移 → (路径, 1-based 行, 0-based 列)；文件不存在时 `None`。
    pub(crate) fn position_in(&self, file: u32, orig: usize) -> Option<(String, u32, u32)> {
        let entry = self.files.get(file as usize)?;
        let (line, column) = entry
            .index
            .line_col(orig.min(entry.text.len()), &entry.text);
        Some((entry.path.to_string_lossy().into_owned(), line + 1, column))
    }

    pub(crate) fn error_at(&self, at: usize, message: impl Into<String>) -> CompileError {
        let (path, line, column) = self.position(at);
        CompileError {
            message: message.into(),
            path: Some(path),
            line: Some(line),
            column: Some(column),
        }
    }

    /// 合并相邻同文件、偏移连续的段（保持分段表最小且便于二分）。
    pub(crate) fn normalize(&mut self) {
        let mut out: Vec<Seg> = Vec::with_capacity(self.segs.len());
        for seg in self.segs.drain(..) {
            if seg.start == seg.end {
                continue;
            }
            match out.last_mut() {
                Some(prev)
                    if prev.file == seg.file
                        && prev.end == seg.start
                        && prev.orig + (prev.end - prev.start) == seg.orig =>
                {
                    prev.end = seg.end;
                }
                _ => out.push(seg),
            }
        }
        self.segs = out;
    }

    /// scss 的越界回退：把偏移钳到最后一个段内。
    fn locate_or_last(&self, at: usize) -> (u32, usize) {
        if let Some(found) = self.locate_exact(at) {
            return found;
        }
        match self.segs.last() {
            Some(seg) => (
                seg.file,
                seg.orig + (at.saturating_sub(seg.start)).min(seg.end - seg.start),
            ),
            None => (0, at),
        }
    }
}

fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn merged_with(files: &[(&str, &str)]) -> (Merged, Vec<u32>) {
        let mut merged = Merged::new();
        let ids = files
            .iter()
            .map(|(path, text)| {
                merged.add_file(Path::new(path), text.to_string(), text.to_string())
            })
            .collect();
        (merged, ids)
    }

    #[test]
    fn append_part_spans_files() {
        let (mut merged, ids) = merged_with(&[("/a.scss", "ab"), ("/b.scss", "cd")]);
        merged.append_part(Part::whole(ids[0], "ab".to_string()));
        merged.append_part(Part::whole(ids[1], "cd".to_string()));
        assert_eq!(merged.text, "abcd");
        assert_eq!(merged.locate_exact(0), Some((ids[0], 0)));
        assert_eq!(merged.locate_exact(1), Some((ids[0], 1)));
        assert_eq!(merged.locate_exact(2), Some((ids[1], 0)));
        assert_eq!(merged.locate_exact(3), Some((ids[1], 1)));
        assert_eq!(merged.locate_exact(4), None);
    }

    #[test]
    fn normalize_merges_contiguous_segments() {
        let (mut merged, ids) = merged_with(&[("/a.scss", "abcd")]);
        let file = ids[0];
        merged.segs = vec![
            Seg {
                start: 0,
                end: 2,
                file,
                orig: 0,
            },
            Seg {
                start: 2,
                end: 4,
                file,
                orig: 2,
            },
            // 同文件但原文不连续：不能合并
            Seg {
                start: 4,
                end: 6,
                file,
                orig: 10,
            },
            // 空段被丢弃
            Seg {
                start: 6,
                end: 6,
                file,
                orig: 20,
            },
        ];
        merged.normalize();
        assert_eq!(merged.segs.len(), 2);
        assert_eq!((merged.segs[0].start, merged.segs[0].end), (0, 4));
        assert_eq!((merged.segs[1].start, merged.segs[1].end), (4, 6));
    }
}
