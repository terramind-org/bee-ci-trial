//! Trigrams: the unit of the code index.
//!
//! A trigram is three consecutive bytes of a file with ASCII letters folded to lower case,
//! packed big-endian into a `u32`. Folding makes case-insensitive search as cheap as
//! case-sensitive search; the verifier (a real regex run over each candidate) restores exact
//! semantics. Every window is indexed, newlines included, so a pattern that spans lines
//! still plans soundly.

/// Three ASCII-lower-cased bytes packed into the low 24 bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Trigram(pub u32);

impl Trigram {
    /// Packs three bytes, folding ASCII case.
    pub fn new(a: u8, b: u8, c: u8) -> Trigram {
        Trigram(
            (u32::from(a.to_ascii_lowercase()) << 16)
                | (u32::from(b.to_ascii_lowercase()) << 8)
                | u32::from(c.to_ascii_lowercase()),
        )
    }

    /// The three (folded) bytes.
    pub fn bytes(self) -> [u8; 3] {
        [(self.0 >> 16) as u8, (self.0 >> 8) as u8, self.0 as u8]
    }
}

/// Every distinct trigram of `bytes`, sorted ascending.
pub fn trigrams_of(bytes: &[u8]) -> Vec<Trigram> {
    let mut out: Vec<Trigram> = bytes
        .windows(3)
        .map(|w| Trigram::new(w[0], w[1], w[2]))
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// Folds ASCII letters of `bytes` to lower case (the index's view of text).
pub fn fold(bytes: &[u8]) -> Vec<u8> {
    bytes.iter().map(u8::to_ascii_lowercase).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packs_and_folds() {
        let t = Trigram::new(b'A', b'b', b'C');
        assert_eq!(t, Trigram::new(b'a', b'B', b'c'));
        assert_eq!(t.bytes(), *b"abc");
        assert_eq!(t.0, 0x0061_6263);
    }

    #[test]
    fn distinct_sorted() {
        let t = trigrams_of(b"abcabc");
        assert_eq!(
            t,
            vec![
                Trigram::new(b'a', b'b', b'c'),
                Trigram::new(b'b', b'c', b'a'),
                Trigram::new(b'c', b'a', b'b'),
            ]
        );
        assert!(trigrams_of(b"ab").is_empty());
        assert_eq!(trigrams_of(b"a\nb").len(), 1);
        assert_eq!(fold(b"AbC\xff"), b"abc\xff");
    }
}
