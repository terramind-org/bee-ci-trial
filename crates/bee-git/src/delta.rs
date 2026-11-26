//! Git's delta encoding, as used by packfile ofs and ref deltas.
//!
//! A delta is: base size (varint), result size (varint), then instructions. An instruction
//! byte with the high bit set copies a range of the base (offset and size bytes selected by
//! its low 7 bits; size 0 means 0x10000); a byte `1..=127` inserts that many literal bytes;
//! byte 0 is reserved. [`apply`] checks every bound before touching memory.

use std::collections::HashMap;

use bee_core::Result;

use crate::error::integrity;

/// Largest copy one instruction emits when encoding (git's own limit, for old readers).
const MAX_COPY: usize = 0x10000;
/// Window length used to find matches.
const BLOCK: usize = 16;

/// Reads a delta size varint (7 bits per byte, little-endian, high bit = more).
fn read_varint(delta: &[u8], pos: &mut usize) -> Result<u64> {
    let mut v: u64 = 0;
    let mut shift = 0u32;
    loop {
        let b = *delta
            .get(*pos)
            .ok_or_else(|| integrity("delta: truncated size header"))?;
        *pos += 1;
        if shift > 63 {
            return Err(integrity("delta: size header overflows"));
        }
        v |= u64::from(b & 0x7f)
            .checked_shl(shift)
            .ok_or_else(|| integrity("delta: size header overflows"))?;
        if b & 0x80 == 0 {
            return Ok(v);
        }
        shift += 7;
    }
}

fn write_varint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

/// The result size a delta declares, without applying it.
pub fn result_size(delta: &[u8]) -> Result<u64> {
    let mut pos = 0;
    read_varint(delta, &mut pos)?;
    read_varint(delta, &mut pos)
}

/// Applies `delta` to `base`. Fails with `INTEGRITY` if the declared base size is wrong, the
/// result would exceed `max_result` bytes, an instruction reads outside the base or the delta,
/// uses the reserved opcode, or the output length differs from the declared size.
pub fn apply(base: &[u8], delta: &[u8], max_result: u64) -> Result<Vec<u8>> {
    let mut pos = 0;
    let base_size = read_varint(delta, &mut pos)?;
    if base_size != base.len() as u64 {
        return Err(integrity(format!(
            "delta: base is {} bytes, delta expects {base_size}",
            base.len()
        )));
    }
    let size = read_varint(delta, &mut pos)?;
    if size > max_result {
        return Err(integrity(format!(
            "delta: result of {size} bytes exceeds the {max_result}-byte limit"
        )));
    }
    let size = usize::try_from(size).map_err(|_| integrity("delta: result too large"))?;
    let mut out = Vec::with_capacity(size.min(1 << 24));
    while pos < delta.len() {
        let op = delta[pos];
        pos += 1;
        if op & 0x80 != 0 {
            let mut offset: u64 = 0;
            let mut len: u64 = 0;
            for i in 0..4 {
                if op & (1 << i) != 0 {
                    let b = *delta
                        .get(pos)
                        .ok_or_else(|| integrity("delta: truncated copy"))?;
                    pos += 1;
                    offset |= u64::from(b) << (8 * i);
                }
            }
            for i in 0..3 {
                if op & (0x10 << i) != 0 {
                    let b = *delta
                        .get(pos)
                        .ok_or_else(|| integrity("delta: truncated copy"))?;
                    pos += 1;
                    len |= u64::from(b) << (8 * i);
                }
            }
            if len == 0 {
                len = 0x10000;
            }
            let end = offset
                .checked_add(len)
                .filter(|&e| e <= base.len() as u64)
                .ok_or_else(|| integrity("delta: copy outside the base"))?;
            if out.len() as u64 + len > size as u64 {
                return Err(integrity("delta: output longer than declared"));
            }
            out.extend_from_slice(&base[offset as usize..end as usize]);
        } else if op != 0 {
            let n = usize::from(op);
            let lit = delta
                .get(pos..pos + n)
                .ok_or_else(|| integrity("delta: truncated insert"))?;
            if out.len() + n > size {
                return Err(integrity("delta: output longer than declared"));
            }
            out.extend_from_slice(lit);
            pos += n;
        } else {
            return Err(integrity("delta: reserved instruction 0"));
        }
    }
    if out.len() != size {
        return Err(integrity(format!(
            "delta: produced {} bytes, declared {size}",
            out.len()
        )));
    }
    Ok(out)
}

fn push_insert(out: &mut Vec<u8>, mut lit: &[u8]) {
    while !lit.is_empty() {
        let n = lit.len().min(127);
        out.push(n as u8);
        out.extend_from_slice(&lit[..n]);
        lit = &lit[n..];
    }
}

fn push_copy(out: &mut Vec<u8>, mut offset: usize, mut len: usize) {
    while len > 0 {
        let n = len.min(MAX_COPY);
        let mut op = 0x80u8;
        let mut args = Vec::with_capacity(7);
        for i in 0..4 {
            let b = ((offset >> (8 * i)) & 0xff) as u8;
            if b != 0 {
                op |= 1 << i;
                args.push(b);
            }
        }
        // A size of exactly 0x10000 is encoded as no size bytes.
        if n != 0x10000 {
            for i in 0..3 {
                let b = ((n >> (8 * i)) & 0xff) as u8;
                if b != 0 {
                    op |= 0x10 << i;
                    args.push(b);
                }
            }
        }
        out.push(op);
        out.extend_from_slice(&args);
        offset += n;
        len -= n;
    }
}

/// Encodes `target` as a delta against `base`. Deterministic: the same inputs always give the
/// same bytes. Bases of 4 GiB or more are not indexed (the delta is then all inserts).
pub fn create(base: &[u8], target: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    write_varint(&mut out, base.len() as u64);
    write_varint(&mut out, target.len() as u64);
    // First occurrence of each block-aligned window of the base. Only looked up, never
    // iterated, so the map's ordering cannot affect the output.
    let mut index: HashMap<&[u8], usize> = HashMap::new();
    if base.len() <= u32::MAX as usize {
        let mut i = 0;
        while i + BLOCK <= base.len() {
            index.entry(&base[i..i + BLOCK]).or_insert(i);
            i += BLOCK;
        }
    }
    let mut lit_start = 0;
    let mut i = 0;
    while i + BLOCK <= target.len() {
        let Some(&at) = index.get(&target[i..i + BLOCK]) else {
            i += 1;
            continue;
        };
        // Extend the match backwards into pending literals, then forwards.
        let mut start_t = i;
        let mut start_b = at;
        while start_t > lit_start && start_b > 0 && target[start_t - 1] == base[start_b - 1] {
            start_t -= 1;
            start_b -= 1;
        }
        let mut len = i + BLOCK - start_t;
        while start_t + len < target.len()
            && start_b + len < base.len()
            && target[start_t + len] == base[start_b + len]
        {
            len += 1;
        }
        push_insert(&mut out, &target[lit_start..start_t]);
        push_copy(&mut out, start_b, len);
        i = start_t + len;
        lit_start = i;
    }
    push_insert(&mut out, &target[lit_start..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn create_then_apply() {
        let base: Vec<u8> = (0..5000u32).map(|i| (i * 7 % 251) as u8).collect();
        let mut target = base.clone();
        target.splice(100..120, b"inserted text here".iter().copied());
        target.extend_from_slice(&base[..300]);
        let d = create(&base, &target);
        assert!(d.len() < target.len() / 4);
        assert_eq!(apply(&base, &d, u64::MAX).unwrap(), target);
        assert_eq!(result_size(&d).unwrap(), target.len() as u64);
    }

    #[test]
    fn large_copies_split() {
        let base = vec![3u8; 200_000];
        let d = create(&base, &base);
        assert_eq!(apply(&base, &d, u64::MAX).unwrap(), base);
        let empty = create(b"", b"");
        assert_eq!(apply(b"", &empty, 0).unwrap(), b"");
    }

    #[test]
    fn apply_bounds() {
        // base 3, result 2, copy offset 2 size 2: outside the base.
        assert!(apply(b"abc", &[3, 2, 0x91, 2, 2], 10).is_err());
        // wrong base size
        assert!(apply(b"ab", &[3, 1, 1, b'x'], 10).is_err());
        // reserved opcode
        assert!(apply(b"abc", &[3, 1, 0], 10).is_err());
        // truncated insert
        assert!(apply(b"abc", &[3, 5, 5, b'x'], 10).is_err());
        // result longer than declared
        assert!(apply(b"abc", &[3, 1, 2, b'x', b'y'], 10).is_err());
        // shorter than declared
        assert!(apply(b"abc", &[3, 2, 1, b'x'], 10).is_err());
        // limit
        assert!(apply(b"abc", &[3, 2, 2, b'x', b'y'], 1).is_err());
        // copy with size 0 means 0x10000: needs a base that large
        assert!(apply(b"abc", &[3, 3, 0x80], 1 << 20).is_err());
        // truncated copy args, truncated header
        assert!(apply(b"abc", &[3, 3, 0x81], 10).is_err());
        assert!(apply(b"abc", &[0x83], 10).is_err());
        assert!(apply(b"abc", &[0xff; 12], 10).is_err());
        // a valid copy
        assert_eq!(apply(b"abc", &[3, 2, 0x91, 1, 2], 10).unwrap(), b"bc");
    }

    proptest! {
        #[test]
        fn round_trip(base in proptest::collection::vec(0u8..4, 0..600),
                      target in proptest::collection::vec(0u8..4, 0..600)) {
            let d = create(&base, &target);
            prop_assert_eq!(apply(&base, &d, u64::MAX).unwrap(), target);
        }

        #[test]
        fn apply_never_panics(base in proptest::collection::vec(any::<u8>(), 0..64),
                              delta in proptest::collection::vec(any::<u8>(), 0..64)) {
            let _ = apply(&base, &delta, 1 << 16);
        }
    }
}
