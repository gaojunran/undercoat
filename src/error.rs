//! 编译错误类型：整个 crate 唯一的错误值。
//!
//! 解析期错误带定位（行/列相对出错文件文本），求值期语义错误只有 message；
//! `Display` 只输出 message，`source()` 恒为 `None`（无错误链）。
//!
//! 内部各阶段用 `Result<T, Box<CompileError>>`（公开入口再拆箱）：`CompileError`
//! 本身 64 字节，直接内联进 `Result` 会让深度递归（mixin 嵌套 256 层）的调试版
//! 栈帧显著变大而溢出；装箱后 `Result` 仍是 8 字节（胖指针大小）。

/// 编译错误：`path` / `line` / `column` 只在定位命中时存在。
#[derive(Debug)]
pub struct CompileError {
    pub message: String,
    /// 出错文件绝对路径（定位命中时）。
    pub path: Option<String>,
    /// 1-based 行号。
    pub line: Option<u32>,
    /// 0-based 列号。
    pub column: Option<u32>,
}

impl CompileError {
    /// 无定位的错误（读盘失败、不支持的入口等）。
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            path: None,
            line: None,
            column: None,
        }
    }
}

impl std::fmt::Display for CompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for CompileError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        None
    }
}
