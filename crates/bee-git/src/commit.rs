//! Git commits, kept byte-exact.
//!
//! A commit is `tree <hex>\n`, then header lines, then (normally) an empty line and the
//! message. [`GitCommit`] stores the tree id, the header bytes after the tree line exactly as
//! found, and the message, so any commit — signed, with `mergetag`, `encoding` or unknown
//! headers, with a non-UTF-8 message, or with no message separator at all — serializes back
//! to the same bytes and id.

use bee_core::Result;

use crate::error::malformed;
use crate::oid::{GitOid, ObjectFormat};

/// A parsed commit.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct GitCommit {
    tree: GitOid,
    headers: Vec<u8>,
    message: Option<Vec<u8>>,
}

/// A commit signature and the bytes it covers ([`GitCommit::signature`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitSignature {
    /// The commit body without its signature header.
    pub payload: Vec<u8>,
    /// The armored signature, newline-terminated.
    pub signature: Vec<u8>,
}

/// An identity line value: `Name <email> <seconds> <+hhmm>`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Ident {
    /// The name.
    pub name: Vec<u8>,
    /// The email (between `<` and `>`).
    pub email: Vec<u8>,
    /// Seconds since the Unix epoch.
    pub seconds: i64,
    /// Timezone offset in minutes east of UTC.
    pub tz_offset_minutes: i32,
}

impl Ident {
    /// Parses an identity value leniently, the way git reads history it did not write: the
    /// email is between the first `<` and the following `>`, the name is what precedes it,
    /// trimmed. A missing or garbled date gives time 0 and offset 0. `None` when there is no
    /// `<…>` at all.
    pub fn parse(value: &[u8]) -> Option<Ident> {
        let lt = value.iter().position(|&b| b == b'<')?;
        let gt = lt + 1 + value[lt + 1..].iter().position(|&b| b == b'>')?;
        let name = value[..lt].trim_ascii().to_vec();
        let email = value[lt + 1..gt].to_vec();
        let mut words = value[gt + 1..]
            .split(|&b| b == b' ')
            .filter(|w| !w.is_empty());
        let seconds = words
            .next()
            .and_then(|w| std::str::from_utf8(w).ok()?.parse::<i64>().ok());
        let tz = words.next().and_then(parse_tz);
        let (seconds, tz) = match (seconds, tz) {
            (Some(s), Some(tz)) => (s, tz),
            (Some(s), None) => (s, 0),
            _ => (0, 0),
        };
        Some(Ident {
            name,
            email,
            seconds,
            tz_offset_minutes: tz,
        })
    }

    /// The value bytes: `Name <email> <seconds> <±hhmm>`. Offsets beyond ±99:59 are clamped.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = self.name.clone();
        out.extend_from_slice(b" <");
        out.extend_from_slice(&self.email);
        out.extend_from_slice(b"> ");
        out.extend_from_slice(self.seconds.to_string().as_bytes());
        out.push(b' ');
        out.extend_from_slice(format_tz(self.tz_offset_minutes).as_bytes());
        out
    }
}

/// `±hhmm` → minutes. Accepts git's lenient forms: a sign and up to 4 digits read as hhmm.
fn parse_tz(word: &[u8]) -> Option<i32> {
    let (sign, digits) = match word.first()? {
        b'+' => (1, &word[1..]),
        b'-' => (-1, &word[1..]),
        _ => return None,
    };
    if digits.is_empty() || digits.len() > 4 || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let v: i32 = std::str::from_utf8(digits).ok()?.parse().ok()?;
    Some(sign * ((v / 100) * 60 + v % 100))
}

/// Minutes → `±hhmm`, clamped to ±99:59.
pub(crate) fn format_tz(minutes: i32) -> String {
    let sign = if minutes < 0 { '-' } else { '+' };
    let abs = minutes.unsigned_abs().min(99 * 60 + 59);
    format!("{sign}{:02}{:02}", abs / 60, abs % 60)
}

/// One header: name and value, continuation lines joined with `\n` (leading space removed).
pub(crate) fn split_headers(headers: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut out: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    for line in headers.split(|&b| b == b'\n') {
        if let Some(cont) = line.strip_prefix(b" ")
            && let Some(last) = out.last_mut()
        {
            last.1.push(b'\n');
            last.1.extend_from_slice(cont);
            continue;
        }
        match line.iter().position(|&b| b == b' ') {
            Some(sp) => out.push((line[..sp].to_vec(), line[sp + 1..].to_vec())),
            None => out.push((line.to_vec(), Vec::new())),
        }
    }
    // `split` yields a trailing empty piece after the final newline.
    if headers.ends_with(b"\n") || headers.is_empty() {
        out.pop();
    }
    out
}

/// Writes one header, continuing multi-line values with a leading space.
pub(crate) fn push_header(out: &mut Vec<u8>, name: &[u8], value: &[u8]) {
    out.extend_from_slice(name);
    out.push(b' ');
    for (i, line) in value.split(|&b| b == b'\n').enumerate() {
        if i > 0 {
            out.extend_from_slice(b"\n ");
        }
        out.extend_from_slice(line);
    }
    out.push(b'\n');
}

/// Splits a body after its first line into header bytes and an optional message: the headers
/// end at the first empty line, which is dropped; with no empty line everything is headers.
pub(crate) fn split_body(rest: &[u8]) -> (Vec<u8>, Option<Vec<u8>>) {
    let mut pos = 0;
    while pos < rest.len() {
        if rest[pos] == b'\n' {
            return (rest[..pos].to_vec(), Some(rest[pos + 1..].to_vec()));
        }
        match rest[pos..].iter().position(|&b| b == b'\n') {
            Some(nl) => pos += nl + 1,
            None => break,
        }
    }
    (rest.to_vec(), None)
}

impl GitCommit {
    /// Parses a commit body. Only the first line is checked strictly (`tree <lowercase hex>`,
    /// as git writes it), plus the leading `parent` lines; every other byte is kept as is.
    pub fn parse(data: &[u8], format: ObjectFormat) -> Result<GitCommit> {
        let n = format.oid_len() * 2;
        let line = data
            .strip_prefix(b"tree ")
            .ok_or_else(|| malformed("commit", "does not start with a tree line"))?;
        if line.get(n) != Some(&b'\n') {
            return Err(malformed("commit", "tree line is not `tree <id>`"));
        }
        let tree = format.parse_hex(&line[..n])?;
        let (headers, message) = split_body(&line[n + 1..]);
        let commit = GitCommit {
            tree,
            headers,
            message,
        };
        commit.try_parents(format)?;
        Ok(commit)
    }

    /// Builds a commit from its parts. `headers` are written in order after the tree line;
    /// the message follows an empty line.
    pub fn from_parts(tree: GitOid, headers: Vec<u8>, message: Option<Vec<u8>>) -> GitCommit {
        GitCommit {
            tree,
            headers,
            message,
        }
    }

    /// An ordinary commit: parents, author, committer, then the message.
    pub fn new(
        tree: GitOid,
        parents: &[GitOid],
        author: &Ident,
        committer: &Ident,
        message: &[u8],
    ) -> GitCommit {
        let mut headers = Vec::new();
        for p in parents {
            push_header(&mut headers, b"parent", p.to_hex().as_bytes());
        }
        push_header(&mut headers, b"author", &author.to_bytes());
        push_header(&mut headers, b"committer", &committer.to_bytes());
        GitCommit::from_parts(tree, headers, Some(message.to_vec()))
    }

    /// The root tree.
    pub fn tree(&self) -> GitOid {
        self.tree
    }

    /// The header bytes after the tree line, excluding the empty line that ends them.
    pub fn raw_headers(&self) -> &[u8] {
        &self.headers
    }

    /// The message, or `None` when the commit has no empty line ending its headers.
    pub fn message(&self) -> Option<&[u8]> {
        self.message.as_deref()
    }

    /// Every header after the tree line, in order, as `(name, value)`.
    pub fn headers(&self) -> Vec<(Vec<u8>, Vec<u8>)> {
        split_headers(&self.headers)
    }

    /// The first value of a header.
    pub fn header(&self, name: &[u8]) -> Option<Vec<u8>> {
        self.headers()
            .into_iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v)
    }

    fn try_parents(&self, format: ObjectFormat) -> Result<Vec<GitOid>> {
        let mut out = Vec::new();
        for (name, value) in self.headers() {
            if name != b"parent" {
                break;
            }
            out.push(format.parse_hex(&value)?);
        }
        Ok(out)
    }

    /// The parents: the `parent` lines directly after the tree line, in order.
    pub fn parents(&self) -> Vec<GitOid> {
        self.try_parents(self.tree.format()).unwrap_or_default()
    }

    /// The author identity, if present and parseable.
    pub fn author(&self) -> Option<Ident> {
        Ident::parse(&self.header(b"author")?)
    }

    /// The committer identity, if present and parseable.
    pub fn committer(&self) -> Option<Ident> {
        Ident::parse(&self.header(b"committer")?)
    }

    /// The `encoding` header, if any.
    pub fn encoding(&self) -> Option<Vec<u8>> {
        self.header(b"encoding")
    }

    /// The commit's signature and the bytes it signs: the `gpgsig` header (`gpgsig-sha256`
    /// for SHA-256 objects) with continuation lines joined by `\n`, and the commit body with
    /// that header removed and every other byte kept, as `git verify-commit` hands both to
    /// the verifier. `None` when the commit carries no signature header.
    pub fn signature(&self) -> Option<CommitSignature> {
        let name: &[u8] = match self.tree.format() {
            ObjectFormat::Sha1 => b"gpgsig",
            ObjectFormat::Sha256 => b"gpgsig-sha256",
        };
        let mut kept = Vec::with_capacity(self.headers.len());
        let mut signature: Option<Vec<u8>> = None;
        let mut in_sig = false;
        for line in self.headers.split_inclusive(|&b| b == b'\n') {
            if let Some(cont) = line.strip_prefix(b" ")
                && in_sig
            {
                if let Some(s) = signature.as_mut() {
                    s.push(b'\n');
                    s.extend_from_slice(cont.strip_suffix(b"\n").unwrap_or(cont));
                }
                continue;
            }
            in_sig = false;
            if signature.is_none() && line.starts_with(name) && line.get(name.len()) == Some(&b' ')
            {
                let v = &line[name.len() + 1..];
                signature = Some(v.strip_suffix(b"\n").unwrap_or(v).to_vec());
                in_sig = true;
                continue;
            }
            kept.extend_from_slice(line);
        }
        let mut signature = signature?;
        signature.push(b'\n');
        let payload = GitCommit::from_parts(self.tree, kept, self.message.clone()).serialize();
        Some(CommitSignature { payload, signature })
    }

    /// The body bytes.
    pub fn serialize(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.headers.len() + 80);
        out.extend_from_slice(b"tree ");
        out.extend_from_slice(self.tree.to_hex().as_bytes());
        out.push(b'\n');
        out.extend_from_slice(&self.headers);
        if let Some(m) = &self.message {
            out.push(b'\n');
            out.extend_from_slice(m);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_splits_header_from_payload() {
        let c = GitCommit::parse(&sample(), ObjectFormat::Sha1).unwrap();
        let sig = c.signature().unwrap();
        assert_eq!(
            sig.signature,
            b"-----BEGIN PGP SIGNATURE-----\n\nabc\n-----END PGP SIGNATURE-----\n"
        );
        let text = String::from_utf8(sig.payload).unwrap();
        assert!(!text.contains("gpgsig") && !text.contains("abc"));
        assert!(text.contains("encoding ISO-8859-1\nx-custom value\n\nsubject\n\nbody\n"));
        let plain =
            format!("tree {TREE}\nauthor A <a@x> 1 +0000\ncommitter A <a@x> 1 +0000\n\nm\n");
        let unsigned = GitCommit::parse(plain.as_bytes(), ObjectFormat::Sha1).unwrap();
        assert_eq!(unsigned.signature(), None);
    }

    const TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

    fn sample() -> Vec<u8> {
        format!(
            "tree {TREE}\nparent {p}\nauthor A U Thor <a@example.com> 1700000000 +0530\n\
             committer C <c@example.com> 1700000001 -0800\nencoding ISO-8859-1\n\
             gpgsig -----BEGIN PGP SIGNATURE-----\n \n abc\n -----END PGP SIGNATURE-----\n\
             x-custom value\n\nsubject\n\nbody\n",
            p = "1".repeat(40)
        )
        .into_bytes()
    }

    #[test]
    fn parse_and_access() {
        let bytes = sample();
        let c = GitCommit::parse(&bytes, ObjectFormat::Sha1).unwrap();
        assert_eq!(c.serialize(), bytes);
        assert_eq!(c.tree().to_hex(), TREE);
        assert_eq!(c.parents(), vec![GitOid::Sha1([0x11; 20])]);
        let a = c.author().unwrap();
        assert_eq!(a.name, b"A U Thor");
        assert_eq!(a.email, b"a@example.com");
        assert_eq!((a.seconds, a.tz_offset_minutes), (1_700_000_000, 330));
        assert_eq!(c.committer().unwrap().tz_offset_minutes, -480);
        assert_eq!(c.encoding().unwrap(), b"ISO-8859-1");
        let sig = c.header(b"gpgsig").unwrap();
        assert_eq!(
            sig,
            b"-----BEGIN PGP SIGNATURE-----\n\nabc\n-----END PGP SIGNATURE-----"
        );
        assert_eq!(c.message().unwrap(), b"subject\n\nbody\n");
        let names: Vec<Vec<u8>> = c.headers().into_iter().map(|h| h.0).collect();
        assert_eq!(
            names,
            [
                &b"parent"[..],
                b"author",
                b"committer",
                b"encoding",
                b"gpgsig",
                b"x-custom"
            ]
        );
    }

    #[test]
    fn rebuilding_headers_is_exact() {
        let c = GitCommit::parse(&sample(), ObjectFormat::Sha1).unwrap();
        let mut rebuilt = Vec::new();
        for (n, v) in c.headers() {
            push_header(&mut rebuilt, &n, &v);
        }
        assert_eq!(rebuilt, c.raw_headers());
    }

    #[test]
    fn odd_shapes_round_trip() {
        for body in [
            format!("tree {TREE}\nauthor x"),
            format!("tree {TREE}\n"),
            format!("tree {TREE}\n\n"),
            format!("tree {TREE}\nauthor <> 0 +0000\ncommitter <> 0 +0000\n\n\n\nmsg"),
            format!("tree {TREE}\nnovalue\n \n\nm"),
        ] {
            let c = GitCommit::parse(body.as_bytes(), ObjectFormat::Sha1).unwrap();
            assert_eq!(c.serialize(), body.as_bytes());
        }
        assert!(GitCommit::parse(b"parent x\n", ObjectFormat::Sha1).is_err());
        let upper = format!("tree {}\n", TREE.to_uppercase());
        assert!(GitCommit::parse(upper.as_bytes(), ObjectFormat::Sha1).is_err());
        let bad_parent = format!("tree {TREE}\nparent zz\n\n");
        assert!(GitCommit::parse(bad_parent.as_bytes(), ObjectFormat::Sha1).is_err());
    }

    #[test]
    fn idents() {
        let i = Ident::parse(b"Name <e> 12 +0100").unwrap();
        assert_eq!(i.to_bytes(), b"Name <e> 12 +0100");
        assert_eq!(Ident::parse(b"x <e> junk").unwrap().seconds, 0);
        assert_eq!(Ident::parse(b"x <e> 5").unwrap().seconds, 5);
        assert_eq!(Ident::parse(b"no email"), None);
        assert_eq!(format_tz(0), "+0000");
        assert_eq!(format_tz(-90), "-0130");
        assert_eq!(format_tz(i32::MIN), "-9959");
        assert_eq!(parse_tz(b"0100"), None);
        assert_eq!(parse_tz(b"+12345"), None);
        let c = GitCommit::new(
            ObjectFormat::Sha1.empty_tree(),
            &[GitOid::Sha1([2; 20])],
            &i,
            &i,
            b"m\n",
        );
        let back = GitCommit::parse(&c.serialize(), ObjectFormat::Sha1).unwrap();
        assert_eq!(back, c);
        assert_eq!(back.parents(), vec![GitOid::Sha1([2; 20])]);
    }
}
