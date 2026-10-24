//! undercoat: a native Rust compiler from less / scss → CSS.
//!
//! Parsing is delegated to `oxc-css-parser` (which produces a span-carrying AST); this
//! crate handles `@import` / `@use` assembly, evaluation, selector expansion, and printing,
//! with no Node runtime required.
//!
//! Both dialects share one output intermediate representation and printer; all output-shape
//! differences live in `doc::Style`. The evaluation cores are independent (the value models
//! differ; see `docs/architecture.md`).
//!
//! Conformance baseline: less 4.1.3; scss capabilities match dart-sass 1.93.2 and output
//! shape matches 1.35.2.

pub(crate) mod doc;
mod error;
pub(crate) mod less;
pub(crate) mod merge;
pub(crate) mod print;
// Public only as a test hook (`scss::indented`); the product API is the 4 symbols below.
#[doc(hidden)]
pub mod scss;
pub(crate) mod sourcemap;
pub(crate) mod text;

use std::path::Path;

pub use error::CompileError;

/// Compilation output.
#[derive(Debug, Default)]
pub struct Output {
    pub css: String,
    pub map: Option<String>,
    /// Absolute paths of dependency files (less `output.imports` / sass `stats.includedFiles`
    /// semantics).
    pub deps: Vec<String>,
}

/// Compilation options.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Extra lookup roots for `@import` (absolute paths), with different semantics per
    /// dialect: less treats them purely as appended lookup roots (straight `root.join(href)`,
    /// no suffix appended); scss feeds them into dart's full candidate enumeration
    /// (`_partial` / directory index / multiple suffixes, etc.).
    pub paths: Vec<std::path::PathBuf>,
    /// Whether to emit a sourcemap.
    pub source_map: bool,
    /// Base directory for sourcemap `sources` (projectPath): when the prefix matches, rewrite
    /// them to a relative form.
    pub project_root: Option<String>,
    /// Dialect-specific options.
    pub less: LessOptions,
}

/// Options specific to the less pipeline (the scss pipeline does not read them).
#[derive(Debug, Clone, Default)]
pub struct LessOptions {
    /// less `globalVars` (the options of the useCompilerPlugins pairs): affects the less
    /// pipeline only.
    pub global_vars: Vec<(String, String)>,
}

/// Line-start offset index: byte offset → (line, column), both 0-based (sourcemap v3
/// semantics).
///
/// Not a product API; kept externally visible only for tests/debugging.
#[doc(hidden)]
pub struct LineIndex {
    starts: Vec<usize>,
}

impl LineIndex {
    pub fn new(src: &str) -> Self {
        let mut starts = vec![0usize];
        for (i, b) in src.bytes().enumerate() {
            if b == b'\n' {
                starts.push(i + 1);
            }
        }
        Self { starts }
    }

    /// Byte offset → (0-based line, 0-based column). Columns are counted in UTF-16 units (as
    /// sourcemap v3 requires).
    pub fn line_col(&self, offset: usize, src: &str) -> (u32, u32) {
        let line = match self.starts.binary_search(&offset) {
            Ok(l) => l,
            Err(l) => l - 1,
        };
        let start = self.starts[line];
        let end = offset.min(src.len());
        let col: usize = src[start..end].chars().map(|c| c.len_utf16()).sum();
        (line as u32, col as u32)
    }
}

/// Compilation entry point: dispatches on the file extension.
pub fn compile_file(path: &Path, source: &str, opts: &Options) -> Result<Output, CompileError> {
    let name = path.to_string_lossy();
    if name.ends_with(".less") {
        less::compile(path, source, opts).map_err(|e| *e)
    } else if name.ends_with(".scss")
        || name.ends_with(".sass")
        || name.ends_with(".css")
        || name.ends_with(".wxss")
    {
        // `.sass` shares the `.scss` pipeline: the indented syntax is folded into equivalent
        // SCSS during loading in `scss::imports`.
        scss::compile(path, source, opts).map_err(|e| *e)
    } else {
        Err(CompileError::new(format!(
            "unsupported style extension: {name}"
        )))
    }
}
