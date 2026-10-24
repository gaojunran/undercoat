//! Merged text and segment table after `@import` inlining.
//!
//! Both less and scss expand `@import` into one **merged text**: subsequent parsing,
//! evaluation, and printing only ever see merged-text coordinates; error locations and
//! sourcemaps then go back through the segment table to "which file + offset within that
//! file". This module owns only that mapping and knows nothing about syntax.
//!
//! # Coordinate contract
//!
//! - [`Seg`] says that merged text `[start, end)` comes from `[orig, orig + len)` of
//!   `files[file]`, i.e. `end - start == the source length covered by the segment`. Segments
//!   are sorted by `start` ascending and never overlap; [`Merged::normalize`] merges adjacent
//!   segments that are contiguous within the same file.
//! - [`Merged::locate_exact`] only does an **exact** match: it returns `None` when the offset
//!   does not fall inside any segment, and how to fall back on out-of-range offsets is up to
//!   the caller (less falls back to the same offset in the entry file, scss to the last
//!   segment).
//! - Generated text (the `@media` / `(reference)` wrapper, synthesized `@use` statements,
//!   interpolation insertions) is registered as segments too, mapped to a fixed original
//!   offset, so the segment table always covers the entire merged text.
//!
//! # Why less has two texts
//!
//! Before parsing, less applies a `split_signed_dimensions` lexical fixup to each file
//! (splitting `1px+2` into separate tokens), so spans point into the patched [`File::text`],
//! while the sourcemap's `sourcesContent` must be the on-disk original [`File::original`].
//! scss does no such rewriting, so there the two are identical.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::{CompileError, LineIndex};

/// A source file taking part in the merge; the index is its position in the sourcemap
/// `sources`.
pub(crate) struct File {
    pub path: PathBuf,
    /// The coordinates spans refer to (for less, after `split_signed_dimensions`).
    pub text: String,
    /// The on-disk original (`sourcesContent`); for scss this equals [`File::text`].
    pub original: String,
    /// The line table, built once at registration and reused for location lookups.
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

/// One segment of the merged text: `[start, end)` starts at `orig` in `files[file]`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Seg {
    pub start: usize,
    pub end: usize,
    pub file: u32,
    pub orig: usize,
}

/// A sub-piece assembled in one go (the former less `Expanded` / scss `Piece`): text plus
/// the segment table before translation.
#[derive(Default)]
pub(crate) struct Part {
    pub text: String,
    pub segs: Vec<Seg>,
}

impl Part {
    pub(crate) fn empty() -> Self {
        Self::default()
    }

    /// The complete text of one file (the segment covers `[0, len)`).
    pub(crate) fn whole(file: u32, text: String) -> Self {
        Self::source(file, text, 0)
    }

    /// A contiguous slice within one file; `orig` is where the slice starts in the original.
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

    /// Append a whole sub-piece (all segments shift as a whole).
    pub(crate) fn append(&mut self, other: &Part) {
        let base = self.text.len();
        self.text.push_str(&other.text);
        self.segs.extend(other.segs.iter().map(|seg| Seg {
            start: seg.start + base,
            end: seg.end + base,
            ..*seg
        }));
    }

    /// Append `other[from..to)`, translating the clipped segments into the new coordinates.
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

    /// Sandwich the imported body with generated wrapper text (`@media` / `(reference)`); the
    /// wrapper parts are attributed to the host file.
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

/// Merged text + file table + segment table.
#[derive(Default)]
pub(crate) struct Merged {
    pub text: String,
    pub files: Vec<File>,
    pub segs: Vec<Seg>,
    /// canonical path → file id (deduplicated by [`Merged::add_file`]).
    index: HashMap<PathBuf, u32>,
}

impl Merged {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Register a source file; the same path (after canonicalize) returns the existing id.
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

    /// Append one sub-piece (the assembly done by the former less `push_expanded` / scss
    /// `Loader::run`).
    pub(crate) fn append_part(&mut self, part: Part) {
        let base = self.text.len();
        self.text.push_str(&part.text);
        self.segs.extend(part.segs.into_iter().map(|seg| Seg {
            start: seg.start + base,
            end: seg.end + base,
            ..seg
        }));
    }

    /// Exact match: the offset must fall inside some segment.
    pub(crate) fn locate_exact(&self, at: usize) -> Option<(u32, usize)> {
        let i = self.segs.partition_point(|s| s.start <= at);
        let seg = self.segs.get(i.checked_sub(1)?)?;
        (at < seg.end).then(|| (seg.file, seg.orig + (at - seg.start)))
    }

    /// Merged offset → (file path, 1-based line, 0-based column); falls back to the last
    /// segment on a miss.
    pub(crate) fn position(&self, at: usize) -> (String, u32, u32) {
        let (file, orig) = self.locate_or_last(at.min(self.text.len()));
        let entry = &self.files[file as usize];
        let (line, column) = entry
            .index
            .line_col(orig.min(entry.text.len()), &entry.text);
        (entry.path.to_string_lossy().into_owned(), line + 1, column)
    }

    /// Offset within a given file → (path, 1-based line, 0-based column); `None` when the file
    /// does not exist.
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

    /// Merge adjacent segments that are contiguous in the same file (keeps the segment table
    /// minimal and friendly to binary search).
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

    /// scss's out-of-range fallback: clamp the offset into the last segment.
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
            // Same file but non-contiguous in the original: cannot merge
            Seg {
                start: 4,
                end: 6,
                file,
                orig: 10,
            },
            // Empty segments are dropped
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
