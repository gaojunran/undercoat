//! 涉及文件的内置函数：`data-uri`、`image-size` / `image-width` / `image-height`、
//! `svg-gradient`。
//!
//! 对拍基线 less@4.1.3：`functions/data-uri.js`、`less-node/image-size.js`
//! （依赖 image-size@0.5.5）、`functions/svg.js`。文件按 less 的 `entryPath` 语义
//! 相对**入口 less 文件所在目录**解析（不是进程 cwd），再依次尝试 `paths`。

use std::fs;
use std::path::{Path, PathBuf};

use super::value::{Color, fmt_number, parse_color, split_ws, unquote_ref};

/// `image-size` 的产物：less 拼成 `Dimension(width,'px') Dimension(height,'px')`。
pub(crate) struct Size {
    pub(crate) width: i64,
    pub(crate) height: i64,
}

/// 按 less `file-manager` 的顺序查找文件：绝对路径直接用，否则先入口目录再 `paths`。
fn locate(dir: &Path, paths: &[PathBuf], file: &str) -> Option<PathBuf> {
    let rel = Path::new(file);
    if rel.is_absolute() {
        return rel.is_file().then(|| rel.to_path_buf());
    }
    std::iter::once(dir.to_path_buf())
        .chain(paths.iter().map(PathBuf::from))
        .map(|root| root.join(rel))
        .find(|candidate| candidate.is_file())
}

/// `data-uri([mime,] path)`：把文件内联成 `url("data:<mime>,<data><fragment>")`。
///
/// 缺文件时按 less 回退成 `url(<原 path 节点>)`（保留原有引号形态）。
pub(crate) fn data_uri(dir: &Path, paths: &[PathBuf], args: &[String]) -> Option<String> {
    let (mime, file) = match args {
        [file] => (None, unquote_ref(file)),
        [mime, file] => (Some(unquote_ref(mime)), unquote_ref(file)),
        _ => return None,
    };
    // less：`#fragment` 不参与查找与类型推断，原样拼在 data 之后
    let (path, fragment) = match file.find('#') {
        Some(i) => (&file[..i], &file[i..]),
        None => (file, ""),
    };
    let fallback = || Some(format!("url({})", args.last()?));
    let Some(found) = locate(dir, paths, path) else {
        tracing::warn!("data-uri: 找不到 {path}，按 less 回退为 url()");
        return fallback();
    };
    let Ok(bytes) = fs::read(&found) else {
        return fallback();
    };
    let (mime, base64) = match mime {
        // 显式 mime：只有 `;base64` 结尾才 base64 编码（less 的判断）
        Some(mime) => (mime.to_string(), mime.ends_with(";base64")),
        None => {
            let mime = mime_lookup(path);
            // svg 与 ASCII/UTF-8 文本走 encodeURIComponent，其余 base64
            let base64 = mime != "image/svg+xml" && !is_utf8_charset(mime);
            let mime = if base64 {
                format!("{mime};base64")
            } else {
                mime.to_string()
            };
            (mime, base64)
        }
    };
    let data = if base64 {
        base64_encode(&bytes)
    } else {
        encode_uri_component(&String::from_utf8_lossy(&bytes))
    };
    Some(format!("url(\"data:{mime},{data}{fragment}\")"))
}

/// `image-size` / `image-width` / `image-height`：读图片头取宽高。
pub(crate) fn image_size(dir: &Path, paths: &[PathBuf], arg: &str) -> Option<Size> {
    let file = unquote_ref(arg);
    let file = file.split('#').next().unwrap_or(file);
    let found = locate(dir, paths, file)?;
    let bytes = fs::read(&found).ok()?;
    sniff(&bytes)
}

/// 按文件头识别格式（less 的 image-size 也不看扩展名）。
fn sniff(data: &[u8]) -> Option<Size> {
    if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        png(data)
    } else if data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a") {
        gif(data)
    } else if data.starts_with(&[0xFF, 0xD8]) {
        jpeg(data)
    } else {
        svg(data)
    }
}

/// PNG：IHDR 的宽高是偏移 16/20 的两个大端 u32；CgBI（"fried" png）后移一个块。
fn png(data: &[u8]) -> Option<Size> {
    let be32 = |at: usize| -> Option<i64> {
        let b = data.get(at..at + 4)?;
        Some(i64::from(u32::from_be_bytes([b[0], b[1], b[2], b[3]])))
    };
    if data.get(12..16)? == b"CgBI" {
        return Some(Size {
            width: be32(32)?,
            height: be32(36)?,
        });
    }
    if data.get(12..16)? != b"IHDR" {
        return None;
    }
    Some(Size {
        width: be32(16)?,
        height: be32(20)?,
    })
}

/// GIF：逻辑屏幕描述符里的两个小端 u16。
fn gif(data: &[u8]) -> Option<Size> {
    let le16 = |at: usize| -> Option<i64> {
        let b = data.get(at..at + 2)?;
        Some(i64::from(u16::from_le_bytes([b[0], b[1]])))
    };
    Some(Size {
        width: le16(6)?,
        height: le16(8)?,
    })
}

/// JPEG：逐段跳过，在 SOF（C0–CF，排除 C4/C8/CC）里读高、宽。
fn jpeg(data: &[u8]) -> Option<Size> {
    let u16_at = |at: usize| -> Option<usize> {
        let b = data.get(at..at + 2)?;
        Some(usize::from(u16::from_be_bytes([b[0], b[1]])))
    };
    let mut at = 2;
    while at + 1 < data.len() {
        if data[at] != 0xFF {
            // 段间允许 0xFF 填充字节（image-size 0.5.5 直接放弃，这里更宽容）
            at += 1;
            continue;
        }
        let marker = data[at + 1];
        // 无长度字段的独立标记
        if marker == 0x01 || (0xD0..=0xD9).contains(&marker) {
            at += 2;
            continue;
        }
        let len = u16_at(at + 2)?;
        if (0xC0..=0xCF).contains(&marker) && !matches!(marker, 0xC4 | 0xC8 | 0xCC) {
            let height = u16_at(at + 5)?;
            let width = u16_at(at + 7)?;
            return Some(Size {
                width: i64::try_from(width).ok()?,
                height: i64::try_from(height).ok()?,
            });
        }
        at += 2 + len.max(2);
    }
    None
}

/// SVG：扫 `<svg …>` 的 `width` / `height` / `viewBox`（image-size 0.5.5 的规则：
/// 宽高齐备直接用，否则按 `viewBox` 比例补齐缺失的一边）。
fn svg(data: &[u8]) -> Option<Size> {
    let capped = &data[..data.len().min(128 * 1024)];
    let text = String::from_utf8_lossy(capped);
    let tag = svg_tag(&text)?;
    // image-size 的 `width`/`height` 正则跳过百分比值（`[^%]+?`），交回 viewBox 推算
    let dim = |name| {
        svg_attr(tag, name)
            .filter(|v| !v.contains('%'))
            .and_then(leading_int)
    };
    let width = dim("width");
    let height = dim("height");
    if let (Some(width), Some(height)) = (width, height) {
        return Some(Size { width, height });
    }
    let view = svg_attr(tag, "viewBox").and_then(|v| {
        let bounds: Vec<i64> = v.split_whitespace().filter_map(leading_int).collect();
        (bounds.len() >= 4 && bounds[3] != 0).then(|| (bounds[2], bounds[3]))
    });
    let (vw, vh) = view?;
    let ratio = vw as f64 / vh as f64;
    match (width, height) {
        (Some(w), None) => Some(Size {
            width: w,
            height: (w as f64 / ratio).floor() as i64,
        }),
        (None, Some(h)) => Some(Size {
            width: (h as f64 * ratio).floor() as i64,
            height: h,
        }),
        _ => Some(Size {
            width: vw,
            height: vh,
        }),
    }
}

fn svg_tag(text: &str) -> Option<&str> {
    let start = text.find("<svg")?;
    let rest = &text[start..];
    // 必须紧跟空白或 `>`，避免匹配 `<svgfoo`
    let next = rest[4..].chars().next()?;
    if !(next.is_whitespace() || next == '>') {
        return None;
    }
    Some(&rest[..rest.find('>')? + 1])
}

/// 引号属性值；`name` 前不能是标识符字符（等价 image-size 的 `\b`）。
fn svg_attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let bytes = tag.as_bytes();
    let mut from = 0;
    while let Some(found) = tag[from..].find(name) {
        let at = from + found;
        from = at + name.len();
        if at > 0 && is_ident_byte(bytes[at - 1]) {
            continue;
        }
        let Some(rest) = tag[from..].trim_start().strip_prefix('=') else {
            continue;
        };
        let rest = rest.trim_start();
        let Some(quote) = rest.chars().next().filter(|c| *c == '"' || *c == '\'') else {
            continue;
        };
        let value = &rest[1..];
        return value.find(quote).map(|end| &value[..end]);
    }
    None
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b':' | b'.')
}

/// `parseInt` 的前缀语义：`100px` → 100，`50%` → 50，非数字开头 → None。
fn leading_int(value: &str) -> Option<i64> {
    let digits: String = value
        .trim()
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

/// `svg-gradient(direction, [color [position], …])`：less `functions/svg.js`。
pub(crate) fn svg_gradient(args: &[String]) -> Option<String> {
    let (kind, direction, rect) = match args.first()?.trim() {
        "to bottom" => (
            "linear",
            "x1=\"0%\" y1=\"0%\" x2=\"0%\" y2=\"100%\"",
            "x=\"0\" y=\"0\" width=\"1\" height=\"1\"",
        ),
        "to right" => (
            "linear",
            "x1=\"0%\" y1=\"0%\" x2=\"100%\" y2=\"0%\"",
            "x=\"0\" y=\"0\" width=\"1\" height=\"1\"",
        ),
        "to bottom right" => (
            "linear",
            "x1=\"0%\" y1=\"0%\" x2=\"100%\" y2=\"100%\"",
            "x=\"0\" y=\"0\" width=\"1\" height=\"1\"",
        ),
        "to top right" => (
            "linear",
            "x1=\"0%\" y1=\"100%\" x2=\"100%\" y2=\"0%\"",
            "x=\"0\" y=\"0\" width=\"1\" height=\"1\"",
        ),
        "ellipse" | "ellipse at center" => (
            "radial",
            "cx=\"50%\" cy=\"50%\" r=\"75%\"",
            "x=\"-50\" y=\"-50\" width=\"101\" height=\"101\"",
        ),
        // less 对未知方向抛错；这里保留原调用文本
        _ => return None,
    };
    let stops = args.get(1..)?;
    if stops.len() < 2 {
        return None;
    }
    let mut out = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 1 1\"><{kind}Gradient id=\"g\" {direction}>"
    );
    for (i, stop) in stops.iter().enumerate() {
        let parts = split_ws(stop);
        let (color, position) = match parts.len() {
            1 => (parse_color(&parts[0])?, None),
            2 if super::value::parse_number(&parts[1]).is_some() => {
                (parse_color(&parts[0])?, Some(parts[1].clone()))
            }
            _ => return None,
        };
        let last = i + 1 == stops.len();
        // 首尾可省略位置；中间的色标必须带位置（less 的校验）
        if position.is_none() && !last && i != 0 {
            return None;
        }
        let offset = position.unwrap_or_else(|| if i == 0 { "0%" } else { "100%" }.to_string());
        out.push_str(&format!(
            "<stop offset=\"{offset}\" stop-color=\"{}\"",
            hex_of(color)
        ));
        if color.a < 1.0 {
            out.push_str(&format!(" stop-opacity=\"{}\"", fmt_number(color.a)));
        }
        out.push_str("/>");
    }
    out.push_str(&format!(
        "</{kind}Gradient><rect {rect} fill=\"url(#g)\" /></svg>"
    ));
    Some(format!(
        "url('data:image/svg+xml,{}')",
        encode_uri_component(&out)
    ))
}

/// less `Color.toRGB()`：`#rrggbb`（通道四舍五入并夹到 0..255）。
fn hex_of(color: Color) -> String {
    let channel = |v: f64| v.round().clamp(0.0, 255.0) as u8;
    format!(
        "#{:02x}{:02x}{:02x}",
        channel(color.r),
        channel(color.g),
        channel(color.b)
    )
}

/// `mime@1.6.0` 的子集（less node 环境用它推断 data-uri 的 MIME）。
fn mime_lookup(file: &str) -> &'static str {
    let ext = file
        .rsplit(['.', '/', '\\'])
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" | "jpe" => "image/jpeg",
        "gif" => "image/gif",
        "svg" | "svgz" => "image/svg+xml",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "bmp" => "image/bmp",
        "tif" | "tiff" => "image/tiff",
        "html" | "htm" => "text/html",
        "xhtml" => "application/xhtml+xml",
        "css" => "text/css",
        "less" => "text/less",
        "scss" => "text/x-scss",
        "js" | "mjs" => "application/javascript",
        "json" | "map" => "application/json",
        "txt" => "text/plain",
        "xml" => "application/xml",
        "md" | "markdown" => "text/markdown",
        "csv" => "text/csv",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "eot" => "application/vnd.ms-fontobject",
        "vtt" => "text/vtt",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "gz" => "application/gzip",
        "wasm" => "application/wasm",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" => "audio/ogg",
        _ => "application/octet-stream",
    }
}

/// `mime@1.6.0` 的 `charsets.lookup`：这些类型按 UTF-8 文本内联（不 base64）。
fn is_utf8_charset(mime: &str) -> bool {
    mime.starts_with("text/")
        || mime
            .strip_prefix("application/")
            .is_some_and(|rest| rest.starts_with("javascript") || rest.starts_with("json"))
}

/// JS `encodeURIComponent`：保留 `A-Za-z0-9-_.!~*'()`，其余按 UTF-8 字节转 `%XX`。
fn encode_uri_component(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for b in text.bytes() {
        let keep = b.is_ascii_alphanumeric()
            || matches!(
                b,
                b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')'
            );
        if keep {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn base64_encode(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let bytes = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let bits = u32::from(bytes[0]) << 16 | u32::from(bytes[1]) << 8 | u32::from(bytes[2]);
        let pick = |shift: u32| char::from(TABLE[(bits >> shift) as usize & 63]);
        out.push(pick(18));
        out.push(pick(12));
        out.push(if chunk.len() > 1 { pick(6) } else { '=' });
        out.push(if chunk.len() > 2 { pick(0) } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("undercoat-file-fn-{name}"));
        fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    #[test]
    fn png_size() {
        let mut data = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        data.extend_from_slice(&640u32.to_be_bytes());
        data.extend_from_slice(&430u32.to_be_bytes());
        let size = sniff(&data).expect("png");
        assert_eq!((size.width, size.height), (640, 430));
    }

    #[test]
    fn gif_size() {
        let mut data = b"GIF89a".to_vec();
        data.extend_from_slice(&7u16.to_le_bytes());
        data.extend_from_slice(&3u16.to_le_bytes());
        let size = sniff(&data).expect("gif");
        assert_eq!((size.width, size.height), (7, 3));
    }

    #[test]
    fn jpeg_size() {
        // SOI + APP0(JFIF, 16 字节) + SOF0(高 430 / 宽 640) + EOI
        let mut data = vec![0xFF, 0xD8];
        data.extend_from_slice(&[0xFF, 0xE0, 0x00, 0x10]);
        data.extend_from_slice(b"JFIF\0\x01\x01\0\0\x01\0\x01\0\0");
        data.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x11, 0x08]);
        data.extend_from_slice(&430u16.to_be_bytes());
        data.extend_from_slice(&640u16.to_be_bytes());
        data.extend_from_slice(&[0x03, 0x01, 0x11, 0x00, 0x02, 0x11, 0x01, 0x03, 0x11, 0x01]);
        data.extend_from_slice(&[0xFF, 0xD9]);
        let size = sniff(&data).expect("jpeg");
        assert_eq!((size.width, size.height), (640, 430));
    }

    #[test]
    fn svg_size() {
        let data = br#"<svg xmlns="http://www.w3.org/2000/svg" width="12" height="34"></svg>"#;
        let size = sniff(data).expect("svg");
        assert_eq!((size.width, size.height), (12, 34));

        // 只有 viewBox：按比例补另一边
        let data = br#"<svg viewBox="0 0 60 30" width="20"></svg>"#;
        let size = sniff(data).expect("svg viewbox");
        assert_eq!((size.width, size.height), (20, 10));
    }

    #[test]
    fn data_uri_mime_and_fallback() {
        let dir = temp_dir("data-uri");
        fs::write(dir.join("a.txt"), "hi").expect("write");
        fs::write(dir.join("a.svg"), "<svg version=\"1.1\"><g></g></svg>").expect("write");
        fs::write(dir.join("a.png"), [0x89u8, b'P', b'N', b'G', 1, 2, 3]).expect("write");
        fs::write(dir.join("noext"), "zz").expect("write");
        let arg = |s: &str| s.to_string();

        // 文本类型：encodeURIComponent，不 base64
        assert_eq!(
            data_uri(&dir, &[], &[arg("'a.txt'")]).as_deref(),
            Some("url(\"data:text/plain,hi\")")
        );
        assert_eq!(
            data_uri(&dir, &[], &[arg("'a.svg'")]).as_deref(),
            Some(
                "url(\"data:image/svg+xml,%3Csvg%20version%3D%221.1%22%3E%3Cg%3E%3C%2Fg%3E%3C%2Fsvg%3E\")"
            )
        );
        // 二进制：推断 mime + base64（7 字节 → 末字节补一个 `=`）
        assert_eq!(
            data_uri(&dir, &[], &[arg("'a.png'")]).as_deref(),
            Some("url(\"data:image/png;base64,iVBORwECAw==\")")
        );
        // 未知扩展名 → application/octet-stream
        assert_eq!(
            data_uri(&dir, &[], &[arg("'noext'")]).as_deref(),
            Some("url(\"data:application/octet-stream;base64,eno=\")")
        );
        // 显式 mime
        assert_eq!(
            data_uri(&dir, &[], &[arg("'text/html'"), arg("'a.txt'")]).as_deref(),
            Some("url(\"data:text/html,hi\")")
        );
        // 缺文件 → 回退 url(<原节点>)
        assert_eq!(
            data_uri(&dir, &[], &[arg("\"missing.png\"")]).as_deref(),
            Some("url(\"missing.png\")")
        );
    }

    #[test]
    fn svg_gradient_matches_less() {
        let out = svg_gradient(&[arg("to bottom"), arg("black"), arg("white")]).expect("gradient");
        assert!(out.starts_with("url('data:image/svg+xml,%3Csvg%20xmlns%3D"));
        assert!(out.contains("id%3D%22g%22%20x1%3D%220%25%22%20y1%3D%220%25%22"));
        assert!(out.contains("stop-color%3D%22%23000000%22%2F%3E"));
        assert!(out.ends_with("%3C%2Fsvg%3E')"));

        let out = svg_gradient(&[arg("to bottom"), arg("red 20%"), arg("blue")]).expect("pos");
        assert!(out.contains("offset%3D%2220%25%22"));
        // 中间色标缺位置 → less 抛错，这里保持原调用
        assert!(svg_gradient(&[arg("to bottom"), arg("red"), arg("blue"), arg("green")]).is_none());
    }

    fn arg(s: &str) -> String {
        s.to_string()
    }
}
