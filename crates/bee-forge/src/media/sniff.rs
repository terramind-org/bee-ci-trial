//! Content sniffing and image dimensions: pure functions of the bytes.
//!
//! The kind of a binary upload (images, video, audio, PDF) is decided by its magic bytes
//! alone, never by the content type the client declared or a file name, so nothing a browser
//! could run (SVG, HTML, JavaScript) is ever accepted: none of them starts with one of the
//! signatures below. Text kinds (plain text, Markdown, CSV, WebVTT) have no signature, so
//! [`sniff_as`] accepts them only as declared and only when the bytes are inert text: UTF-8
//! with no control characters but tab, line feed, form feed and carriage return, and not
//! starting with `<` (which refuses HTML, SVG and XML). A script declared as plain text stays
//! plain text: it is served as `text/plain` with `nosniff`, a sandbox CSP and
//! `Content-Disposition: attachment` ([`Kind::is_inline`]), so nothing runs it.

/// A media kind the forge accepts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    /// PNG.
    Png,
    /// JPEG (JFIF or Exif).
    Jpeg,
    /// GIF 87a or 89a.
    Gif,
    /// WebP (lossy, lossless or extended).
    Webp,
    /// MP4 (ISO base media, an `ftyp` box first).
    Mp4,
    /// WebM (EBML with doc type `webm`).
    Webm,
    /// MPEG audio (MP3: an ID3 tag or an MPEG audio frame header first).
    Mpeg,
    /// Ogg (`OggS`).
    Ogg,
    /// WAV (RIFF with form type `WAVE`).
    Wav,
    /// PDF (`%PDF-`).
    Pdf,
    /// Plain text (no signature; declared, and inert text).
    Text,
    /// Markdown (no signature; declared, and inert text).
    Markdown,
    /// CSV (no signature; declared, and inert text).
    Csv,
    /// WebVTT captions (inert text starting with `WEBVTT`).
    Vtt,
}

/// Every kind, in a fixed order.
pub const ALL_KINDS: &[Kind] = &[
    Kind::Png,
    Kind::Jpeg,
    Kind::Gif,
    Kind::Webp,
    Kind::Mp4,
    Kind::Webm,
    Kind::Mpeg,
    Kind::Ogg,
    Kind::Wav,
    Kind::Pdf,
    Kind::Text,
    Kind::Markdown,
    Kind::Csv,
    Kind::Vtt,
];

impl Kind {
    /// The content type the forge serves this kind with.
    pub fn content_type(self) -> &'static str {
        match self {
            Kind::Png => "image/png",
            Kind::Jpeg => "image/jpeg",
            Kind::Gif => "image/gif",
            Kind::Webp => "image/webp",
            Kind::Mp4 => "video/mp4",
            Kind::Webm => "video/webm",
            Kind::Mpeg => "audio/mpeg",
            Kind::Ogg => "audio/ogg",
            Kind::Wav => "audio/wav",
            Kind::Pdf => "application/pdf",
            Kind::Text => "text/plain",
            Kind::Markdown => "text/markdown",
            Kind::Csv => "text/csv",
            Kind::Vtt => "text/vtt",
        }
    }

    /// The kind a declared content type names (parameters and case are ignored; `image/jpg`
    /// is accepted as JPEG, `audio/mp3` as MPEG audio, `audio/x-wav` and `audio/wave` as WAV,
    /// `text/x-markdown` as Markdown). `None` for anything else, SVG, HTML and JavaScript
    /// included.
    pub fn from_content_type(ct: &str) -> Option<Kind> {
        let base = ct
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        match base.as_str() {
            "image/png" => Some(Kind::Png),
            "image/jpeg" | "image/jpg" => Some(Kind::Jpeg),
            "image/gif" => Some(Kind::Gif),
            "image/webp" => Some(Kind::Webp),
            "video/mp4" => Some(Kind::Mp4),
            "video/webm" => Some(Kind::Webm),
            "audio/mpeg" | "audio/mp3" => Some(Kind::Mpeg),
            "audio/ogg" => Some(Kind::Ogg),
            "audio/wav" | "audio/x-wav" | "audio/wave" => Some(Kind::Wav),
            "application/pdf" => Some(Kind::Pdf),
            "text/plain" => Some(Kind::Text),
            "text/markdown" | "text/x-markdown" => Some(Kind::Markdown),
            "text/csv" => Some(Kind::Csv),
            "text/vtt" => Some(Kind::Vtt),
            _ => None,
        }
    }

    /// Whether the kind is a still or animated image (dimensions are read from its header).
    pub fn is_image(self) -> bool {
        matches!(self, Kind::Png | Kind::Jpeg | Kind::Gif | Kind::Webp)
    }

    /// Whether the kind is a video.
    pub fn is_video(self) -> bool {
        matches!(self, Kind::Mp4 | Kind::Webm)
    }

    /// Whether the kind is audio.
    pub fn is_audio(self) -> bool {
        matches!(self, Kind::Mpeg | Kind::Ogg | Kind::Wav)
    }

    /// Whether the kind is text without a signature, accepted only as declared.
    pub fn is_text(self) -> bool {
        matches!(self, Kind::Text | Kind::Markdown | Kind::Csv | Kind::Vtt)
    }

    /// Whether a browser may show the kind in the page (images, video, audio). Everything
    /// else is served with `Content-Disposition: attachment`.
    pub fn is_inline(self) -> bool {
        self.is_image() || self.is_video() || self.is_audio()
    }
}

/// The kind of `bytes` by their magic bytes, or `None` when they carry no known signature
/// (text kinds never sniff: see [`sniff_as`]).
pub fn sniff(bytes: &[u8]) -> Option<Kind> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some(Kind::Png);
    }
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some(Kind::Jpeg);
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some(Kind::Gif);
    }
    if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some(Kind::Webp);
    }
    if bytes.len() >= 12 && &bytes[4..8] == b"ftyp" {
        return Some(Kind::Mp4);
    }
    if bytes.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        // EBML is shared with Matroska; only the `webm` doc type is WebM.
        let head = &bytes[..bytes.len().min(64)];
        if head.windows(4).any(|w| w == b"webm") {
            return Some(Kind::Webm);
        }
    }
    if bytes.starts_with(b"OggS") {
        return Some(Kind::Ogg);
    }
    if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WAVE" {
        return Some(Kind::Wav);
    }
    if bytes.starts_with(b"%PDF-") {
        return Some(Kind::Pdf);
    }
    if bytes.starts_with(b"ID3") || mpeg_frame(bytes) {
        return Some(Kind::Mpeg);
    }
    None
}

/// An MPEG audio frame header: 11 sync bits, a version other than the reserved one, a layer
/// other than the reserved one, and a bitrate and sample rate that are not reserved.
fn mpeg_frame(b: &[u8]) -> bool {
    let (Some(&b0), Some(&b1), Some(&b2)) = (b.first(), b.get(1), b.get(2)) else {
        return false;
    };
    b0 == 0xFF
        && b1 & 0xE0 == 0xE0
        && (b1 >> 3) & 0x03 != 0x01
        && (b1 >> 1) & 0x03 != 0x00
        && b2 >> 4 != 0x0F
        && (b2 >> 2) & 0x03 != 0x03
}

/// Whether `bytes` are inert text: UTF-8 (an optional byte-order mark first), no control
/// characters but tab, line feed, form feed and carriage return, and the first character that
/// is not white space is not `<`.
pub fn inert_text(bytes: &[u8]) -> bool {
    let body = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    let Ok(text) = std::str::from_utf8(body) else {
        return false;
    };
    if text
        .chars()
        .any(|c| c.is_control() && !matches!(c, '\t' | '\n' | '\x0C' | '\r'))
    {
        return false;
    }
    !text.trim_start().starts_with('<')
}

/// The kind of `bytes` uploaded as `declared`, or `None` when they are not that kind. Binary
/// kinds must sniff as `declared`; text kinds must be [`inert_text`] that carries no binary
/// signature, and WebVTT must start with `WEBVTT` followed by white space or the end.
pub fn sniff_as(declared: Kind, bytes: &[u8]) -> Option<Kind> {
    if !declared.is_text() {
        return (sniff(bytes) == Some(declared)).then_some(declared);
    }
    if sniff(bytes).is_some() || !inert_text(bytes) {
        return None;
    }
    if declared == Kind::Vtt {
        let body = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
        let rest = body.strip_prefix(b"WEBVTT")?;
        if !matches!(rest.first(), None | Some(b' ' | b'\t' | b'\n' | b'\r')) {
            return None;
        }
    }
    Some(declared)
}

fn be16(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from(*b.get(at)?) << 8 | u32::from(*b.get(at + 1)?))
}

fn le16(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from(*b.get(at)?) | u32::from(*b.get(at + 1)?) << 8)
}

fn le24(b: &[u8], at: usize) -> Option<u32> {
    Some(le16(b, at)? | u32::from(*b.get(at + 2)?) << 16)
}

fn be32(b: &[u8], at: usize) -> Option<u32> {
    Some(be16(b, at)? << 16 | be16(b, at + 2)?)
}

/// Width and height of an image read from its header; `None` for videos, truncated headers
/// and zero dimensions.
pub fn image_size(kind: Kind, b: &[u8]) -> Option<(u32, u32)> {
    let (w, h) = match kind {
        Kind::Png => {
            if b.get(12..16)? != b"IHDR" {
                return None;
            }
            (be32(b, 16)?, be32(b, 20)?)
        }
        Kind::Gif => (le16(b, 6)?, le16(b, 8)?),
        Kind::Webp => webp_size(b)?,
        Kind::Jpeg => jpeg_size(b)?,
        _ => return None,
    };
    (w > 0 && h > 0).then_some((w, h))
}

fn webp_size(b: &[u8]) -> Option<(u32, u32)> {
    match b.get(12..16)? {
        b"VP8 " => {
            // Lossy: a key frame's start code, then 14-bit width and height.
            if b.get(23..26)? != [0x9d, 0x01, 0x2a] {
                return None;
            }
            Some((le16(b, 26)? & 0x3FFF, le16(b, 28)? & 0x3FFF))
        }
        b"VP8L" => {
            // Lossless: signature 0x2f, then width-1 and height-1 in 14 bits each.
            if *b.get(20)? != 0x2f {
                return None;
            }
            let (b0, b1, b2, b3) = (
                u32::from(*b.get(21)?),
                u32::from(*b.get(22)?),
                u32::from(*b.get(23)?),
                u32::from(*b.get(24)?),
            );
            let w = 1 + (b0 | (b1 & 0x3F) << 8);
            let h = 1 + ((b1 & 0xC0) >> 6 | b2 << 2 | (b3 & 0x0F) << 10);
            Some((w, h))
        }
        b"VP8X" => Some((1 + le24(b, 24)?, 1 + le24(b, 27)?)),
        _ => None,
    }
}

fn jpeg_size(b: &[u8]) -> Option<(u32, u32)> {
    let mut i = 2;
    loop {
        // Markers may be padded with any number of 0xFF fill bytes.
        if *b.get(i)? != 0xFF {
            return None;
        }
        while *b.get(i)? == 0xFF {
            i += 1;
        }
        let marker = *b.get(i)?;
        i += 1;
        match marker {
            // Standalone markers carry no length.
            0x01 | 0xD0..=0xD8 => continue,
            0xD9 | 0xDA => return None,
            _ => {}
        }
        let len = be16(b, i)? as usize;
        if len < 2 {
            return None;
        }
        // Start of frame (every SOFn but DHT, JPG and DAC).
        if (0xC0..=0xCF).contains(&marker) && !matches!(marker, 0xC4 | 0xC8 | 0xCC) {
            return Some((be16(b, i + 5)?, be16(b, i + 3)?));
        }
        i += len;
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A minimal PNG header of `w` × `h` (signature and IHDR; enough for sniffing).
    pub(crate) fn png(w: u32, h: u32) -> Vec<u8> {
        let mut v = b"\x89PNG\r\n\x1a\n".to_vec();
        v.extend_from_slice(&13u32.to_be_bytes());
        v.extend_from_slice(b"IHDR");
        v.extend_from_slice(&w.to_be_bytes());
        v.extend_from_slice(&h.to_be_bytes());
        v.extend_from_slice(&[8, 6, 0, 0, 0, 0, 0, 0, 0]);
        v
    }

    fn jpeg(w: u16, h: u16) -> Vec<u8> {
        let mut v = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x04, 0x4A, 0x46];
        // A fill byte before SOF0.
        v.extend_from_slice(&[0xFF, 0xFF, 0xC0, 0x00, 0x11, 0x08]);
        v.extend_from_slice(&h.to_be_bytes());
        v.extend_from_slice(&w.to_be_bytes());
        v.extend_from_slice(&[3, 1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1]);
        v
    }

    fn webp(chunk: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut v = b"RIFF\0\0\0\0WEBP".to_vec();
        v.extend_from_slice(chunk);
        v.extend_from_slice(&(body.len() as u32).to_le_bytes());
        v.extend_from_slice(body);
        v
    }

    #[test]
    fn sniffs_every_kind_and_nothing_else() {
        assert_eq!(sniff(&png(1, 1)), Some(Kind::Png));
        assert_eq!(sniff(&jpeg(1, 1)), Some(Kind::Jpeg));
        assert_eq!(sniff(b"GIF89a\x02\x00\x03\x00"), Some(Kind::Gif));
        assert_eq!(sniff(&webp(b"VP8X", &[0; 10])), Some(Kind::Webp));
        assert_eq!(sniff(b"\0\0\0\x18ftypisom\0\0\0\0"), Some(Kind::Mp4));
        let mut webm = vec![0x1A, 0x45, 0xDF, 0xA3, 0x9F, 0x42, 0x82, 0x84];
        webm.extend_from_slice(b"webm");
        assert_eq!(sniff(&webm), Some(Kind::Webm));
        let mut mkv = vec![0x1A, 0x45, 0xDF, 0xA3, 0x9F, 0x42, 0x82, 0x88];
        mkv.extend_from_slice(b"matroska");
        assert_eq!(sniff(&mkv), None, "Matroska is not WebM");
        for refused in [
            &b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>"[..],
            b"<?xml version=\"1.0\"?><svg/>",
            b"<!doctype html><script>alert(1)</script>",
            b"alert(document.cookie)",
            b"",
            b"\x89PN",
        ] {
            assert_eq!(
                sniff(refused),
                None,
                "{:?}",
                String::from_utf8_lossy(refused)
            );
        }
    }

    #[test]
    fn sniffs_audio_and_pdf() {
        assert_eq!(sniff(b"ID3\x04\0\0\0\0\0\0"), Some(Kind::Mpeg));
        assert_eq!(sniff(&[0xFF, 0xFB, 0x90, 0x64, 0, 0]), Some(Kind::Mpeg));
        // A reserved layer is no MPEG audio frame.
        assert_eq!(sniff(&[0xFF, 0xF9, 0x90, 0x64]), None);
        assert_eq!(sniff(b"OggS\0\x02\0\0"), Some(Kind::Ogg));
        assert_eq!(sniff(b"RIFF\x24\0\0\0WAVEfmt "), Some(Kind::Wav));
        assert_eq!(sniff(b"%PDF-1.7\n%\xE2\xE3"), Some(Kind::Pdf));
        // Text has no signature.
        assert_eq!(sniff(b"WEBVTT\n\n00:00.000 --> 00:01.000\nHi"), None);
        assert_eq!(sniff(b"name,ms\nload,120\n"), None);
    }

    #[test]
    fn text_kinds_need_inert_text_as_declared() {
        assert_eq!(
            sniff_as(Kind::Text, b"All 214 tests pass\n"),
            Some(Kind::Text)
        );
        assert_eq!(
            sniff_as(Kind::Markdown, b"# Report\n\n- ok\n"),
            Some(Kind::Markdown)
        );
        assert_eq!(
            sniff_as(Kind::Csv, b"\xEF\xBB\xBFname,ms\r\nload,120\r\n"),
            Some(Kind::Csv)
        );
        assert_eq!(
            sniff_as(Kind::Vtt, b"WEBVTT\n\n00:00.000 --> 00:01.000\nHello\n"),
            Some(Kind::Vtt)
        );
        assert_eq!(sniff_as(Kind::Vtt, b"WEBVTTX\n"), None);
        assert_eq!(sniff_as(Kind::Vtt, b"Hello\n"), None);
        for markup in [
            &b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>"[..],
            b"  \n<!doctype html><script>alert(1)</script>",
            b"<?xml version=\"1.0\"?><svg/>",
            b"\xEF\xBB\xBF<html>",
        ] {
            for k in [Kind::Text, Kind::Markdown, Kind::Csv, Kind::Vtt] {
                assert_eq!(sniff_as(k, markup), None, "{k:?}");
            }
        }
        // Binary bytes, control characters and signatures are no text.
        assert_eq!(sniff_as(Kind::Text, b"a\0b"), None);
        assert_eq!(sniff_as(Kind::Text, b"\xFF\xFE"), None);
        assert_eq!(sniff_as(Kind::Text, &png(1, 1)), None);
        assert_eq!(sniff_as(Kind::Text, b"%PDF-1.4"), None);
        // Binary kinds must sniff as declared.
        assert_eq!(sniff_as(Kind::Png, &png(1, 1)), Some(Kind::Png));
        assert_eq!(sniff_as(Kind::Jpeg, &png(1, 1)), None);
        assert_eq!(sniff_as(Kind::Pdf, b"%PDF-1.4"), Some(Kind::Pdf));
        assert!(Kind::Png.is_inline() && Kind::Webm.is_inline() && Kind::Ogg.is_inline());
        assert!(!Kind::Pdf.is_inline() && !Kind::Vtt.is_inline() && !Kind::Csv.is_inline());
    }

    #[test]
    fn content_types_map_to_kinds() {
        assert_eq!(Kind::from_content_type("IMAGE/PNG"), Some(Kind::Png));
        assert_eq!(Kind::from_content_type("image/jpg"), Some(Kind::Jpeg));
        assert_eq!(
            Kind::from_content_type("video/webm; codecs=vp9"),
            Some(Kind::Webm)
        );
        assert_eq!(Kind::from_content_type("image/svg+xml"), None);
        assert_eq!(Kind::from_content_type("text/html"), None);
        assert_eq!(Kind::from_content_type("text/javascript"), None);
        assert_eq!(Kind::from_content_type("application/javascript"), None);
        assert_eq!(Kind::from_content_type("audio/x-wav"), Some(Kind::Wav));
        assert_eq!(
            Kind::from_content_type("text/plain; charset=utf-8"),
            Some(Kind::Text)
        );
        for k in ALL_KINDS {
            assert_eq!(Kind::from_content_type(k.content_type()), Some(*k));
        }
    }

    #[test]
    fn reads_image_dimensions() {
        assert_eq!(image_size(Kind::Png, &png(640, 480)), Some((640, 480)));
        assert_eq!(image_size(Kind::Png, &png(0, 480)), None);
        assert_eq!(image_size(Kind::Jpeg, &jpeg(1200, 630)), Some((1200, 630)));
        assert_eq!(
            image_size(Kind::Gif, b"GIF89a\x40\x01\xF0\x00"),
            Some((320, 240))
        );
        // Lossy: frame tag, start code, 14-bit sizes.
        let lossy = webp(
            b"VP8 ",
            &[0, 0, 0, 0x9d, 0x01, 0x2a, 0x80, 0x02, 0xE0, 0x01],
        );
        assert_eq!(image_size(Kind::Webp, &lossy), Some((640, 480)));
        // Lossless: 0x2f, then width-1 = 99, height-1 = 49.
        let (w1, h1) = (99u32, 49u32);
        let bits = w1 | h1 << 14;
        let lossless = webp(
            b"VP8L",
            &[
                0x2f,
                bits as u8,
                (bits >> 8) as u8,
                (bits >> 16) as u8,
                (bits >> 24) as u8,
            ],
        );
        assert_eq!(image_size(Kind::Webp, &lossless), Some((100, 50)));
        let ext = webp(b"VP8X", &[0, 0, 0, 0, 0x1F, 0x03, 0, 0xDF, 0x01, 0]);
        assert_eq!(image_size(Kind::Webp, &ext), Some((800, 480)));
        assert_eq!(image_size(Kind::Mp4, b"\0\0\0\x18ftypisom"), None);
        assert_eq!(image_size(Kind::Jpeg, &[0xFF, 0xD8, 0xFF]), None);
    }
}
