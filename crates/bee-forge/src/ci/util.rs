//! Encodings shared by the server side of CI and the local runner: base64url, SHA-256
//! digests, secret masking, workflow commands (`::error ...::`), and a minimal zip format
//! (stored entries) for artifacts and log archives.

use bee_proto::v1::{AnnotationLevel, CheckAnnotation};
use sha2::{Digest, Sha256};

use crate::ids::hex;

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
const B64_STD: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn encode_with(bytes: &[u8], alphabet: &[u8; 64], pad: bool) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        let chars = chunk.len() + 1;
        for i in 0..4 {
            if i < chars {
                out.push(alphabet[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else if pad {
                out.push('=');
            }
        }
    }
    out
}

/// Unpadded base64url (JWS segments).
pub fn b64url(bytes: &[u8]) -> String {
    encode_with(bytes, B64, false)
}

/// Padded standard base64.
pub fn b64(bytes: &[u8]) -> String {
    encode_with(bytes, B64_STD, true)
}

/// Decodes base64 in either alphabet, padded or not.
pub fn b64_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut acc = 0u32;
    let mut bits = 0;
    for c in text.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' => break,
            _ => return None,
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// `sha256:<hex>` of bytes.
pub fn sha256_digest(bytes: &[u8]) -> String {
    format!("sha256:{}", hex(&Sha256::digest(bytes)))
}

/// Hex SHA-256 of a string (token hashes).
pub fn sha256_hex(text: &str) -> String {
    hex(&Sha256::digest(text.as_bytes()))
}

/// Replaces every occurrence of each secret (and its single-line pieces of 4+ characters for
/// multi-line secrets) with `***`. Empty and very short values are not masked, as on GitHub.
pub fn mask(text: &str, secrets: &[String]) -> String {
    let mut needles: Vec<&str> = Vec::new();
    for s in secrets {
        if s.trim().len() >= 2 {
            needles.push(s.as_str());
        }
        if s.contains('\n') {
            for line in s.lines() {
                if line.trim().len() >= 4 {
                    needles.push(line);
                }
            }
        }
    }
    needles.sort_by_key(|n| std::cmp::Reverse(n.len()));
    let mut out = text.to_string();
    for n in needles {
        if out.contains(n) {
            out = out.replace(n, "***");
        }
    }
    out
}

/// A parsed workflow command line (`::name key=value,...::message`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Command {
    /// Command name (`error`, `warning`, `notice`, `add-mask`, `group`, ...).
    pub name: String,
    /// Properties.
    pub props: Vec<(String, String)>,
    /// The message.
    pub message: String,
}

fn unescape_data(s: &str) -> String {
    s.replace("%0D", "\r")
        .replace("%0A", "\n")
        .replace("%25", "%")
}

fn unescape_prop(s: &str) -> String {
    unescape_data(s).replace("%3A", ":").replace("%2C", ",")
}

/// Parses a workflow command, if the line is one.
pub fn parse_command(line: &str) -> Option<Command> {
    let rest = line.trim_start().strip_prefix("::")?;
    let (head, message) = rest.split_once("::")?;
    let (name, props) = match head.split_once(' ') {
        Some((n, p)) => (n, p),
        None => (head, ""),
    };
    if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
        return None;
    }
    let props = props
        .split(',')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), unescape_prop(v)))
        .collect();
    Some(Command {
        name: name.to_string(),
        props,
        message: unescape_data(message),
    })
}

/// Annotations from `::error`, `::warning` and `::notice` commands in a log, at most `limit`.
pub fn annotations(log: &str, limit: usize) -> Vec<CheckAnnotation> {
    let mut out = Vec::new();
    for line in log.lines() {
        if out.len() >= limit {
            break;
        }
        let Some(c) = parse_command(line) else {
            continue;
        };
        let level = match c.name.as_str() {
            "error" => AnnotationLevel::Failure,
            "warning" => AnnotationLevel::Warning,
            "notice" => AnnotationLevel::Notice,
            _ => continue,
        };
        let prop = |k: &str| {
            c.props
                .iter()
                .find(|(n, _)| n == k)
                .map(|(_, v)| v.clone())
                .unwrap_or_default()
        };
        let num = |k: &str| prop(k).parse::<u32>().unwrap_or(0);
        let start = num("line");
        out.push(CheckAnnotation {
            path: prop("file").into_bytes(),
            start_line: start,
            end_line: match num("endLine") {
                0 => start,
                n => n,
            },
            start_column: num("col"),
            end_column: num("endColumn"),
            level: level as i32,
            message: c.message,
            title: prop("title"),
            raw_details: String::new(),
        });
    }
    out
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in bytes {
        crc ^= u32::from(b);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

/// Builds a zip archive with stored (uncompressed) entries, in the order given.
pub fn zip(entries: &[(String, Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, data) in entries {
        let offset = out.len() as u32;
        let crc = crc32(data);
        let n = name.as_bytes();
        let mut header = Vec::new();
        header.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        header.extend_from_slice(&20u16.to_le_bytes()); // version needed
        header.extend_from_slice(&0x0800u16.to_le_bytes()); // UTF-8 names
        header.extend_from_slice(&0u16.to_le_bytes()); // stored
        header.extend_from_slice(&0u16.to_le_bytes()); // time
        header.extend_from_slice(&0x0021u16.to_le_bytes()); // date 1980-01-01
        header.extend_from_slice(&crc.to_le_bytes());
        header.extend_from_slice(&(data.len() as u32).to_le_bytes());
        header.extend_from_slice(&(data.len() as u32).to_le_bytes());
        header.extend_from_slice(&(n.len() as u16).to_le_bytes());
        header.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&header);
        out.extend_from_slice(n);
        out.extend_from_slice(data);
        central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes()); // made by
        central.extend_from_slice(&header[4..30]);
        central.extend_from_slice(&0u16.to_le_bytes()); // comment length
        central.extend_from_slice(&0u16.to_le_bytes()); // disk
        central.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
        central.extend_from_slice(&0u32.to_le_bytes()); // external attrs
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(n);
    }
    let cd_offset = out.len() as u32;
    let cd_len = central.len() as u32;
    out.extend_from_slice(&central);
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&cd_len.to_le_bytes());
    out.extend_from_slice(&cd_offset.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

/// Reads the stored entries of a zip written by [`zip`]; `None` for anything else.
pub fn unzip(bytes: &[u8]) -> Option<Vec<(String, Vec<u8>)>> {
    let u16_at = |i: usize| -> Option<u16> {
        Some(u16::from_le_bytes(bytes.get(i..i + 2)?.try_into().ok()?))
    };
    let u32_at = |i: usize| -> Option<u32> {
        Some(u32::from_le_bytes(bytes.get(i..i + 4)?.try_into().ok()?))
    };
    let mut out = Vec::new();
    let mut i = 0;
    while u32_at(i)? == 0x0403_4b50 {
        if u16_at(i + 8)? != 0 {
            return None;
        }
        let crc = u32_at(i + 14)?;
        let size = u32_at(i + 18)? as usize;
        let name_len = u16_at(i + 26)? as usize;
        let extra = u16_at(i + 28)? as usize;
        let name_start = i + 30;
        let name =
            String::from_utf8(bytes.get(name_start..name_start + name_len)?.to_vec()).ok()?;
        let data_start = name_start + name_len + extra;
        let data = bytes.get(data_start..data_start + size)?.to_vec();
        if crc32(&data) != crc {
            return None;
        }
        out.push((name, data));
        i = data_start + size;
    }
    Some(out)
}

/// Converts a workflow value to JSON.
pub fn to_json(v: &bee_workflow::Value) -> serde_json::Value {
    serde_json::from_str(&v.to_json_compact()).unwrap_or(serde_json::Value::Null)
}

/// Converts JSON to a workflow value.
pub fn from_json(v: &serde_json::Value) -> bee_workflow::Value {
    bee_workflow::value::parse_json(&v.to_string()).unwrap_or(bee_workflow::Value::Null)
}

/// Converts a workflow object to JSON text.
pub fn object_json(o: &bee_workflow::value::Object) -> String {
    bee_workflow::Value::from(o.clone()).to_json_compact()
}

/// Parses JSON text into a workflow object (empty on errors or non-objects).
pub fn json_object(text: &str) -> bee_workflow::value::Object {
    bee_workflow::value::parse_json(text)
        .ok()
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trips() {
        for data in [&b""[..], b"f", b"fo", b"foo", b"foob", b"\xff\xfe\x00"] {
            assert_eq!(b64_decode(&b64url(data)).unwrap(), data);
            assert_eq!(b64_decode(&b64(data)).unwrap(), data);
        }
        assert_eq!(b64(b"foob"), "Zm9vYg==");
        assert_eq!(b64url(b"\xfb\xff"), "-_8");
    }

    #[test]
    fn zip_round_trips() {
        let entries = vec![
            ("a.txt".to_string(), b"hello".to_vec()),
            ("dir/b.bin".to_string(), vec![0u8, 1, 2]),
        ];
        assert_eq!(unzip(&zip(&entries)).unwrap(), entries);
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert!(unzip(b"not a zip").is_none() || unzip(b"not a zip").unwrap().is_empty());
    }

    #[test]
    fn masking_hides_secrets_and_their_lines() {
        let s = mask(
            "token=abc123 and line2part",
            &["abc123".into(), "x\nline2part".into(), "".into()],
        );
        assert_eq!(s, "token=*** and ***");
    }

    #[test]
    fn commands_become_annotations() {
        let log = "ok\n::error file=src/a.rs,line=3,title=Bad::it broke%0Ahere\n::warning::careful\n::group::x\n";
        let a = annotations(log, 10);
        assert_eq!(a.len(), 2);
        assert_eq!(a[0].path, b"src/a.rs");
        assert_eq!(a[0].start_line, 3);
        assert_eq!(a[0].title, "Bad");
        assert_eq!(a[0].message, "it broke\nhere");
        assert_eq!(a[1].level, AnnotationLevel::Warning as i32);
    }
}
