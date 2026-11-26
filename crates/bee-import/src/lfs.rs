//! Git LFS pointer detection. A pointer is a small UTF-8 file (at most 1024 bytes) whose
//! first line is the LFS spec version and which names a `sha256` object id and a size.
//! The importer carries pointer files verbatim and lists the objects they name, so the forge
//! importer can fetch them from the LFS server.

use crate::report::LfsObject;

/// Pointer files are never larger than this.
pub const MAX_POINTER_BYTES: usize = 1024;

const VERSIONS: [&str; 2] = [
    "version https://git-lfs.github.com/spec/v1",
    "version https://hawser.github.com/spec/v1",
];

/// The LFS object a blob points at, when the blob is an LFS pointer.
pub fn lfs_pointer(data: &[u8]) -> Option<LfsObject> {
    if data.len() > MAX_POINTER_BYTES {
        return None;
    }
    let text = std::str::from_utf8(data).ok()?;
    let mut lines = text.lines();
    if !VERSIONS.contains(&lines.next()?) {
        return None;
    }
    let mut oid = None;
    let mut size = None;
    for line in lines {
        if let Some(hex) = line.strip_prefix("oid sha256:")
            && hex.len() == 64
            && bee_core::hex::is_lower_hex(hex)
        {
            oid = Some(format!("sha256:{hex}"));
        } else if let Some(n) = line.strip_prefix("size ")
            && !n.is_empty()
            && n.bytes().all(|b| b.is_ascii_digit())
        {
            size = n.parse().ok();
        }
    }
    Some(LfsObject {
        oid: oid?,
        size: size?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pointers_are_recognised() {
        let hex = "4d7a214614ab2935c943f9e0ff69d22eadbb8f32b1258daaa5e2ca24d17e2393";
        let p =
            format!("version https://git-lfs.github.com/spec/v1\noid sha256:{hex}\nsize 12345\n");
        assert_eq!(
            lfs_pointer(p.as_bytes()),
            Some(LfsObject {
                oid: format!("sha256:{hex}"),
                size: 12345
            })
        );
        assert_eq!(lfs_pointer(b"hello\n"), None);
        let no_size = format!("version https://git-lfs.github.com/spec/v1\noid sha256:{hex}\n");
        assert_eq!(lfs_pointer(no_size.as_bytes()), None);
        let bad_oid = "version https://git-lfs.github.com/spec/v1\noid sha256:XYZ\nsize 1\n";
        assert_eq!(lfs_pointer(bad_oid.as_bytes()), None);
        let mut big = p.clone().into_bytes();
        big.extend(std::iter::repeat_n(b'x', MAX_POINTER_BYTES));
        assert_eq!(lfs_pointer(&big), None);
    }
}
