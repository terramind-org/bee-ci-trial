//! The line model.

/// Splits `text` into lines on `\n`, keeping each terminator with its line.
///
/// No other byte is special: `\r\n` stays as the two bytes it is, so CRLF and LF files differ
/// line by line. The last line has no terminator when the text does not end in `\n`. Empty text
/// has no lines.
///
/// ```
/// let lines = bee_merge::split_lines(b"a\r\nb\nc");
/// assert_eq!(lines, vec![&b"a\r\n"[..], b"b\n", b"c"]);
/// ```
pub fn split_lines(text: &[u8]) -> Vec<&[u8]> {
    let mut lines = Vec::new();
    let mut start = 0;
    for (i, &byte) in text.iter().enumerate() {
        if byte == b'\n' {
            lines.push(&text[start..=i]);
            start = i + 1;
        }
    }
    if start < text.len() {
        lines.push(&text[start..]);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::split_lines;

    #[test]
    fn empty_text_has_no_lines() {
        assert!(split_lines(b"").is_empty());
    }

    #[test]
    fn terminators_are_kept_and_crlf_is_not_normalized() {
        assert_eq!(split_lines(b"\n"), vec![&b"\n"[..]]);
        assert_eq!(
            split_lines(b"a\r\n\r\nb"),
            vec![&b"a\r\n"[..], b"\r\n", b"b"]
        );
    }

    #[test]
    fn concatenation_round_trips() {
        let text = b"x\n\ny\rz\n\nlast";
        assert_eq!(split_lines(text).concat(), text.to_vec());
    }
}
