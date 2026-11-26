//! pkt-line framing (git's wire format) and side-band multiplexing.
//!
//! A pkt-line is four lowercase hex digits giving the total length (prefix included) followed
//! by the payload. `0000` is a flush, `0001` a delimiter and `0002` a response end (protocol
//! v2). The longest packet is 65520 bytes, so a payload carries at most 65516 bytes.

use crate::error::{GitError, GitResult};

/// The longest payload of one packet.
pub const MAX_PAYLOAD: usize = 65516;
/// The most data one side-band-64k packet carries (one byte goes to the band number).
pub const MAX_SIDEBAND_DATA: usize = MAX_PAYLOAD - 1;

/// Side-band channel for pack data.
pub const BAND_DATA: u8 = 1;
/// Side-band channel for progress messages.
pub const BAND_PROGRESS: u8 = 2;
/// Side-band channel for a fatal error.
pub const BAND_ERROR: u8 = 3;

/// Appends one data packet. Payloads longer than [`MAX_PAYLOAD`] are split over several
/// packets (callers only send short lines; pack data goes through [`sideband`]).
pub fn data(out: &mut Vec<u8>, payload: &[u8]) {
    for chunk in payload.chunks(MAX_PAYLOAD) {
        out.extend_from_slice(format!("{:04x}", chunk.len() + 4).as_bytes());
        out.extend_from_slice(chunk);
    }
}

/// Appends one text packet.
pub fn text(out: &mut Vec<u8>, line: &str) {
    data(out, line.as_bytes());
}

/// Appends a flush packet.
pub fn flush(out: &mut Vec<u8>) {
    out.extend_from_slice(b"0000");
}

/// Appends a delimiter packet.
pub fn delim(out: &mut Vec<u8>) {
    out.extend_from_slice(b"0001");
}

/// Appends `data` on a side-band channel, split into packets.
pub fn sideband(out: &mut Vec<u8>, band: u8, payload: &[u8]) {
    for chunk in payload.chunks(MAX_SIDEBAND_DATA) {
        out.extend_from_slice(format!("{:04x}", chunk.len() + 5).as_bytes());
        out.push(band);
        out.extend_from_slice(chunk);
    }
}

/// One parsed packet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pkt<'a> {
    /// A data packet's payload.
    Data(&'a [u8]),
    /// `0000`.
    Flush,
    /// `0001`.
    Delim,
    /// `0002`.
    ResponseEnd,
}

impl<'a> Pkt<'a> {
    /// The payload of a data packet without one trailing newline; `None` for control packets.
    pub fn line(&self) -> Option<&'a [u8]> {
        match self {
            Pkt::Data(d) => Some(d.strip_suffix(b"\n").unwrap_or(d)),
            _ => None,
        }
    }
}

/// Reads packets from a buffer.
#[derive(Clone, Debug)]
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    /// A reader over `buf`.
    pub fn new(buf: &'a [u8]) -> Reader<'a> {
        Reader { buf, pos: 0 }
    }

    /// The next packet, or `None` at the end of the buffer.
    pub fn next_pkt(&mut self) -> GitResult<Option<Pkt<'a>>> {
        if self.pos == self.buf.len() {
            return Ok(None);
        }
        let head = self
            .buf
            .get(self.pos..self.pos + 4)
            .ok_or_else(|| GitError::invalid("truncated pkt-line length"))?;
        let text = std::str::from_utf8(head)
            .map_err(|_| GitError::invalid("pkt-line length is not hex"))?;
        let len = usize::from_str_radix(text, 16)
            .map_err(|_| GitError::invalid(format!("pkt-line length {text:?} is not hex")))?;
        let pkt = match len {
            0 => Pkt::Flush,
            1 => Pkt::Delim,
            2 => Pkt::ResponseEnd,
            3 => return Err(GitError::invalid("pkt-line length 3 is reserved")),
            n => {
                let body = self
                    .buf
                    .get(self.pos + 4..self.pos + n)
                    .ok_or_else(|| GitError::invalid("truncated pkt-line"))?;
                self.pos += n;
                return Ok(Some(Pkt::Data(body)));
            }
        };
        self.pos += 4;
        Ok(Some(pkt))
    }

    /// The next packet; the end of the buffer is an error.
    pub fn expect_pkt(&mut self) -> GitResult<Pkt<'a>> {
        self.next_pkt()?
            .ok_or_else(|| GitError::invalid("request ended early"))
    }

    /// Everything after the packets read so far (a pack follows the commands of a push).
    pub fn rest(&self) -> &'a [u8] {
        &self.buf[self.pos..]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let mut out = Vec::new();
        text(&mut out, "want abc\n");
        delim(&mut out);
        flush(&mut out);
        out.extend_from_slice(b"PACK");
        assert_eq!(&out[..13], b"000dwant abc\n");
        let mut r = Reader::new(&out);
        let p = r.expect_pkt().unwrap();
        assert_eq!(p, Pkt::Data(b"want abc\n"));
        assert_eq!(p.line(), Some(&b"want abc"[..]));
        assert_eq!(r.expect_pkt().unwrap(), Pkt::Delim);
        assert_eq!(r.expect_pkt().unwrap(), Pkt::Flush);
        assert_eq!(r.rest(), b"PACK");
    }

    #[test]
    fn rejects_bad_lengths() {
        assert!(Reader::new(b"zzzz").next_pkt().is_err());
        assert!(Reader::new(b"0003").next_pkt().is_err());
        assert!(Reader::new(b"0010abc").next_pkt().is_err());
        assert!(Reader::new(b"00").next_pkt().is_err());
        assert_eq!(Reader::new(b"").next_pkt().unwrap(), None);
        assert!(Reader::new(b"").expect_pkt().is_err());
    }

    #[test]
    fn sideband_splits_long_payloads() {
        let payload = vec![7u8; MAX_SIDEBAND_DATA + 10];
        let mut out = Vec::new();
        sideband(&mut out, BAND_DATA, &payload);
        let mut r = Reader::new(&out);
        let Pkt::Data(a) = r.expect_pkt().unwrap() else {
            panic!()
        };
        let Pkt::Data(b) = r.expect_pkt().unwrap() else {
            panic!()
        };
        assert_eq!((a[0], a.len()), (BAND_DATA, MAX_PAYLOAD));
        assert_eq!((b[0], b.len()), (BAND_DATA, 11));
    }
}
