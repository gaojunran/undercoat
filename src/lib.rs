//! undercoat：less / scss → CSS 的原生 Rust 编译器。
//!
//! 解析复用 `oxc-css-parser`（产出带 span 的 AST），本 crate 负责 `@import` / `@use`
//! 装配、求值、选择器展开与打印，不需要 Node 运行时。
//!
//! 两个方言共用一个输出中间表示与打印器，产物形态差异全部集中在 `doc::Style`；
//! 求值核心各自独立（值模型不同，见 `docs/architecture.md`）。
//!
//! 对拍基线：less 4.1.3；scss 能力对齐 dart-sass 1.93.2，输出形态对齐 1.35.2。

pub(crate) mod doc;
mod error;
pub(crate) mod less;
pub(crate) mod merge;
pub(crate) mod print;
// 公开仅为测试钩子（`scss::indented`），产品 API 只有本文件下方 4 个符号。
#[doc(hidden)]
pub mod scss;
pub(crate) mod sourcemap;
pub(crate) mod text;

use std::path::Path;

pub use error::CompileError;

/// 编译产物。
#[derive(Debug, Default)]
pub struct Output {
    pub css: String,
    pub map: Option<String>,
    /// 依赖文件绝对路径（less `output.imports` / sass `stats.includedFiles` 语义）。
    pub deps: Vec<String>,
}

/// 编译选项。
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// `@import` 的额外查找根（绝对路径），两侧语义不同：
    /// less 只当其是追加查找根（直接 `root.join(href)`，不补后缀）；
    /// scss 则参与 dart 的完整候选枚举（`_partial` / 目录 index / 多后缀等）。
    pub paths: Vec<std::path::PathBuf>,
    /// 是否产出 sourcemap。
    pub source_map: bool,
    /// sourcemap `sources` 的基准目录（projectPath）：命中前缀则改写成相对形态。
    pub project_root: Option<String>,
    /// 方言专属选项。
    pub less: LessOptions,
}

/// less 通道专属选项（scss 通道不读）。
#[derive(Debug, Clone, Default)]
pub struct LessOptions {
    /// less `globalVars`（useCompilerPlugins 二元组的 options）：只影响 less 通道。
    pub global_vars: Vec<(String, String)>,
}

/// 行首偏移索引：字节 offset → (行, 列)，均 0-based（sourcemap v3 语义）。
///
/// 非产品 API，仅为测试/调试保留外部可见。
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

    /// 字节偏移 → (0-based 行, 0-based 列)。列按 UTF-16 计算（sourcemap v3 要求）。
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

/// 编译入口：按后缀分流。
pub fn compile_file(path: &Path, source: &str, opts: &Options) -> Result<Output, CompileError> {
    let name = path.to_string_lossy();
    if name.ends_with(".less") {
        less::compile(path, source, opts).map_err(|e| *e)
    } else if name.ends_with(".scss")
        || name.ends_with(".sass")
        || name.ends_with(".css")
        || name.ends_with(".wxss")
    {
        // `.sass` 与 `.scss` 同一条通道：缩进语法在 `scss::imports` 装载期折成等义 SCSS。
        scss::compile(path, source, opts).map_err(|e| *e)
    } else {
        Err(CompileError::new(format!("不支持的样式后缀: {name}")))
    }
}
