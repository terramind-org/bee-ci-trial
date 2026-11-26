//! Mapping git ref names onto bee ref names (SPEC §5, §17.2).
//!
//! | git | bee |
//! |---|---|
//! | `refs/heads/<b>` | `<b>` |
//! | `refs/tags/<t>` | `tags/<t>` |
//! | `refs/pull/<rest>` | `pull/<rest>` (only with `include_pull_refs`) |
//! | `refs/<other>` | `git/<other>` (notes, remotes, anything else) |
//!
//! Names outside `refs/` (`HEAD`, `FETCH_HEAD`) are not refs and are dropped.
//!
//! bee's grammar is stricter than git's, so the part after the prefix is sanitized:
//! every byte that is not valid UTF-8, a control character, a space or one of
//! `~ ^ : ? * [ \ @` becomes `%XX` (uppercase hex); a component that starts or ends with
//! `.`, ends with `.lock` or contains `..` gets the offending dot written as `%2E`; an empty
//! component becomes `%`; a name that would be id-shaped has its first `_` written as
//! `%5F`; a name longer than 255 bytes is cut and suffixed with `-` and 8 hex digits of
//! BLAKE3 over the full git name. So `refs/heads/feature@x` becomes `feature%40x`.
//!
//! Names are assigned deterministically: names that need no change are taken first, in
//! category order (branches, tags, pull refs, other) and then bytewise by git name; renamed
//! refs follow in the same order. A name already taken gets `-` and 8 hex digits of BLAKE3
//! over the git name appended; if that is taken too the ref is dropped
//! (`ref_name_collision`). Every renamed ref is reported with its original name.

use std::collections::BTreeSet;

use bee_core::{RefName, hex};

use crate::source::GitRef;

/// What kind of ref a git name is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RefKind {
    /// `refs/heads/*`.
    Branch,
    /// `refs/tags/*`.
    Tag,
    /// `refs/pull/*`.
    PullRequest,
    /// Any other `refs/*`.
    Other,
}

impl RefKind {
    /// Classifies a full git ref name and returns the bee prefix and the rest of the name,
    /// or `None` for names outside `refs/`.
    pub fn classify(name: &[u8]) -> Option<(RefKind, &'static str, &[u8])> {
        let rest = name.strip_prefix(b"refs/")?;
        Some(if let Some(r) = rest.strip_prefix(b"heads/") {
            (RefKind::Branch, "", r)
        } else if let Some(r) = rest.strip_prefix(b"tags/") {
            (RefKind::Tag, "tags/", r)
        } else if let Some(r) = rest.strip_prefix(b"pull/") {
            (RefKind::PullRequest, "pull/", r)
        } else {
            (RefKind::Other, "git/", rest)
        })
    }

    /// Stable lowercase name.
    pub fn as_str(self) -> &'static str {
        match self {
            RefKind::Branch => "branch",
            RefKind::Tag => "tag",
            RefKind::PullRequest => "pull_request",
            RefKind::Other => "other",
        }
    }
}

/// A git ref with the bee name it maps to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MappedRef {
    /// The git ref.
    pub git: GitRef,
    /// Its kind.
    pub kind: RefKind,
    /// The bee ref name.
    pub bee: RefName,
    /// Why the name differs from the plain prefix mapping, when it does.
    pub renamed: Option<&'static str>,
}

/// A ref that could not be mapped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnmappedRef {
    /// The git ref.
    pub git: GitRef,
    /// A stable reason (`unsupported_ref_name`, `ref_name_collision`, `excluded_by_option`).
    pub reason: &'static str,
}

/// The result of [`map_ref_names`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RefMapping {
    /// Mapped refs, sorted by bee name.
    pub mapped: Vec<MappedRef>,
    /// Refs left out on purpose (pull refs without `include_pull_refs`).
    pub excluded: Vec<UnmappedRef>,
    /// Refs that cannot be carried.
    pub dropped: Vec<UnmappedRef>,
}

const RENAMED_SANITIZED: &str = "name not valid in bee's ref grammar";
const RENAMED_COLLISION: &str = "name collided with another ref";
const RENAMED_LONG: &str = "name longer than 255 bytes";

fn forbidden(c: char) -> bool {
    c.is_control() || c == ' ' || bee_core::refname::FORBIDDEN_REF_CHARS.contains(&c)
}

fn push_escaped(out: &mut String, bytes: &[u8]) {
    for b in bytes {
        out.push_str(&format!("%{b:02X}"));
    }
}

fn sanitize_component(c: &[u8]) -> String {
    if c.is_empty() {
        return "%".to_string();
    }
    let mut out = String::new();
    for chunk in c.utf8_chunks() {
        for ch in chunk.valid().chars() {
            if forbidden(ch) {
                let mut buf = [0u8; 4];
                push_escaped(&mut out, ch.encode_utf8(&mut buf).as_bytes());
            } else {
                out.push(ch);
            }
        }
        push_escaped(&mut out, chunk.invalid());
    }
    while out.contains("..") {
        out = out.replacen("..", ".%2E", 1);
    }
    if let Some(rest) = out.strip_prefix('.') {
        out = format!("%2E{rest}");
    }
    if let Some(rest) = out.strip_suffix(".lock") {
        out = format!("{rest}%2Elock");
    }
    if let Some(rest) = out.strip_suffix('.') {
        out = format!("{rest}%2E");
    }
    out
}

fn short_hash(name: &[u8]) -> String {
    hex::encode(&bee_object::digest(name)[..4])
}

/// The bee name for a git ref name before collision handling: `None` for names outside
/// `refs/`. The flag says whether sanitizing changed anything.
pub fn bee_ref_name(git_name: &[u8]) -> Option<(String, Option<&'static str>)> {
    let (_, prefix, rest) = RefKind::classify(git_name)?;
    let plain = std::str::from_utf8(rest)
        .ok()
        .map(|r| format!("{prefix}{r}"));
    let mut name = prefix.to_string();
    let parts: Vec<String> = rest.split(|b| *b == b'/').map(sanitize_component).collect();
    name.push_str(&parts.join("/"));
    if bee_core::RefName::new(name.clone()).is_err()
        && let Some(i) = name.find('_')
        && name[..i].len() <= 4
    {
        name.replace_range(i..=i, "%5F");
    }
    let mut why = (plain.as_deref() != Some(name.as_str())).then_some(RENAMED_SANITIZED);
    let max = bee_core::Limits::DEFAULT.ref_name_bytes as usize;
    if name.len() > max {
        let mut cut = max - 9;
        while !name.is_char_boundary(cut) {
            cut -= 1;
        }
        name.truncate(cut);
        while name.ends_with(['.', '/']) {
            name.pop();
        }
        name.push('-');
        name.push_str(&short_hash(git_name));
        why = Some(RENAMED_LONG);
    }
    Some((name, why))
}

/// Maps every git ref to a bee name (see the module docs for the rules).
pub fn map_ref_names(refs: &[GitRef], include_pull_refs: bool) -> RefMapping {
    let mut out = RefMapping::default();
    let mut candidates: Vec<(RefKind, GitRef, String, Option<&'static str>)> = Vec::new();
    for r in refs {
        let Some((kind, _, _)) = RefKind::classify(&r.name) else {
            out.dropped.push(UnmappedRef {
                git: r.clone(),
                reason: "unsupported_ref_name",
            });
            continue;
        };
        if kind == RefKind::PullRequest && !include_pull_refs {
            out.excluded.push(UnmappedRef {
                git: r.clone(),
                reason: "excluded_by_option",
            });
            continue;
        }
        match bee_ref_name(&r.name) {
            Some((name, why)) if RefName::new(name.clone()).is_ok() => {
                candidates.push((kind, r.clone(), name, why))
            }
            _ => out.dropped.push(UnmappedRef {
                git: r.clone(),
                reason: "unsupported_ref_name",
            }),
        }
    }
    candidates
        .sort_by(|a, b| (a.3.is_some(), a.0, &a.1.name).cmp(&(b.3.is_some(), b.0, &b.1.name)));
    let mut taken: BTreeSet<String> = BTreeSet::new();
    for (kind, git, name, why) in candidates {
        let (name, why) = if taken.contains(&name) {
            (
                format!("{name}-{}", short_hash(&git.name)),
                Some(RENAMED_COLLISION),
            )
        } else {
            (name, why)
        };
        match RefName::new(name.clone()) {
            Ok(bee) if !taken.contains(&name) => {
                taken.insert(name);
                out.mapped.push(MappedRef {
                    git,
                    kind,
                    bee,
                    renamed: why,
                });
            }
            _ => out.dropped.push(UnmappedRef {
                git,
                reason: "ref_name_collision",
            }),
        }
    }
    out.mapped.sort_by(|a, b| a.bee.cmp(&b.bee));
    out.dropped.sort_by(|a, b| a.git.name.cmp(&b.git.name));
    out.excluded.sort_by(|a, b| a.git.name.cmp(&b.git.name));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use bee_git::GitOid;

    fn name(git: &[u8]) -> String {
        bee_ref_name(git).unwrap().0
    }

    #[test]
    fn prefixes_map_by_kind() {
        assert_eq!(name(b"refs/heads/main"), "main");
        assert_eq!(name(b"refs/heads/release/1.0"), "release/1.0");
        assert_eq!(name(b"refs/tags/v1.2"), "tags/v1.2");
        assert_eq!(name(b"refs/pull/12/head"), "pull/12/head");
        assert_eq!(name(b"refs/notes/commits"), "git/notes/commits");
        assert_eq!(name(b"refs/remotes/origin/x"), "git/remotes/origin/x");
        assert_eq!(bee_ref_name(b"HEAD"), None);
        assert_eq!(bee_ref_name(b"refs/heads/main").unwrap().1, None);
    }

    #[test]
    fn invalid_names_are_escaped() {
        let cases: &[(&[u8], &str)] = &[
            (b"refs/heads/feature@x", "feature%40x"),
            (b"refs/heads/a:b", "a%3Ab"),
            (b"refs/heads/caf\xe9", "caf%E9"),
            (b"refs/heads/caf\xc3\xa9", "caf\u{e9}"),
            (b"refs/heads/x.lock", "x%2Elock"),
            (b"refs/heads/.hidden/y.", "%2Ehidden/y%2E"),
            (b"refs/heads/a..b", "a.%2Eb"),
            (b"refs/heads/c1_abcdef", "c1%5Fabcdef"),
            (b"refs/heads/k_00", "k%5F00"),
            (b"refs/heads/a//b", "a/%/b"),
        ];
        for (git, want) in cases {
            let (got, why) = bee_ref_name(git).unwrap();
            assert_eq!(&got, want, "{}", String::from_utf8_lossy(git));
            assert!(RefName::new(got.clone()).is_ok(), "{got}");
            assert_eq!(why.is_some(), *want != "caf\u{e9}", "{got}");
        }
        let long = format!("refs/heads/{}", "a".repeat(300));
        let (got, why) = bee_ref_name(long.as_bytes()).unwrap();
        assert_eq!(got.len(), 255);
        assert!(RefName::new(got).is_ok());
        assert_eq!(why, Some(RENAMED_LONG));
    }

    #[test]
    fn mapping_is_deterministic_and_resolves_collisions() {
        let t = GitOid::Sha1([1; 20]);
        let refs = vec![
            GitRef::new("refs/heads/a%40b", t),
            GitRef::new("refs/heads/a@b", t),
            GitRef::new("refs/tags/v1", t),
            GitRef::new("refs/heads/tags/v1", t),
            GitRef::new("refs/pull/1/head", t),
            GitRef::new("HEAD", t),
        ];
        let m = map_ref_names(&refs, false);
        let mut rev = refs.clone();
        rev.reverse();
        assert_eq!(map_ref_names(&rev, false), m);
        let by_git = |g: &str| {
            m.mapped
                .iter()
                .find(|r| r.git.name == g.as_bytes())
                .map(|r| (r.bee.as_str().to_string(), r.renamed))
                .unwrap()
        };
        // The literal name wins; the sanitized one gets a hash suffix.
        assert_eq!(by_git("refs/heads/a%40b"), ("a%40b".to_string(), None));
        let (renamed, why) = by_git("refs/heads/a@b");
        assert!(renamed.starts_with("a%40b-"), "{renamed}");
        assert_eq!(why, Some(RENAMED_COLLISION));
        // Branches are assigned before tags.
        assert_eq!(by_git("refs/heads/tags/v1"), ("tags/v1".to_string(), None));
        assert!(by_git("refs/tags/v1").0.starts_with("tags/v1-"));
        assert_eq!(m.excluded.len(), 1);
        assert_eq!(m.dropped.len(), 1);
        assert_eq!(m.dropped[0].reason, "unsupported_ref_name");
        let with_pull = map_ref_names(&refs, true);
        assert!(
            with_pull
                .mapped
                .iter()
                .any(|r| r.bee.as_str() == "pull/1/head" && r.kind == RefKind::PullRequest)
        );
    }
}
