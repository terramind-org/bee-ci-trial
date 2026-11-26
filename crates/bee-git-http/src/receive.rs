//! `git-receive-pack`: pushes become changesets, workspace ref updates and Lands.
//!
//! For each pushed ref (SPEC §17.4):
//!
//! - the pushed commits are converted with their exact git ids and checked like
//!   CreateChangeset ([`crate::ingest`]);
//! - a **protected** ref (policy land rule, `main`, `release/*`) moves only through **Land**:
//!   the new commit must descend from the client's old value (else the push is a force push,
//!   refused with `NOT_FAST_FORWARD`), and the stack is landed through the coalescer under
//!   policy. When the tip moved meanwhile, Land rebases the stack and the report tells the
//!   client the ref's real new id (`option new-oid`, report-status v2). Deleting a protected
//!   ref is `POLICY_DENIED`;
//! - any other ref is a compare-and-swap from the client's old value: a stale old value is
//!   `VERSION_MISMATCH` ("fetch first"), a non-fast-forward move needs the `force`
//!   permission;
//! - refused refs are reported as `ng <ref> <REASON>: <message>`.
//!
//! An `atomic` push validates every command before writing anything, lands at most one
//! protected ref, writes the other refs first and rolls them back (compare-and-swap) if a later
//! write or the land fails. Push options are passed to the commit checks
//! (`push-protection-bypass=<reason>`).

use std::collections::BTreeMap;

use bee_core::{ObjectId, Reason, RefName};
use bee_git::{GitOid, ObjectFormat, Pack, RawObject};

use crate::advertise::{RefList, ref_tag_rows};
use crate::backend::{PushContext, RefState, RepoSession};
use crate::error::{GitError, GitResult};
use crate::gitmap::GitView;
use crate::ingest::Ingest;
use crate::pktline::{self, BAND_DATA, Pkt, Reader};
use crate::refs::bee_name;

/// One pushed ref update.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Command {
    /// The client's view of the ref (null id: create).
    pub old: GitOid,
    /// The new value (null id: delete).
    pub new: GitOid,
    /// The git ref name.
    pub name: String,
}

/// A parsed push request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PushRequest {
    /// The commands.
    pub commands: Vec<Command>,
    /// Capabilities the client asked for.
    pub caps: Vec<String>,
    /// Push options.
    pub options: Vec<String>,
    /// The commits the pushing clone is shallow at (its history boundary). The repo must hold
    /// each of them; the pack then needs nothing below them.
    pub shallow: Vec<GitOid>,
    /// The pack bytes (empty for a delete-only push).
    pub pack: Vec<u8>,
}

impl PushRequest {
    /// Whether the client asked for a capability.
    pub fn has(&self, cap: &str) -> bool {
        self.caps.iter().any(|c| c == cap)
    }
}

/// Parses a receive-pack request body.
pub fn parse(format: ObjectFormat, body: &[u8]) -> GitResult<PushRequest> {
    let mut r = Reader::new(body);
    let mut commands = Vec::new();
    let mut caps = Vec::new();
    let mut shallow = Vec::new();
    loop {
        match r.next_pkt()? {
            Some(Pkt::Data(d)) => {
                let d = d.strip_suffix(b"\n").unwrap_or(d);
                let (cmd, cap) = match d.iter().position(|&b| b == 0) {
                    Some(z) => (&d[..z], Some(&d[z + 1..])),
                    None => (d, None),
                };
                if let Some(cap) = cap {
                    let text = String::from_utf8_lossy(cap);
                    caps.extend(
                        text.split(' ')
                            .filter(|c| !c.is_empty())
                            .map(str::to_string),
                    );
                }
                let text = std::str::from_utf8(cmd)
                    .map_err(|_| GitError::invalid("push command is not UTF-8"))?;
                if let Some(s) = text.strip_prefix("shallow ") {
                    shallow.push(
                        format
                            .parse_hex(s.trim().as_bytes())
                            .map_err(|_| GitError::invalid(format!("bad shallow id {s:?}")))?,
                    );
                    continue;
                }
                let mut parts = text.splitn(3, ' ');
                let (Some(old), Some(new), Some(name)) = (parts.next(), parts.next(), parts.next())
                else {
                    return Err(GitError::invalid(format!(
                        "malformed push command {text:?}"
                    )));
                };
                let parse = |h: &str| {
                    format
                        .parse_hex(h.as_bytes())
                        .map_err(|_| GitError::invalid(format!("bad object id {h:?}")))
                };
                commands.push(Command {
                    old: parse(old)?,
                    new: parse(new)?,
                    name: name.to_string(),
                });
            }
            Some(Pkt::Flush) | None => break,
            Some(_) => return Err(GitError::invalid("unexpected packet in push commands")),
        }
    }
    let mut options = Vec::new();
    if caps.iter().any(|c| c == "push-options") {
        loop {
            match r.next_pkt()? {
                Some(Pkt::Data(d)) => options
                    .push(String::from_utf8_lossy(d.strip_suffix(b"\n").unwrap_or(d)).into_owned()),
                Some(Pkt::Flush) | None => break,
                Some(_) => return Err(GitError::invalid("unexpected packet in push options")),
            }
        }
    }
    Ok(PushRequest {
        commands,
        caps,
        options,
        shallow,
        pack: r.rest().to_vec(),
    })
}

/// The outcome of one command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Updated; `new` is set when the ref ended somewhere other than the pushed id.
    Ok {
        /// The ref's real new id (a rebased land).
        new: Option<GitOid>,
    },
    /// Refused with a reason.
    Ng(GitError),
}

/// A planned ref write.
struct Planned {
    index: usize,
    bee: RefName,
    protected: bool,
    /// The changeset to point at (`None`: delete).
    target: Option<ObjectId>,
    /// The ref's state when validated.
    current: Option<(ObjectId, u64)>,
    force: bool,
    /// Annotated tags met while peeling the new value (outermost first).
    tags: Vec<(GitOid, Vec<u8>)>,
}

/// Executes a push and returns each command's outcome, in order.
pub async fn execute(
    session: &dyn RepoSession,
    view: &mut GitView<'_>,
    refs: &RefList,
    req: &PushRequest,
    at_ms: u64,
) -> GitResult<(Result<(), GitError>, Vec<Outcome>)> {
    let format = view.format();
    let me = session.principal().to_string();
    let mut outcomes: Vec<Option<Outcome>> = vec![None; req.commands.len()];
    // A shallow clone's boundary commits must be history the repo has: their parents are
    // not in the pack, and the repo supplies them.
    for s in &req.shallow {
        let known = match view.bee_of(s).await? {
            Some(id) => {
                id.kind() == bee_core::ObjectKind::Changeset
                    && session.history().has_changeset(&id).await?
            }
            None => false,
        };
        if !known {
            let e = GitError::invalid(format!(
                "this clone is shallow at {s}, which the repo does not have: fetch from this \
                 repo (or unshallow) before pushing"
            ));
            return Ok((Err(e.clone()), vec![Outcome::Ng(e); req.commands.len()]));
        }
    }
    // Unpack.
    let needs_pack = req.commands.iter().any(|c| !c.new.is_null());
    let mut objects: BTreeMap<GitOid, RawObject> = BTreeMap::new();
    let unpack: Result<(), GitError> = if needs_pack && !req.pack.is_empty() {
        let limit = session.max_blob_bytes().max(1 << 20);
        match Pack::parse_with_limit(&req.pack, format, limit) {
            Ok(pack) => {
                let res = pack.resolve(&|_| Ok(None), &mut |e| {
                    objects.insert(e.oid, e.object);
                    Ok(())
                });
                res.map_err(GitError::from)
            }
            Err(e) => Err(GitError::from(e)),
        }
    } else {
        Ok(())
    };
    if let Err(e) = &unpack {
        let ng = GitError::named(e.reason.clone(), format!("unpacker error: {}", e.message));
        return Ok((unpack.clone(), vec![Outcome::Ng(ng); req.commands.len()]));
    }
    let push = PushContext::from_options(req.options.clone());
    let mut ingest = Ingest::new(session, view, objects, push, at_ms);
    // Validate every command and ingest its commits.
    let mut planned: Vec<Planned> = Vec::new();
    for (i, c) in req.commands.iter().enumerate() {
        match validate(session, &mut ingest, refs, &me, c, i).await {
            Ok(p) => planned.push(p),
            Err(e) => outcomes[i] = Some(Outcome::Ng(e)),
        }
    }
    let atomic = req.has("atomic");
    if atomic {
        if planned.iter().filter(|p| p.protected).count() > 1 {
            let e = GitError::invalid("an atomic push may land at most one protected ref");
            return Ok((unpack, vec![Outcome::Ng(e); req.commands.len()]));
        }
        if outcomes.iter().any(Option::is_some) {
            let fail = GitError::named(
                "INVALID_STATE",
                "atomic push failed: another ref was refused",
            );
            let out = outcomes
                .into_iter()
                .map(|o| o.unwrap_or_else(|| Outcome::Ng(fail.clone())))
                .collect();
            return Ok((unpack, out));
        }
    }
    let _publication = if atomic && planned.len() > 1 {
        match session.atomic_publication().await {
            Ok(permit) => permit,
            Err(e) => return Ok((unpack, vec![Outcome::Ng(e); req.commands.len()])),
        }
    } else {
        None
    };
    // Plain writes first, then lands (an atomic push rolls the plain writes back if the land
    // is refused).
    planned.sort_by_key(|p| p.protected);
    let mut written: Vec<(usize, RefName, Option<ObjectId>, RefState)> = Vec::new();
    let mut failed: Option<GitError> = None;
    for p in &planned {
        if atomic && failed.is_some() {
            break;
        }
        let r = write(session, ingest.view_mut(), p).await;
        match r {
            Ok((state, new)) => {
                if !p.tags.is_empty()
                    && let (Some(target), Some((tag, bytes))) = (state.target, p.tags.first())
                    && let Ok(rows) = ref_tag_rows(&p.bee, &target, tag, bytes)
                {
                    ingest.view_mut().history().put_derived(rows).await?;
                }
                if !p.protected {
                    written.push((p.index, p.bee.clone(), p.current.map(|c| c.0), state));
                }
                outcomes[p.index] = Some(Outcome::Ok { new });
            }
            Err(e) => {
                failed = Some(e.clone());
                outcomes[p.index] = Some(Outcome::Ng(e));
            }
        }
    }
    if atomic && let Some(e) = failed {
        // Roll back the plain writes that went through.
        for (index, name, old, state) in written.iter().rev() {
            let _ = session.write_ref(name, state.version, *old, true).await;
            outcomes[*index] = Some(Outcome::Ng(GitError::named(
                "INVALID_STATE",
                format!("atomic push failed and was rolled back: {}", e.line()),
            )));
        }
        let fail = GitError::named("INVALID_STATE", format!("atomic push failed: {}", e.line()));
        for o in outcomes.iter_mut() {
            if o.is_none() {
                *o = Some(Outcome::Ng(fail.clone()));
            }
        }
    }
    let out = outcomes
        .into_iter()
        .map(|o| o.unwrap_or_else(|| Outcome::Ng(GitError::internal("command was not processed"))))
        .collect();
    Ok((unpack, out))
}

async fn validate(
    session: &dyn RepoSession,
    ingest: &mut Ingest<'_, '_, '_>,
    refs: &RefList,
    me: &str,
    c: &Command,
    index: usize,
) -> GitResult<Planned> {
    let bee = bee_name(&c.name, me)?;
    if !session.ref_visible(&bee) {
        return Err(GitError::new(
            Reason::PermissionDenied,
            format!("this token may not write {}", bee.as_str()),
        ));
    }
    let h = session.history();
    let current = h
        .meta()
        .get_ref(h.repo(), &bee)
        .await?
        .and_then(|r| r.target.map(|t| (t, r.version)));
    let protected = session.is_protected(&bee).await?;
    let current_oid = match &current {
        Some((t, _)) => Some(ingest.view_mut().commit_oid(t).await?),
        None => None,
    };
    let advertised = refs.get(&c.name).map(|r| (r.oid, r.peeled));
    let old_matches = |old: &GitOid| match (&current_oid, advertised) {
        (None, _) => old.is_null(),
        (Some(cur), adv) => old == cur || adv.is_some_and(|(o, p)| *old == o || Some(*old) == p),
    };
    if c.new.is_null() {
        if protected {
            return Err(GitError::new(
                Reason::PolicyDenied,
                format!(
                    "{} is protected; it moves only through Land and cannot be deleted over git",
                    bee.as_str()
                ),
            ));
        }
        if current.is_none() {
            return Err(GitError::not_found(format!("{} does not exist", c.name)));
        }
        if !old_matches(&c.old) {
            return Err(stale(&c.name));
        }
        return Ok(Planned {
            index,
            bee,
            protected,
            target: None,
            current,
            force: false,
            tags: Vec::new(),
        });
    }
    let (commit, tags) = ingest.peel(&c.new).await?;
    if !tags.is_empty() && !bee.as_str().starts_with("tags/") {
        return Err(GitError::invalid(format!(
            "{} is an annotated tag; only refs/tags/* may point at tags",
            c.new
        )));
    }
    let target = ingest.commit(&commit).await?;
    let history = session.history();
    if protected {
        // The push must not rewrite history the client saw: the new commit descends from the
        // client's old value. A moved tip is fine: Land rebases onto it.
        if !c.old.is_null() {
            let old_cs = match ingest.view_mut().bee_of(&c.old).await? {
                Some(id) => id,
                None => return Err(stale(&c.name)),
            };
            if !history.is_ancestor(&old_cs, &target).await? {
                return Err(GitError::new(
                    Reason::NotFastForward,
                    format!(
                        "{} is protected: force pushes are refused; it moves only through Land \
                         (rebase onto the current tip and push again)",
                        bee.as_str()
                    ),
                ));
            }
        } else if current.is_some() {
            return Err(stale(&c.name));
        }
        return Ok(Planned {
            index,
            bee,
            protected,
            target: Some(target),
            current,
            force: false,
            tags,
        });
    }
    if !old_matches(&c.old) {
        return Err(stale(&c.name));
    }
    let force = match &current {
        Some((t, _)) => !history.is_ancestor(t, &target).await?,
        None => false,
    };
    Ok(Planned {
        index,
        bee,
        protected,
        target: Some(target),
        current,
        force,
        tags,
    })
}

fn stale(name: &str) -> GitError {
    GitError::new(
        Reason::VersionMismatch,
        format!("{name} moved since you fetched it; fetch first"),
    )
}

async fn write(
    session: &dyn RepoSession,
    view: &mut GitView<'_>,
    p: &Planned,
) -> GitResult<(RefState, Option<GitOid>)> {
    if p.protected {
        let target = p
            .target
            .ok_or_else(|| GitError::internal("land without a head"))?;
        let state = session.land(&p.bee, &target).await?;
        let new = match state.target {
            Some(t) if t != target => Some(view.commit_oid(&t).await?),
            _ => None,
        };
        view.flush().await?;
        return Ok((state, new));
    }
    let expected = p.current.map_or(0, |c| c.1);
    let state = session
        .write_ref(&p.bee, expected, p.target, p.force)
        .await?;
    Ok((state, None))
}

/// The report-status (v1 or v2) for a push, framed for side-band when asked.
pub fn report(req: &PushRequest, unpack: &Result<(), GitError>, outcomes: &[Outcome]) -> Vec<u8> {
    let v2 = req.has("report-status-v2");
    let wanted = v2 || req.has("report-status");
    let mut status = Vec::new();
    if wanted {
        match unpack {
            Ok(()) => pktline::text(&mut status, "unpack ok\n"),
            Err(e) => pktline::text(&mut status, &format!("unpack {}\n", e.line())),
        }
        for (c, o) in req.commands.iter().zip(outcomes) {
            match o {
                Outcome::Ok { new } => {
                    pktline::text(&mut status, &format!("ok {}\n", c.name));
                    if v2 && let Some(n) = new {
                        pktline::text(&mut status, &format!("option refname {}\n", c.name));
                        pktline::text(&mut status, &format!("option old-oid {}\n", c.old));
                        pktline::text(&mut status, &format!("option new-oid {n}\n"));
                    }
                }
                Outcome::Ng(e) => {
                    pktline::text(&mut status, &format!("ng {} {}\n", c.name, e.line()))
                }
            }
        }
        pktline::flush(&mut status);
    }
    if req.has("side-band-64k") {
        let mut out = Vec::new();
        if !status.is_empty() {
            pktline::sideband(&mut out, BAND_DATA, &status);
        }
        pktline::flush(&mut out);
        out
    } else {
        status
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_commands_options_and_pack() {
        let f = ObjectFormat::Sha1;
        let z = GitOid::null(f);
        let a = GitOid::Sha1([0xaa; 20]);
        let mut body = Vec::new();
        pktline::data(
            &mut body,
            format!(
                "{z} {a} refs/heads/main\0report-status-v2 side-band-64k push-options atomic\n"
            )
            .as_bytes(),
        );
        pktline::text(&mut body, &format!("{a} {z} refs/heads/old\n"));
        pktline::flush(&mut body);
        pktline::text(&mut body, "push-protection-bypass=used_in_tests\n");
        pktline::flush(&mut body);
        body.extend_from_slice(b"PACK....");
        let req = parse(f, &body).unwrap();
        assert_eq!(req.commands.len(), 2);
        assert_eq!(req.commands[0].name, "refs/heads/main");
        assert!(req.commands[1].new.is_null());
        assert!(req.has("atomic") && req.has("report-status-v2"));
        assert_eq!(req.options, vec!["push-protection-bypass=used_in_tests"]);
        assert_eq!(req.pack, b"PACK....");
        let ctx = PushContext::from_options(req.options.clone());
        assert_eq!(ctx.bypass.as_deref(), Some("used_in_tests"));
    }

    #[test]
    fn report_lines() {
        let f = ObjectFormat::Sha1;
        let a = GitOid::Sha1([0xaa; 20]);
        let b = GitOid::Sha1([0xbb; 20]);
        let req = PushRequest {
            commands: vec![
                Command {
                    old: GitOid::null(f),
                    new: a,
                    name: "refs/heads/main".into(),
                },
                Command {
                    old: GitOid::null(f),
                    new: a,
                    name: "refs/heads/x".into(),
                },
            ],
            caps: vec!["report-status-v2".into()],
            options: vec![],
            shallow: vec![],
            pack: vec![],
        };
        let out = report(
            &req,
            &Ok(()),
            &[
                Outcome::Ok { new: Some(b) },
                Outcome::Ng(GitError::named("SECRET_DETECTED", "found a secret")),
            ],
        );
        let text = String::from_utf8_lossy(&out);
        assert!(text.contains("unpack ok\n"));
        assert!(text.contains("ok refs/heads/main\n"));
        assert!(text.contains(&format!("option new-oid {b}\n")));
        assert!(text.contains("ng refs/heads/x SECRET_DETECTED: found a secret\n"));
    }
}
