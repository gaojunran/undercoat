//! 文本扫描原语：CSS/Less/Sass 源文本里的引号、注释、括号与顶层切分。
//!
//! 全部按**字节**扫描（UTF-8 的续字节不会与 ASCII 结构字符冲突），返回字节偏移。
//! 调用方按需换算成自身使用的单位（区间、切片、字符下标）。
//!
//! 两个边界约定：
//! - 引号未闭合时扫描到文本末尾（`skip_string` 返回 `len`）。
//! - 注释未闭合时同样到文本末尾（`skip_comment` 返回 `len`）。

/// `i` 指向引号，返回闭引号**之后**的偏移；未闭合返回 `len`。
pub(crate) fn skip_string(src: &str, i: usize) -> usize {
    let bytes = src.as_bytes();
    let quote = bytes[i];
    let mut j = i + 1;
    while j < bytes.len() {
        match bytes[j] {
            // 转义：跳过下一个字节（可能是多字节字符的一部分，但只跳 1 也安全，
            // 因为后续字节不会等于 ASCII 引号）。
            b'\\' => j += 2,
            b if b == quote => return j + 1,
            _ => j += 1,
        }
    }
    bytes.len()
}

/// `i` 指向 `/` 且后随 `*` 或 `/` 时，返回注释**之后**的偏移；不是注释返回 `None`。
///
/// `/* */` 与 `//`（后者到行尾，不吞换行）都支持。
pub(crate) fn skip_comment(src: &str, i: usize) -> Option<usize> {
    let bytes = src.as_bytes();
    if bytes.get(i) != Some(&b'/') {
        return None;
    }
    match bytes.get(i + 1) {
        Some(b'*') => {
            let mut j = i + 2;
            while j + 1 < bytes.len() {
                if bytes[j] == b'*' && bytes[j + 1] == b'/' {
                    return Some(j + 2);
                }
                j += 1;
            }
            Some(bytes.len())
        }
        Some(b'/') => {
            let mut j = i + 2;
            while j < bytes.len() && bytes[j] != b'\n' {
                j += 1;
            }
            Some(j)
        }
        _ => None,
    }
}

/// `open` 指向 `(` / `[` / `{`，返回配对闭符**之后**的偏移。
///
/// 跳过引号字符串（含转义）与任意嵌套的三种括号；不识别注释（`url(//host)` 里的
/// `//` 是路径而非注释）。
pub(crate) fn matching(src: &str, open: usize) -> Option<usize> {
    let bytes = src.as_bytes();
    if !matches!(bytes.get(open), Some(b'(' | b'[' | b'{')) {
        return None;
    }
    let mut depth = 0usize;
    let mut i = open;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' | b'"' => {
                i = skip_string(src, i);
                continue;
            }
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// 按顶层 `sep` 切分，返回各段的字节区间（不含分隔符）。
///
/// 跳过引号字符串、注释与嵌套括号；分隔符本身不进任何一段，空段保留。
pub(crate) fn split_top_level(src: &str, sep: u8) -> Vec<(usize, usize)> {
    let bytes = src.as_bytes();
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' | b'"' => {
                i = skip_string(src, i);
                continue;
            }
            b'/' if matches!(bytes.get(i + 1), Some(b'/' | b'*')) => {
                i = skip_comment(src, i).unwrap_or(i + 1);
                continue;
            }
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth = depth.saturating_sub(1),
            b if b == sep && depth == 0 => {
                out.push((start, i));
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    out.push((start, bytes.len()));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pieces(src: &str, sep: u8) -> Vec<&str> {
        split_top_level(src, sep)
            .into_iter()
            .map(|(s, e)| &src[s..e])
            .collect()
    }

    #[test]
    fn unterminated_string_runs_to_end() {
        assert_eq!(skip_string("'abc", 0), 4);
        assert_eq!(skip_string("\"a\\\"b\"", 0), 6);
    }

    #[test]
    fn nested_brackets_match_after_close() {
        assert_eq!(matching("(a(b)c)", 0), Some(7));
        assert_eq!(matching("f(a[b]{c})d", 1), Some(10));
        assert_eq!(matching("(a\")b\")", 0), Some(7));
        assert_eq!(matching("(unclosed", 0), None);
        assert_eq!(matching("x", 0), None);
    }

    #[test]
    fn line_and_block_comments() {
        assert_eq!(skip_comment("//abc\ndef", 0), Some(5));
        assert_eq!(skip_comment("/* a,\nb */x", 0), Some(10));
        assert_eq!(skip_comment("/x", 0), None);
        assert_eq!(skip_comment("/* open", 0), Some(7));
    }

    #[test]
    fn split_ranges_are_exact() {
        let src = "a, (b, c), \"d,e\", f";
        let got = pieces(src, b',');
        assert_eq!(got, ["a", " (b, c)", " \"d,e\"", " f"]);
        // 区间回切原文
        let ranges = split_top_level(src, b',');
        assert_eq!(ranges[1], (2, 9));
        // 注释里的分隔符不切
        assert_eq!(pieces("a /* , */ , b", b','), ["a /* , */ ", " b"]);
        // 空段保留
        assert_eq!(pieces("a,,b", b','), ["a", "", "b"]);
        assert_eq!(pieces("", b','), [""]);
    }

    #[test]
    fn escapes_keep_quotes_open() {
        // `\'` 不是闭引号，整个字符串是 `'a\'b'`
        assert_eq!(pieces("'a\\'b', c", b','), ["'a\\'b'", " c"]);
        assert_eq!(skip_string("'a\\'b'", 0), 6);
    }
}
