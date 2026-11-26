//! Meta (SPEC §4.3) and provenance (SPEC §10.4).

use std::collections::BTreeMap;

use bee_core::{Limits, ObjectId, ObjectKind, Result};

use crate::cbor::Value;
use crate::fields::{Extensions, Fields, field_error, top_map, validate_extensions};
use crate::hash_id;

/// Meta key of the git extension (§17.4): the original commit header bytes.
pub const GIT_EXTENSION_KEY: u64 = 64;

/// A point in time: `[seconds since the Unix epoch, timezone offset in minutes]`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct Timestamp {
    /// Seconds since 1970-01-01T00:00:00Z; may be negative.
    pub seconds: i64,
    /// Offset from UTC in minutes, as recorded by the author.
    pub tz_offset_minutes: i32,
}

impl Timestamp {
    /// A timestamp.
    pub fn new(seconds: i64, tz_offset_minutes: i32) -> Timestamp {
        Timestamp {
            seconds,
            tz_offset_minutes,
        }
    }

    fn to_value(self) -> Value {
        Value::Array(vec![
            Value::int(self.seconds),
            Value::int(i64::from(self.tz_offset_minutes)),
        ])
    }

    fn from_value(what: &str, v: Value) -> Result<Timestamp> {
        let bad = || field_error(what, "must be [seconds, tz_offset_minutes]");
        let Value::Array(items) = v else {
            return Err(bad());
        };
        let [s, tz] = <[Value; 2]>::try_from(items).map_err(|_| bad())?;
        let seconds = s.as_i64().ok_or_else(bad)?;
        let tz = tz
            .as_i64()
            .and_then(|t| i32::try_from(t).ok())
            .ok_or_else(bad)?;
        Ok(Timestamp {
            seconds,
            tz_offset_minutes: tz,
        })
    }
}

/// A check the agent ran locally: `{1: name, 2: result, 3: duration_ms}`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct LocalCheck {
    /// Check name.
    pub name: String,
    /// Result, e.g. `"pass"`.
    pub result: String,
    /// Wall time the check took, in milliseconds.
    pub duration_ms: u64,
}

/// Model cost claims: `{1: input_tokens, 2: output_tokens, 3: usd_micros}`, all optional.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct Cost {
    /// Input tokens.
    pub input_tokens: Option<u64>,
    /// Output tokens.
    pub output_tokens: Option<u64>,
    /// Cost in millionths of a US dollar.
    pub usd_micros: Option<u64>,
}

/// Provenance (SPEC §10.4). Optional fields are omitted when absent; the two arrays are
/// omitted when empty.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Default)]
pub struct Provenance {
    /// 1: principal ids from the acting agent back to its human or service root.
    pub principal_chain: Vec<String>,
    /// 2: agent runtime and version.
    pub runtime: Option<String>,
    /// 3: model id.
    pub model: Option<String>,
    /// 4: opaque session id.
    pub session: Option<String>,
    /// 5: BLAKE3 of the task prompt.
    pub prompt_digest: Option<[u8; 32]>,
    /// 6: checks the agent ran locally.
    pub checks_run: Vec<LocalCheck>,
    /// 7: cost claims.
    pub cost: Option<Cost>,
}

impl Provenance {
    fn to_value(&self) -> Value {
        let mut m = BTreeMap::new();
        if !self.principal_chain.is_empty() {
            m.insert(
                1,
                Value::Array(
                    self.principal_chain
                        .iter()
                        .map(|p| Value::Text(p.clone()))
                        .collect(),
                ),
            );
        }
        put_text(&mut m, 2, &self.runtime);
        put_text(&mut m, 3, &self.model);
        put_text(&mut m, 4, &self.session);
        if let Some(d) = self.prompt_digest {
            m.insert(5, Value::Bytes(d.to_vec()));
        }
        if !self.checks_run.is_empty() {
            let checks = self
                .checks_run
                .iter()
                .map(|c| {
                    Value::Map(BTreeMap::from([
                        (1, Value::Text(c.name.clone())),
                        (2, Value::Text(c.result.clone())),
                        (3, Value::Uint(c.duration_ms)),
                    ]))
                })
                .collect();
            m.insert(6, Value::Array(checks));
        }
        if let Some(c) = self.cost {
            let mut cm = BTreeMap::new();
            put_uint(&mut cm, 1, c.input_tokens);
            put_uint(&mut cm, 2, c.output_tokens);
            put_uint(&mut cm, 3, c.usd_micros);
            m.insert(7, Value::Map(cm));
        }
        Value::Map(m)
    }

    fn from_value(v: Value) -> Result<Provenance> {
        let mut f = Fields::new("provenance", v)?;
        let principal_chain = f
            .opt_nonempty_array(1)?
            .into_iter()
            .map(|p| match p {
                Value::Text(t) => Ok(t),
                _ => Err(field_error("provenance", "principal_chain holds text")),
            })
            .collect::<Result<_>>()?;
        let runtime = f.opt_text(2)?;
        let model = f.opt_text(3)?;
        let session = f.opt_text(4)?;
        let prompt_digest = match f.opt_bytes(5)? {
            None => None,
            Some(b) => Some(
                <[u8; 32]>::try_from(b.as_slice())
                    .map_err(|_| field_error("provenance", "prompt_digest must be 32 bytes"))?,
            ),
        };
        let checks_run = f
            .opt_nonempty_array(6)?
            .into_iter()
            .map(|c| {
                let mut cf = Fields::new("local check", c)?;
                let check = LocalCheck {
                    name: cf.text(1)?,
                    result: cf.text(2)?,
                    duration_ms: cf.uint(3)?,
                };
                cf.finish()?;
                Ok(check)
            })
            .collect::<Result<_>>()?;
        let cost = match f.take(7) {
            None => None,
            Some(c) => {
                let mut cf = Fields::new("cost", c)?;
                let cost = Cost {
                    input_tokens: cf.opt_uint(1)?,
                    output_tokens: cf.opt_uint(2)?,
                    usd_micros: cf.opt_uint(3)?,
                };
                cf.finish()?;
                Some(cost)
            }
        };
        f.finish()?;
        Ok(Provenance {
            principal_chain,
            runtime,
            model,
            session,
            prompt_digest,
            checks_run,
            cost,
        })
    }
}

fn put_text(m: &mut BTreeMap<u64, Value>, key: u64, v: &Option<String>) {
    if let Some(t) = v {
        m.insert(key, Value::Text(t.clone()));
    }
}

fn put_uint(m: &mut BTreeMap<u64, Value>, key: u64, v: Option<u64>) {
    if let Some(n) = v {
        m.insert(key, Value::Uint(n));
    }
}

/// Changeset metadata (SPEC §4.3). Kept apart from the changeset so it can be redacted.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Meta {
    /// 1: author.
    pub author: String,
    /// 2: author time.
    pub author_time: Timestamp,
    /// 3: committer (import only).
    pub committer: Option<String>,
    /// 4: committer time (import only).
    pub committer_time: Option<Timestamp>,
    /// 5: principal id of the creator, set by the server.
    pub creator: String,
    /// 6: message.
    pub message: String,
    /// 7: provenance claims.
    pub provenance: Option<Provenance>,
    /// 64: original git commit header bytes (import only, ignorable extension).
    pub git: Option<Vec<u8>>,
    extensions: Extensions,
}

impl Meta {
    /// A meta with the required fields and nothing else.
    pub fn new(
        author: impl Into<String>,
        author_time: Timestamp,
        creator: impl Into<String>,
        message: impl Into<String>,
    ) -> Meta {
        Meta {
            author: author.into(),
            author_time,
            creator: creator.into(),
            message: message.into(),
            ..Meta::default()
        }
    }

    /// The fixed meta of every repo's empty root changeset (SPEC §4.4): empty author,
    /// creator and message, time `[0, 0]`.
    pub fn empty_root() -> Meta {
        Meta::default()
    }

    /// Preserved unknown extension fields.
    pub fn extensions(&self) -> &Extensions {
        &self.extensions
    }

    /// Replaces the unknown extension fields (even keys ≥ 64, not 64 itself).
    pub fn set_extensions(&mut self, extensions: Extensions) -> Result<()> {
        validate_extensions(&extensions, &[GIT_EXTENSION_KEY])?;
        self.extensions = extensions;
        Ok(())
    }

    /// Checks the limits encoding does not enforce: message size.
    pub fn validate(&self, limits: &Limits) -> Result<()> {
        Limits::check(
            "message bytes",
            limits.message_bytes,
            self.message.len() as u64,
        )
    }

    /// The canonical encoding.
    pub fn encode(&self) -> Vec<u8> {
        let mut m = top_map(&self.extensions);
        m.insert(1, Value::Text(self.author.clone()));
        m.insert(2, self.author_time.to_value());
        put_text(&mut m, 3, &self.committer);
        if let Some(t) = self.committer_time {
            m.insert(4, t.to_value());
        }
        m.insert(5, Value::Text(self.creator.clone()));
        m.insert(6, Value::Text(self.message.clone()));
        if let Some(p) = &self.provenance {
            m.insert(7, p.to_value());
        }
        if let Some(g) = &self.git {
            m.insert(GIT_EXTENSION_KEY, Value::Bytes(g.clone()));
        }
        Value::Map(m).encode()
    }

    /// The meta's id: BLAKE3 over the canonical encoding, kind meta.
    pub fn id(&self) -> ObjectId {
        hash_id(ObjectKind::Meta, &self.encode())
    }

    /// Decodes and validates with the default limits.
    pub fn decode(bytes: &[u8]) -> Result<Meta> {
        Meta::decode_with_limits(bytes, &Limits::DEFAULT)
    }

    /// Decodes and validates with the given limits.
    pub fn decode_with_limits(bytes: &[u8], limits: &Limits) -> Result<Meta> {
        let mut f = Fields::top("meta", bytes, limits)?;
        let author = f.text(1)?;
        let author_time = Timestamp::from_value("meta author_time", f.req(2)?)?;
        let committer = f.opt_text(3)?;
        let committer_time = match f.take(4) {
            None => None,
            Some(v) => Some(Timestamp::from_value("meta committer_time", v)?),
        };
        let creator = f.text(5)?;
        let message = f.text(6)?;
        let provenance = match f.take(7) {
            None => None,
            Some(v) => Some(Provenance::from_value(v)?),
        };
        let git = f.opt_bytes(GIT_EXTENSION_KEY)?;
        let extensions = f.finish_top()?;
        let meta = Meta {
            author,
            author_time,
            committer,
            committer_time,
            creator,
            message,
            provenance,
            git,
            extensions,
        };
        meta.validate(limits)?;
        Ok(meta)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bee_core::{Reason, Subcode, hex};

    fn full() -> Meta {
        let mut m = Meta::new(
            "Ada <ada@example.com>",
            Timestamp::new(1_700_000_000, -300),
            "01HZX",
            "fix: x\n",
        );
        m.committer = Some("Bob".into());
        m.committer_time = Some(Timestamp::new(-5, 60));
        m.provenance = Some(Provenance {
            principal_chain: vec!["agent".into(), "human".into()],
            runtime: Some("claude-code/3.2".into()),
            model: Some("model-x".into()),
            session: Some("s-1".into()),
            prompt_digest: Some([9; 32]),
            checks_run: vec![LocalCheck {
                name: "test".into(),
                result: "pass".into(),
                duration_ms: 1200,
            }],
            cost: Some(Cost {
                input_tokens: Some(10),
                output_tokens: None,
                usd_micros: Some(3),
            }),
        });
        m.git = Some(b"parent abc\nauthor x\n".to_vec());
        m
    }

    #[test]
    fn meta_round_trip_full() {
        let m = full();
        let back = Meta::decode(&m.encode()).unwrap();
        assert_eq!(back, m);
        assert_eq!(back.id(), m.id());
        assert_eq!(m.id().kind(), ObjectKind::Meta);
    }

    #[test]
    fn empty_root_meta_encoding() {
        let m = Meta::empty_root();
        // {0:1, 1:"", 2:[0,0], 5:"", 6:""}
        assert_eq!(hex::encode(&m.encode()), "a5000101600282000005600660");
        assert_eq!(Meta::decode(&m.encode()).unwrap(), m);
    }

    #[test]
    fn meta_git_extension_preserved() {
        let mut m = Meta::new("a", Timestamp::default(), "c", "m");
        m.git = Some(vec![0, 1, 2]);
        let back = Meta::decode(&m.encode()).unwrap();
        assert_eq!(back.git, Some(vec![0, 1, 2]));
        assert!(back.extensions().is_empty());
        let mut ext = Extensions::new();
        ext.insert(66, Value::Uint(5));
        m.set_extensions(ext.clone()).unwrap();
        let back = Meta::decode(&m.encode()).unwrap();
        assert_eq!(back.extensions(), &ext);
        assert_eq!(back.encode(), m.encode());
        let mut git_as_ext = Extensions::new();
        git_as_ext.insert(64, Value::Uint(0));
        assert!(
            m.set_extensions(git_as_ext).is_err(),
            "64 is a known field, not an extension"
        );
    }

    fn mutate(m: &Meta, f: impl FnOnce(&mut BTreeMap<u64, Value>)) -> Vec<u8> {
        let Value::Map(mut map) = Value::decode(&m.encode()).unwrap() else {
            unreachable!()
        };
        f(&mut map);
        Value::Map(map).encode()
    }

    #[test]
    fn meta_decode_rejections() {
        let m = full();
        let cases: Vec<(Vec<u8>, Subcode)> = vec![
            (
                mutate(&m, |x| {
                    x.remove(&1);
                }),
                Subcode::InvalidField,
            ),
            (
                mutate(&m, |x| {
                    x.remove(&5);
                }),
                Subcode::InvalidField,
            ),
            (
                mutate(&m, |x| {
                    x.insert(1, Value::Bytes(vec![]));
                }),
                Subcode::InvalidField,
            ),
            (
                mutate(&m, |x| {
                    x.insert(2, Value::Array(vec![Value::Uint(0)]));
                }),
                Subcode::InvalidField,
            ),
            (
                mutate(&m, |x| {
                    x.insert(2, Value::Array(vec![Value::Uint(0), Value::Uint(1 << 40)]));
                }),
                Subcode::InvalidField,
            ),
            (
                mutate(&m, |x| {
                    x.insert(2, Value::Array(vec![Value::Uint(u64::MAX), Value::Uint(0)]));
                }),
                Subcode::InvalidField,
            ),
            (
                mutate(&m, |x| {
                    x.insert(2, Value::Uint(0));
                }),
                Subcode::InvalidField,
            ),
            (
                mutate(&m, |x| {
                    x.insert(64, Value::Text("x".into()));
                }),
                Subcode::InvalidField,
            ),
            (
                mutate(&m, |x| {
                    x.insert(65, Value::Uint(0));
                }),
                Subcode::UnknownCriticalExtension,
            ),
            (
                mutate(&m, |x| {
                    x.insert(8, Value::Uint(0));
                }),
                Subcode::InvalidField,
            ),
            (
                mutate(&m, |x| {
                    x.insert(0, Value::Uint(0));
                }),
                Subcode::UnknownFormatVersion,
            ),
        ];
        for (i, (bytes, sub)) in cases.into_iter().enumerate() {
            let err = Meta::decode(&bytes).unwrap_err();
            assert!(err.is_invalid(sub), "case {i}: {err}");
        }
    }

    #[test]
    fn provenance_rejections() {
        let base = full();
        let prov = |f: &dyn Fn(&mut BTreeMap<u64, Value>)| {
            mutate(&base, |x| {
                let Some(Value::Map(p)) = x.get_mut(&7) else {
                    unreachable!()
                };
                f(p);
            })
        };
        for (i, bytes) in [
            prov(&|p| {
                p.insert(1, Value::Array(vec![]));
            }),
            prov(&|p| {
                p.insert(1, Value::Array(vec![Value::Uint(1)]));
            }),
            prov(&|p| {
                p.insert(5, Value::Bytes(vec![1; 31]));
            }),
            prov(&|p| {
                p.insert(6, Value::Array(vec![]));
            }),
            prov(&|p| {
                p.insert(6, Value::Array(vec![Value::Map(BTreeMap::new())]));
            }),
            prov(&|p| {
                p.insert(7, Value::Map(BTreeMap::from([(4, Value::Uint(1))])));
            }),
            prov(&|p| {
                p.insert(8, Value::Uint(1));
            }),
            prov(&|p| {
                p.insert(64, Value::Uint(1));
            }),
            mutate(&base, |x| {
                x.insert(7, Value::Uint(0));
            }),
        ]
        .into_iter()
        .enumerate()
        {
            let err = Meta::decode(&bytes).unwrap_err();
            assert!(err.is_invalid(Subcode::InvalidField), "case {i}: {err}");
        }
    }

    #[test]
    fn empty_provenance_is_distinct_from_none() {
        let mut m = Meta::empty_root();
        m.provenance = Some(Provenance::default());
        let back = Meta::decode(&m.encode()).unwrap();
        assert_eq!(back.provenance, Some(Provenance::default()));
        assert_ne!(back.id(), Meta::empty_root().id());
    }

    #[test]
    fn message_limit() {
        let mut m = Meta::empty_root();
        m.message = "x".repeat(1024 * 1024 + 1);
        assert_eq!(
            m.validate(&Limits::DEFAULT).unwrap_err().reason,
            Reason::TooLarge
        );
        assert_eq!(
            Meta::decode(&m.encode()).unwrap_err().reason,
            Reason::TooLarge
        );
        m.message.pop();
        assert!(Meta::decode(&m.encode()).is_ok());
    }

    #[test]
    fn invalid_utf8_message_rejected() {
        let m = Meta::empty_root();
        let mut bytes = m.encode();
        // Replace the empty message text (last byte 0x60) with a 1-byte invalid text.
        bytes.pop();
        bytes.extend_from_slice(&[0x61, 0xff]);
        assert!(
            Meta::decode(&bytes)
                .unwrap_err()
                .is_invalid(Subcode::NonCanonical)
        );
    }
}
