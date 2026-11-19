//! What a file's bytes are sent as. The type comes from the path's extension alone, never from
//! the bytes, and only kinds that cannot run as a document in a browser are shown in place.
//! Everything else is a download.

use std::fmt::Write as _;

/// What plain text goes out as, whatever kind of text it is.
const TEXT: &str = "text/plain; charset=utf-8";
/// What a download goes out as.
const BYTES: &str = "application/octet-stream";
/// Nothing may load or run. Inline styles alone are let through, since they can do neither and
/// an SVG shown on its own is drawn with them.
const NOTHING: &str = "default-src 'none'; style-src 'unsafe-inline'";
/// The same, in a sandbox: no scripts, no forms, no plugins, and an origin of its own.
const SANDBOXED: &str = "default-src 'none'; style-src 'unsafe-inline'; sandbox";

/// How a file goes to a browser.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Media {
    /// The `Content-Type`.
    pub content_type: &'static str,
    /// Whether a browser may show it in place. Otherwise it is a download.
    pub inline: bool,
    /// The `Content-Security-Policy`.
    pub policy: &'static str,
}

/// How the file at `path` is sent, from its extension as `mime_guess` maps it.
#[must_use]
pub fn of(path: &str) -> Media {
    let shown = |content_type| Media {
        content_type,
        inline: true,
        policy: SANDBOXED,
    };
    match mime_guess::from_path(path).first_raw() {
        Some(
            kind @ ("image/png" | "image/jpeg" | "image/gif" | "image/webp" | "image/avif"
            | "image/svg+xml" | "audio/mpeg" | "audio/m4a" | "audio/aac" | "audio/ogg"
            | "audio/wav" | "audio/flac" | "audio/webm" | "video/mp4" | "video/webm"
            | "video/ogg" | "video/quicktime"),
        ) => shown(kind),
        // A sandbox stops the PDF viewer that Chromium builds in, so a PDF goes without one.
        Some(kind @ "application/pdf") => Media {
            content_type: kind,
            inline: true,
            policy: NOTHING,
        },
        Some(
            "text/plain"
            | "text/markdown"
            | "text/x-markdown"
            | "text/csv"
            | "text/tab-separated-values"
            | "application/json"
            | "text/x-yaml"
            | "text/x-toml",
        ) => shown(TEXT),
        _ => Media {
            content_type: BYTES,
            inline: false,
            policy: SANDBOXED,
        },
    }
}

/// The kinds of image the editing API takes, as [`of`] names them. SVG is not among them: it is
/// markup that can hold script, and a file sent in place should be one that cannot run.
pub const UPLOADS: [&str; 5] = [
    "image/png",
    "image/jpeg",
    "image/gif",
    "image/webp",
    "image/avif",
];

/// Which of [`UPLOADS`] `bytes` are, by the bytes each kind starts with, if any.
#[must_use]
pub fn image(bytes: &[u8]) -> Option<&'static str> {
    let brand = |at: usize| bytes.get(at..at + 4);
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.starts_with(b"RIFF") && brand(8) == Some(b"WEBP") {
        Some("image/webp")
    } else if brand(4) == Some(b"ftyp") && is_avif(bytes) {
        Some("image/avif")
    } else {
        None
    }
}

/// Whether the `ftyp` box that `bytes` start with names AVIF, a still image or a sequence, as
/// its major brand or one of its compatible ones.
fn is_avif(bytes: &[u8]) -> bool {
    let number = |at: usize, width: usize| {
        bytes
            .get(at..at + width)
            .map(|be| be.iter().fold(0_u64, |n, &byte| n << 8 | u64::from(byte)))
    };
    // A box's size is 32 bits, or, when that is 1, 64 bits after its type; 0 runs to the end.
    let (brands_at, size) = match number(0, 4) {
        Some(1) => (16, number(8, 8)),
        Some(0) => (8, Some(u64::MAX)),
        size => (8, size),
    };
    let Some(size) = size else { return false };
    let end = usize::try_from(size).unwrap_or(usize::MAX).min(bytes.len());
    // The major brand comes first, then a minor version, then the compatible brands.
    let minor = brands_at + 4;
    let mut brands = (brands_at..end.saturating_sub(3))
        .step_by(4)
        .filter(|&at| at != minor)
        .filter_map(|at| bytes.get(at..at + 4));
    end >= minor + 4 && brands.any(|brand| brand == b"avif" || brand == b"avis")
}

/// The `Content-Disposition` for the file at `path`: whether it shows in place, and its name.
/// A name that is not plain ASCII is also given in UTF-8, as RFC 6266 has it, beside a plain one
/// for a client that does not read that.
#[must_use]
pub fn disposition(media: Media, path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    let kind = if media.inline { "inline" } else { "attachment" };
    let plain: String = name
        .chars()
        .map(|c| match c {
            ' '..='~' if c != '"' && c != '\\' => c,
            _ => '_',
        })
        .collect();
    if plain == name {
        return format!("{kind}; filename=\"{plain}\"");
    }
    let mut encoded = String::with_capacity(name.len() * 3);
    for byte in name.bytes() {
        // The characters RFC 8187 lets stand for themselves.
        if byte.is_ascii_alphanumeric() || b"!#$&+-.^_`|~".contains(&byte) {
            encoded.push(char::from(byte));
        } else {
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    format!("{kind}; filename=\"{plain}\"; filename*=UTF-8''{encoded}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_what_cannot_run_in_a_browser_is_shown_in_place() {
        for (path, content_type) in [
            ("brand/logo.png", "image/png"),
            ("photo.JPG", "image/jpeg"),
            ("photo.jpeg", "image/jpeg"),
            ("a.gif", "image/gif"),
            ("a.webp", "image/webp"),
            ("a.avif", "image/avif"),
            ("brand/logo.svg", "image/svg+xml"),
            ("deck.pdf", "application/pdf"),
            ("talk.mp3", "audio/mpeg"),
            ("talk.m4a", "audio/m4a"),
            ("talk.aac", "audio/aac"),
            ("talk.ogg", "audio/ogg"),
            ("talk.oga", "audio/ogg"),
            ("talk.opus", "audio/ogg"),
            ("talk.wav", "audio/wav"),
            ("talk.flac", "audio/flac"),
            ("talk.weba", "audio/webm"),
            ("demo.mp4", "video/mp4"),
            ("demo.webm", "video/webm"),
            ("demo.ogv", "video/ogg"),
            ("demo.mov", "video/quicktime"),
            ("notes.txt", TEXT),
            ("page.md", TEXT),
            ("table.csv", TEXT),
            ("table.tsv", TEXT),
            ("data.json", TEXT),
            ("server.log", TEXT),
            ("config.yaml", TEXT),
            ("config.yml", TEXT),
            ("config.toml", TEXT),
            ("config.ini", TEXT),
            ("server.conf", TEXT),
        ] {
            let media = of(path);
            assert_eq!(
                (media.content_type, media.inline),
                (content_type, true),
                "{path}"
            );
        }
        for path in [
            "page.html",
            "page.htm",
            "page.xhtml",
            "page.shtml",
            "feed.xml",
            "feed.rss",
            "drawing.svgz.gz",
            "old.swf",
            "app.js",
            "app.mjs",
            "style.css",
            "kit.zip",
            "tool.wasm",
            "app.ts",
            "run.sh",
            "LICENSE",
            "no extension/Makefile",
            ".hidden",
            "odd.unknown-kind",
        ] {
            let media = of(path);
            assert_eq!((media.content_type, media.inline), (BYTES, false), "{path}");
        }
    }

    #[test]
    fn every_kind_but_a_pdf_is_sandboxed_and_none_may_load_or_run_anything() {
        for path in ["logo.svg", "logo.png", "page.html", "notes.txt", "kit.zip"] {
            assert_eq!(of(path).policy, SANDBOXED, "{path}");
        }
        assert_eq!(of("deck.pdf").policy, NOTHING);
    }

    #[test]
    fn an_image_is_known_by_its_first_bytes_and_svg_is_none() {
        let mut avif = b"\0\0\0\x1cftypmif1\0\0\0\0mif1avifmiaf".to_vec();
        avif.extend_from_slice(b"rest of the file");
        for (bytes, kind) in [
            (&b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR"[..], Some("image/png")),
            (b"\xff\xd8\xff\xe0\0\x10JFIF", Some("image/jpeg")),
            (b"GIF89a\x01\0\x01\0", Some("image/gif")),
            (b"GIF87a", Some("image/gif")),
            (b"RIFF\x24\0\0\0WEBPVP8 ", Some("image/webp")),
            (b"\0\0\0\x14ftypavif\0\0\0\0", Some("image/avif")),
            (b"\0\0\0\x14ftypavis\0\0\0\0", Some("image/avif")),
            (
                b"\0\0\0\x01ftyp\0\0\0\0\0\0\0\x1cmif1\0\0\0\0avif",
                Some("image/avif"),
            ),
            (b"\0\0\0\0ftypmif1\0\0\0\0miafavif", Some("image/avif")),
            (b"\0\0\0\x01ftyp\0\0\0\0\0\0\0\x18avif\0\0", None),
            (&avif, Some("image/avif")),
            (b"\0\0\0\x18ftypmif1\0\0\0\0heic", None),
            (b"\0\0\0\x18ftypisom\0\0\0\0mp41avc1", None),
            (b"\0\0\0\x10ftypheic\0\0\0\0avif", None),
            (b"RIFF\x24\0\0\0WAVEfmt ", None),
            (b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>", None),
            (b"<?xml version=\"1.0\"?><svg/>", None),
            (b"\x89PNG", None),
            (b"", None),
        ] {
            assert_eq!(image(bytes), kind, "{}", String::from_utf8_lossy(bytes));
        }
        for kind in UPLOADS {
            let path = format!("a.{}", kind.trim_start_matches("image/"));
            assert_eq!(of(&path).content_type, kind, "named by its extension");
        }
    }

    #[test]
    fn a_name_is_always_sent_and_encoded_when_it_is_not_plain() {
        for (path, inline, header) in [
            ("brand/kit.zip", false, r#"attachment; filename="kit.zip""#),
            ("logo.png", true, r#"inline; filename="logo.png""#),
            (
                "brand/Årets logga 2026.png",
                true,
                r#"inline; filename="_rets logga 2026.png"; filename*=UTF-8''%C3%85rets%20logga%202026.png"#,
            ),
            (
                "a \"quoted\\\" name.txt",
                true,
                r#"inline; filename="a _quoted__ name.txt"; filename*=UTF-8''a%20%22quoted%5C%22%20name.txt"#,
            ),
            (
                "new\nline.txt",
                true,
                r#"inline; filename="new_line.txt"; filename*=UTF-8''new%0Aline.txt"#,
            ),
        ] {
            let media = Media { inline, ..of(path) };
            assert_eq!(disposition(media, path), header, "{path}");
        }
    }
}
