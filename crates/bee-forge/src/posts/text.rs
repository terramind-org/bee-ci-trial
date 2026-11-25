//! Post text rules, as pure functions.

use bee_proto::v1::PostArticle;
use unicode_normalization::UnicodeNormalization;

use super::{MAX_ARTICLE_CHARS, MAX_ARTICLE_TITLE_CHARS, MAX_BODY_CHARS, MAX_TEXT_CHARS};
use crate::error::{ForgeError, Result};

/// The text as stored: NFC, at most [`MAX_TEXT_CHARS`] Unicode scalar values, and no control
/// character other than `\n`. `has_media` (the post has media, a body or an article) allows
/// empty text.
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
            "a post needs text, a body, an article or media",
        ));
    }
    Ok(nfc)
}

/// Markdown as stored: NFC with `\n` line ends, at most `max` Unicode scalar values (else
/// `too_long`), and no control character other than `\n` and tab.
fn markdown(md: &str, max: usize, too_long: &str, what: &str) -> Result<String> {
    let nfc: String = md.replace("\r\n", "\n").nfc().collect();
    if let Some(c) = nfc
        .chars()
        .find(|c| c.is_control() && *c != '\n' && *c != '\t')
    {
        return Err(ForgeError::invalid(
            "bad_text",
            format!(
                "{what} cannot contain the control character U+{:04X}",
                u32::from(c)
            ),
        ));
    }
    let n = nfc.chars().count();
    if n > max {
        return Err(ForgeError::invalid(
            too_long,
            format!("{what} is {n} characters; at most {max} are allowed"),
        ));
    }
    Ok(nfc)
}

/// The Markdown body as stored (see [`MAX_BODY_CHARS`]); one of only white space is no body.
pub fn body(md: &str) -> Result<String> {
    let body = markdown(md, MAX_BODY_CHARS, "body_too_long", "the post body")?;
    Ok(if body.trim().is_empty() {
        String::new()
    } else {
        body
    })
}

/// The article as stored, `None` when the post has none (unset, or title and body both
/// empty). An article needs a title of 1 to [`MAX_ARTICLE_TITLE_CHARS`] characters on one
/// line and a body (`bad_article`) of at most [`MAX_ARTICLE_CHARS`] (`article_too_long`).
pub fn article(article: Option<&PostArticle>) -> Result<Option<PostArticle>> {
    let Some(a) = article else {
        return Ok(None);
    };
    let title: String = a.title.trim().nfc().collect();
    if title.is_empty() && a.body_markdown.trim().is_empty() {
        return Ok(None);
    }
    let n = title.chars().count();
    if n == 0 || n > MAX_ARTICLE_TITLE_CHARS {
        return Err(ForgeError::invalid(
            "bad_article",
            format!("an article needs a title of 1 to {MAX_ARTICLE_TITLE_CHARS} characters"),
        ));
    }
    if title.chars().any(char::is_control) {
        return Err(ForgeError::invalid(
            "bad_article",
            "an article title is one line without control characters",
        ));
    }
    let body_markdown = markdown(
        &a.body_markdown,
        MAX_ARTICLE_CHARS,
        "article_too_long",
        "the article",
    )?;
    if body_markdown.trim().is_empty() {
        return Err(ForgeError::invalid(
            "bad_article",
            "an article needs a body",
        ));
    }
    Ok(Some(PostArticle {
        title,
        body_markdown,
    }))
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
    fn body_is_markdown_with_a_limit() {
        assert_eq!(body("").unwrap(), "");
        assert_eq!(body(" \n\t").unwrap(), "");
        // Line ends become `\n`, tabs and NFC text stay.
        assert_eq!(
            body("# Title\r\n\r\n\tcode cafe\u{301}\n").unwrap(),
            "# Title\n\n\tcode caf\u{e9}\n"
        );
        assert_eq!(
            body(&"x".repeat(MAX_BODY_CHARS)).unwrap().len(),
            MAX_BODY_CHARS
        );
        assert_eq!(
            body(&"x".repeat(MAX_BODY_CHARS + 1)).unwrap_err().subcode,
            "body_too_long"
        );
        for bad in ["bare\rreturn", "nul\0", "esc\u{1b}[31m"] {
            assert_eq!(body(bad).unwrap_err().subcode, "bad_text", "{bad:?}");
        }
    }

    #[test]
    fn article_needs_a_title_and_a_body() {
        let art = |title: &str, body: &str| {
            article(Some(&PostArticle {
                title: title.into(),
                body_markdown: body.into(),
            }))
        };
        assert_eq!(article(None).unwrap(), None);
        assert_eq!(art(" ", "\n").unwrap(), None);
        let a = art("  How we cut build time  ", "## Before\r\nSlow.\n")
            .unwrap()
            .unwrap();
        assert_eq!(a.title, "How we cut build time");
        assert_eq!(a.body_markdown, "## Before\nSlow.\n");
        assert_eq!(art("", "words").unwrap_err().subcode, "bad_article");
        assert_eq!(art("Title", " ").unwrap_err().subcode, "bad_article");
        assert_eq!(
            art("two\nlines", "words").unwrap_err().subcode,
            "bad_article"
        );
        assert_eq!(
            art(&"t".repeat(MAX_ARTICLE_TITLE_CHARS + 1), "words")
                .unwrap_err()
                .subcode,
            "bad_article"
        );
        assert!(art(&"t".repeat(MAX_ARTICLE_TITLE_CHARS), "words").is_ok());
        assert_eq!(
            art("Title", &"x".repeat(MAX_ARTICLE_CHARS + 1))
                .unwrap_err()
                .subcode,
            "article_too_long"
        );
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
