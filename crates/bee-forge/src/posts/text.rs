//! Post text rules, as pure functions.

use unicode_normalization::UnicodeNormalization;

use super::MAX_TEXT_CHARS;
use crate::error::{ForgeError, Result};

/// The text as stored: NFC, at most [`MAX_TEXT_CHARS`] Unicode scalar values, and no control
/// character other than `\n`. `has_media` allows empty text.
pub fn normalize(text: &str, has_media: bool) -> Result<String> {
    let nfc: String = text.nfc().collect();
    if let Some(c) = nfc.chars().find(|c| c.is_control() && *c != '\n') {
        return Err(ForgeError::invalid(
            "bad_text",
            format!(
                "post text cannot contain the control character U+{:04X}",
                u32::from(c)
            ),
        ));
    }
    let n = nfc.chars().count();
    if n > MAX_TEXT_CHARS {
        return Err(ForgeError::invalid(
            "text_too_long",
            format!("post text is {n} characters; at most {MAX_TEXT_CHARS} are allowed"),
        ));
    }
    if nfc.trim().is_empty() && !has_media {
        return Err(ForgeError::invalid(
            "empty_post",
            "a post needs text or media",
        ));
    }
    Ok(nfc)
}

/// Alt text of an image: 1 to 1000 characters, no control characters other than `\n`.
pub fn alt_text(alt: &str, required: bool) -> Result<String> {
    let (min, max) = super::ALT_TEXT_CHARS;
    let nfc: String = alt.trim().nfc().collect();
    let n = nfc.chars().count();
    if n == 0 && !required {
        return Ok(nfc);
    }
    if n < min {
        return Err(ForgeError::invalid(
            "alt_text_required",
            "images need alt text that describes them",
        ));
    }
    if n > max {
        return Err(ForgeError::invalid(
            "alt_text_too_long",
            format!("alt text is {n} characters; at most {max} are allowed"),
        ));
    }
    if nfc.chars().any(|c| c.is_control() && c != '\n') {
        return Err(ForgeError::invalid(
            "bad_text",
            "alt text cannot contain control characters",
        ));
    }
    Ok(nfc)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_is_nfc_bounded_and_plain() {
        // "e" + combining acute becomes the precomposed "é".
        assert_eq!(normalize("caf\u{65}\u{301}", false).unwrap(), "caf\u{e9}");
        assert_eq!(
            normalize("line one\nline two", false).unwrap(),
            "line one\nline two"
        );
        assert_eq!(
            normalize(&"é".repeat(500), false).unwrap().chars().count(),
            500
        );
        assert_eq!(
            normalize(&"x".repeat(501), false).unwrap_err().subcode,
            "text_too_long"
        );
        // Counted after normalization: 500 decomposed "é" are 500 characters, not 1000.
        assert!(normalize(&"e\u{301}".repeat(500), false).is_ok());
        for bad in ["tab\there", "cr\r\n", "nul\0", "esc\u{1b}[31m", "\u{85}"] {
            assert_eq!(
                normalize(bad, false).unwrap_err().subcode,
                "bad_text",
                "{bad:?}"
            );
        }
        assert_eq!(normalize("", false).unwrap_err().subcode, "empty_post");
        assert_eq!(normalize(" \n ", false).unwrap_err().subcode, "empty_post");
        assert_eq!(normalize("", true).unwrap(), "");
    }

    #[test]
    fn alt_text_rules() {
        assert_eq!(alt_text("", true).unwrap_err().subcode, "alt_text_required");
        assert_eq!(
            alt_text("   ", true).unwrap_err().subcode,
            "alt_text_required"
        );
        assert_eq!(alt_text("", false).unwrap(), "");
        assert_eq!(alt_text(" a chart ", true).unwrap(), "a chart");
        assert_eq!(alt_text(&"a".repeat(1000), true).unwrap().len(), 1000);
        assert_eq!(
            alt_text(&"a".repeat(1001), true).unwrap_err().subcode,
            "alt_text_too_long"
        );
    }
}
