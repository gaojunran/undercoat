//! A CSS printer with source mapping.
//!
//! The printer is the sole exit point for sourcemaps: every token that originates in the
//! source must go through [`Printer::token`], while generated separators (indentation,
//! newlines, commas) go through [`Printer::raw`]. This way "can the original location be
//! looked up" is decided by whether a span is attached at print time, not by patching things
//! up afterwards.
//!
//! Multiple files (`@import` inlining) are registered via [`Printer::add_file`], and
//! [`Printer::set_segments`] supplies the "merged text → each file" segment table: a token's
//! span is first binary-searched from merged-text coordinates back to `(file, original
//! offset)`, then converted using that file's line table. The printer holds no borrows of
//! external strings.
//!
//! The printer is append-only and never rewrites: finishing passes such as extend happen on
//! the IR (`doc`), not on text that has already been written here.

use oxc_css_parser::pos::Span;

use crate::sourcemap::{Builder, Mapping};
use crate::{LineIndex, Options};

struct SourceFile {
    src: String,
    index: LineIndex,
    /// The on-disk original (`sourcesContent`); kept `None` in the single-file case so the
    /// existing map bytes stay unchanged.
    content: Option<String>,
}

/// One segment of the merged text: `[start, end)` starts at `orig_start` in `file`.
///
/// After `@import` text inlining, every span the printer receives is in merged-text
/// coordinates; during the finishing pass they go through this table to be binary-searched
/// back to `(file, original offset)`, then converted to line/column via that file's line
/// table.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SourceSegment {
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) file: u32,
    pub(crate) orig_start: usize,
}

/// One source-token write: records the output offset now and converts it to line/column
/// during the finishing pass.
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

    /// Register an additional source file (`@import` inlining) and return its file id.
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

    /// Record a file's on-disk original (the `sourcesContent` of a multi-file map).
    pub(crate) fn set_content(&mut self, file: u32, content: &str) {
        if let Some(f) = self.files.get_mut(file as usize) {
            f.content = Some(content.to_string());
        }
    }

    /// The merged text → source file segment table (ascending, non-overlapping, covering the
    /// whole merged text).
    pub(crate) fn set_segments(&mut self, segments: Vec<SourceSegment>) {
        self.segments = segments;
    }

    /// Purely generated text (indentation, separators, newlines); produces no mapping.
    pub(crate) fn raw(&mut self, s: &str) {
        self.out.push_str(s);
    }

    /// A source token (using the current file): advances the output and records one mapping.
    pub(crate) fn token(&mut self, s: &str, span: Span) {
        let file = self.current;
        self.token_in(file, s, span);
    }

    /// A source token attributed to the given file.
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

    /// Finishing pass: returns (css, map).
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
            // Add a mapping at the start of the line (col 0) so that IDEs querying by whole
            // line / line start can still look it up
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

    /// Merged offset → `(file, original offset)`; returns `None` without a segment table
    /// (single file), keeping the old path.
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

/// `sources` rewriting: when the projectRoot prefix matches, make it project-relative (WDT
/// `path.posix.relative` semantics).
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
