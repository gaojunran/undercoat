//! Text-scanning primitives: quotes, comments, brackets, and top-level splitting in
//! CSS/Less/Sass source text.
//!
//! Everything scans **byte-wise** (UTF-8 continuation bytes never collide with ASCII
//! structural characters) and returns byte offsets. Callers convert to whatever unit they
//! need (ranges, slices, character indices).
//!
//! Two boundary conventions:
//! - An unterminated quote scans to the end of the text (`skip_string` returns `len`).
//! - An unterminated comment likewise runs to the end (`skip_comment` returns `len`).

/// `i` points at a quote; returns the offset **after** the closing quote, or `len` if
/// unterminated.
pub(crate) fn skip_string(src: &str, i: usize) -> usize {
    let bytes = src.as_bytes();
    let quote = bytes[i];
    let mut j = i + 1;
    while j < bytes.len() {
        match bytes[j] {
            // Escape: skip the next byte (it may be part of a multi-byte character, but skipping
            // just 1 is still safe, because the following bytes never equal an ASCII quote).
            b'\\' => j += 2,
            b if b == quote => return j + 1,
            _ => j += 1,
        }
    }
    bytes.len()
}

/// When `i` points at `/` followed by `*` or `/`, returns the offset **after** the comment;
/// returns `None` if it is not a comment.
///
/// Both `/* */` and `//` are supported (the latter runs to end of line and does not swallow
/// the newline).
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

/// `open` points at `(` / `[` / `{`; returns the offset **after** the matching closer.
///
/// Skips quoted strings (including escapes) and arbitrarily nested brackets of all three
/// kinds; does not recognize comments (the `//` in `url(//host)` is a path, not a comment).
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

/// Split on top-level `sep` and return the byte range of each piece (excluding the separator).
///
/// Skips quoted strings, comments, and nested brackets; the separator itself belongs to no
/// piece, and empty pieces are kept.
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
        // Ranges slice the original back out
        let ranges = split_top_level(src, b',');
        assert_eq!(ranges[1], (2, 9));
        // Separators inside comments do not split
        assert_eq!(pieces("a /* , */ , b", b','), ["a /* , */ ", " b"]);
        // Empty pieces are kept
        assert_eq!(pieces("a,,b", b','), ["a", "", "b"]);
        assert_eq!(pieces("", b','), [""]);
    }

    #[test]
    fn escapes_keep_quotes_open() {
        // `\'` is not a closing quote, so the whole string is `'a\'b'`
        assert_eq!(pieces("'a\\'b', c", b','), ["'a\\'b'", " c"]);
        assert_eq!(skip_string("'a\\'b'", 0), 6);
    }
}
