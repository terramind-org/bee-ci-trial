//! SHA-1 collision detection.
//!
//! `data/shattered-1-prefix.bin` is the first 320 bytes of `shattered-1.pdf`, the public
//! SHAttered research artifact (Stevens, Bursztein, Karpman, Albertini, Markov, 2017,
//! https://shattered.io). Those bytes contain the near-collision blocks, so a
//! collision-detecting SHA-1 must refuse to hash them, while SHA-256 hashes them normally.

use bee_core::Reason;
use bee_git::ObjectFormat;

const PREFIX: &[u8] = include_bytes!("data/shattered-1-prefix.bin");

#[test]
fn sha1_rejects_the_shattered_collision_blocks() {
    assert_eq!(PREFIX.len(), 320);
    let err = ObjectFormat::Sha1.hash(PREFIX).unwrap_err();
    assert_eq!(err.reason, Reason::Integrity);
    assert!(err.payload.contains_key("collision"));
    // Incremental hashing detects it too.
    let mut h = ObjectFormat::Sha1.hasher();
    for chunk in PREFIX.chunks(7) {
        h.update(chunk);
    }
    assert_eq!(h.finish().unwrap_err().reason, Reason::Integrity);
    // SHA-256 repositories are unaffected, and ordinary SHA-1 input still hashes.
    assert!(ObjectFormat::Sha256.hash(PREFIX).is_ok());
    let mut clean = PREFIX.to_vec();
    clean[200] ^= 0x55;
    assert!(ObjectFormat::Sha1.hash(&clean).is_ok());
}
