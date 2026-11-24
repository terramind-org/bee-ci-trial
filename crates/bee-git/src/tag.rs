//! Annotated git tags, kept byte-exact: `object`, `type`, `tag`, `tagger` and any other
//! headers, then an empty line and the message (which carries an inline signature, if any).

use bee_core::Result;

use crate::commit::{Ident, push_header, split_body, split_headers};
use crate::error::malformed;
use crate::object::ObjectType;
use crate::oid::{GitOid, ObjectFormat};

/// A parsed annotated tag.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct GitTag {
    object: GitOid,
    headers: Vec<u8>,
    message: Option<Vec<u8>>,
}

impl GitTag {
    /// Parses a tag body. The first line must be `object <lowercase hex>` and a `type` header
    /// must name a known object type; every byte is kept.
    pub fn parse(data: &[u8], format: ObjectFormat) -> Result<GitTag> {
        let n = format.oid_len() * 2;
        let line = data
            .strip_prefix(b"object ")
            .ok_or_else(|| malformed("tag", "does not start with an object line"))?;
        if line.get(n) != Some(&b'\n') {
            return Err(malformed("tag", "object line is not `object <id>`"));
        }
        let object = format.parse_hex(&line[..n])?;
        let (headers, message) = split_body(&line[n + 1..]);
        let tag = GitTag {
            object,
            headers,
            message,
        };
        if tag.target_type().is_none() {
            return Err(malformed("tag", "missing or unknown `type` header"));
        }
        Ok(tag)
    }

    /// A tag of `object` (of `kind`) named `name`, with an optional tagger and a message.
    pub fn new(
        object: GitOid,
        kind: ObjectType,
        name: &[u8],
        tagger: Option<&Ident>,
        message: &[u8],
    ) -> GitTag {
        let mut headers = Vec::new();
        push_header(&mut headers, b"type", kind.name().as_bytes());
        push_header(&mut headers, b"tag", name);
        if let Some(t) = tagger {
            push_header(&mut headers, b"tagger", &t.to_bytes());
        }
        GitTag {
            object,
            headers,
            message: Some(message.to_vec()),
        }
    }

    /// The tagged object.
    pub fn object(&self) -> GitOid {
        self.object
    }

    /// Headers after the `object` line, in order, as `(name, value)`.
    pub fn headers(&self) -> Vec<(Vec<u8>, Vec<u8>)> {
        split_headers(&self.headers)
    }

    fn header(&self, name: &[u8]) -> Option<Vec<u8>> {
        self.headers()
            .into_iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v)
    }

    /// The tagged object's type, from the `type` header.
    pub fn target_type(&self) -> Option<ObjectType> {
        ObjectType::from_name(&self.header(b"type")?)
    }

    /// The tag name, from the `tag` header.
    pub fn name(&self) -> Option<Vec<u8>> {
        self.header(b"tag")
    }

    /// The tagger, if present and parseable.
    pub fn tagger(&self) -> Option<Ident> {
        Ident::parse(&self.header(b"tagger")?)
    }

    /// The message (signature included), or `None` without a header-ending empty line.
    pub fn message(&self) -> Option<&[u8]> {
        self.message.as_deref()
    }

    /// The body bytes.
    pub fn serialize(&self) -> Vec<u8> {
        let mut out = b"object ".to_vec();
        out.extend_from_slice(self.object.to_hex().as_bytes());
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
    fn tag_round_trip() {
        let body = format!(
            "object {}\ntype commit\ntag v1.0\ntagger T <t@x> 1 +0000\n\nrelease\n\
             -----BEGIN PGP SIGNATURE-----\nxyz\n-----END PGP SIGNATURE-----\n",
            "a".repeat(40)
        );
        let t = GitTag::parse(body.as_bytes(), ObjectFormat::Sha1).unwrap();
        assert_eq!(t.serialize(), body.as_bytes());
        assert_eq!(t.object(), GitOid::Sha1([0xaa; 20]));
        assert_eq!(t.target_type(), Some(ObjectType::Commit));
        assert_eq!(t.name().unwrap(), b"v1.0");
        assert_eq!(t.tagger().unwrap().email, b"t@x");
        assert!(t.message().unwrap().starts_with(b"release\n"));
        assert_eq!(t.headers().len(), 3);

        let made = GitTag::new(
            t.object(),
            ObjectType::Commit,
            b"v1.0",
            t.tagger().as_ref(),
            b"m",
        );
        let back = GitTag::parse(&made.serialize(), ObjectFormat::Sha1).unwrap();
        assert_eq!(back, made);
        assert_eq!(
            GitTag::new(t.object(), ObjectType::Tree, b"x", None, b"").target_type(),
            Some(ObjectType::Tree)
        );
    }

    #[test]
    fn tag_rejects_bad_shapes() {
        let a = "a".repeat(40);
        for bad in [
            "type commit\n".to_string(),
            format!("object {a}\ntag x\n\n"),
            format!("object {a}\ntype widget\n\n"),
            format!("object {}\ntype blob\n\n", "a".repeat(39)),
        ] {
            assert!(
                GitTag::parse(bad.as_bytes(), ObjectFormat::Sha1).is_err(),
                "{bad}"
            );
        }
    }
}
