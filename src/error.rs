//! Compile error type: the only error value in the whole crate.
//!
//! Parse-time errors carry a location (line/column relative to the failing file's text);
//! evaluation-time semantic errors only have a message. `Display` prints only the message,
//! and `source()` is always `None` (no error chain).
//!
//! Each internal stage uses `Result<T, Box<CompileError>>` (the public entry points unbox
//! it): `CompileError` itself is 64 bytes, and inlining it directly into `Result` would make
//! debug-build stack frames of deep recursion (256 levels of nested mixins) significantly
//! larger and overflow the stack; boxed, `Result` is still 8 bytes (the size of a fat
//! pointer).

/// Compile error: `path` / `line` / `column` are present only when a location was matched.
#[derive(Debug)]
pub struct CompileError {
    pub message: String,
    /// Absolute path of the failing file (when a location was matched).
    pub path: Option<String>,
    /// 1-based line number.
    pub line: Option<u32>,
    /// 0-based column number.
    pub column: Option<u32>,
}

impl CompileError {
    /// An error without a location (read failure, unsupported entry point, etc.).
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
