//! Lowercase hexadecimal, the only hex form ids use (SPEC §3).

const DIGITS: &[u8; 16] = b"0123456789abcdef";

/// Encodes bytes as lowercase hex.
pub fn encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(DIGITS[(b >> 4) as usize] as char);
        out.push(DIGITS[(b & 0x0f) as usize] as char);
    }
    out
}

/// The value of one lowercase hex digit, or `None` for anything else (uppercase included).
pub fn nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    }
}

/// True when every byte is a lowercase hex digit.
pub fn is_lower_hex(text: &str) -> bool {
    text.bytes().all(|c| nibble(c).is_some())
}

/// Decodes lowercase hex of even length. Uppercase digits are rejected.
pub fn decode(text: &str) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    let (pairs, rest) = bytes.as_chunks::<2>();
    if !rest.is_empty() {
        return None;
    }
    pairs
        .iter()
        .map(|pair| Some((nibble(pair[0])? << 4) | nibble(pair[1])?))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trip() {
        let data: Vec<u8> = (0..=255).collect();
        let text = encode(&data);
        assert_eq!(text.len(), 512);
        assert!(is_lower_hex(&text));
        assert_eq!(decode(&text).unwrap(), data);
    }

    #[test]
    fn hex_rejects_bad_input() {
        assert_eq!(decode("abc"), None);
        assert_eq!(decode("AB"), None);
        assert_eq!(decode("zz"), None);
        assert_eq!(decode(""), Some(vec![]));
        assert!(!is_lower_hex("aF"));
        assert_eq!(nibble(b'g'), None);
        assert_eq!(nibble(b'f'), Some(15));
    }
}
