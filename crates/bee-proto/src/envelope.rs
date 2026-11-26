//! Length-prefixed message frames.
//!
//! gRPC and Connect streaming both frame each message as one flags byte, a 4-byte big-endian
//! length and the payload. Connect uses flag bit 1 (`END_STREAM`) for its final JSON frame.

use std::fmt;

use bytes::{Buf, BufMut, Bytes, BytesMut};

/// The payload is compressed (rejected by the gateway: only identity encoding is served).
pub const FLAG_COMPRESSED: u8 = 0b0000_0001;
/// Connect end-of-stream frame, carrying the end-stream JSON object.
pub const FLAG_END_STREAM: u8 = 0b0000_0010;

/// Length of the frame header.
pub const HEADER_LEN: usize = 5;

/// One frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    /// Flag bits.
    pub flags: u8,
    /// Payload bytes.
    pub data: Bytes,
}

/// A malformed frame stream.
#[derive(Debug, PartialEq, Eq)]
pub enum EnvelopeError {
    /// A frame declares more bytes than the limit allows.
    TooLarge {
        /// Declared payload length.
        length: usize,
        /// Configured limit.
        limit: usize,
    },
    /// The stream ended inside a frame.
    Truncated,
}

impl fmt::Display for EnvelopeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge { length, limit } => {
                write!(f, "frame of {length} bytes exceeds the {limit} byte limit")
            }
            Self::Truncated => f.write_str("stream ended inside a frame"),
        }
    }
}

impl std::error::Error for EnvelopeError {}

/// Encodes one frame.
pub fn encode(flags: u8, payload: &[u8]) -> Bytes {
    let mut out = BytesMut::with_capacity(HEADER_LEN + payload.len());
    out.put_u8(flags);
    // Payloads never exceed u32 here: the gateway caps request and message sizes far below it.
    out.put_u32(u32::try_from(payload.len()).unwrap_or(u32::MAX));
    out.put_slice(payload);
    out.freeze()
}

/// Incremental frame decoder: push bytes as they arrive, pull complete frames.
#[derive(Debug)]
pub struct Decoder {
    buffer: BytesMut,
    limit: usize,
}

impl Decoder {
    /// A decoder that rejects frames whose payload is larger than `limit` bytes.
    pub fn new(limit: usize) -> Self {
        Self {
            buffer: BytesMut::new(),
            limit,
        }
    }

    /// Appends received bytes.
    pub fn push(&mut self, bytes: &[u8]) {
        self.buffer.extend_from_slice(bytes);
    }

    /// The next complete frame, or `None` if more bytes are needed.
    pub fn next_frame(&mut self) -> Result<Option<Frame>, EnvelopeError> {
        if self.buffer.len() < HEADER_LEN {
            return Ok(None);
        }
        let flags = self.buffer[0];
        let length = u32::from_be_bytes([
            self.buffer[1],
            self.buffer[2],
            self.buffer[3],
            self.buffer[4],
        ]) as usize;
        if length > self.limit {
            return Err(EnvelopeError::TooLarge {
                length,
                limit: self.limit,
            });
        }
        if self.buffer.len() < HEADER_LEN + length {
            return Ok(None);
        }
        self.buffer.advance(HEADER_LEN);
        let data = self.buffer.split_to(length).freeze();
        Ok(Some(Frame { flags, data }))
    }

    /// Fails if bytes of an incomplete frame remain (call once the input has ended).
    pub fn finish(&self) -> Result<(), EnvelopeError> {
        if self.buffer.is_empty() {
            Ok(())
        } else {
            Err(EnvelopeError::Truncated)
        }
    }
}

/// Decodes a complete buffer into frames.
pub fn decode_all(bytes: &[u8], limit: usize) -> Result<Vec<Frame>, EnvelopeError> {
    let mut decoder = Decoder::new(limit);
    decoder.push(bytes);
    let mut frames = Vec::new();
    while let Some(frame) = decoder.next_frame()? {
        frames.push(frame);
    }
    decoder.finish()?;
    Ok(frames)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip_split_anywhere() {
        let mut wire = Vec::new();
        wire.extend_from_slice(&encode(0, b"{\"a\":1}"));
        wire.extend_from_slice(&encode(0, b""));
        wire.extend_from_slice(&encode(FLAG_END_STREAM, b"{}"));
        for split in 0..=wire.len() {
            let mut decoder = Decoder::new(1024);
            let mut frames = Vec::new();
            for part in [&wire[..split], &wire[split..]] {
                decoder.push(part);
                while let Some(frame) = decoder.next_frame().expect("frame") {
                    frames.push(frame);
                }
            }
            decoder.finish().expect("complete");
            assert_eq!(frames.len(), 3, "split at {split}");
            assert_eq!(frames[0].data.as_ref(), b"{\"a\":1}");
            assert!(frames[1].data.is_empty());
            assert_eq!(frames[2].flags, FLAG_END_STREAM);
        }
    }

    #[test]
    fn limits_and_truncation() {
        assert_eq!(
            decode_all(&encode(0, &[0; 10]), 4),
            Err(EnvelopeError::TooLarge {
                length: 10,
                limit: 4
            })
        );
        let wire = encode(0, b"abc");
        assert_eq!(
            decode_all(&wire[..wire.len() - 1], 16),
            Err(EnvelopeError::Truncated)
        );
    }
}
