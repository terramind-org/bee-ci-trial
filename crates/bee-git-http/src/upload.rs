//! `git-upload-pack`: ref advertisement, protocol v2 (`ls-refs`, `fetch`) and the v0/v1
//! stateless-RPC negotiation git uses over HTTP.

use std::sync::Arc;

use bee_core::{ObjectKind, Reason};
use bee_git::{GitOid, ObjectFormat};

use crate::advertise::RefList;
use crate::backend::RepoSession;
use crate::cache::{CacheUse, PackCache, key};
use crate::error::{GitError, GitResult};
use crate::gitmap::GitView;
use crate::pktline::{self, BAND_DATA, BAND_PROGRESS, Pkt, Reader};
use crate::walk::{Deepen, FetchArgs, FetchPlan, Filter, plan};

/// The agent string sent to clients.
pub fn agent() -> String {
    format!("agent=bee/{}", env!("CARGO_PKG_VERSION"))
}

fn utf8(line: &[u8]) -> GitResult<&str> {
    std::str::from_utf8(line).map_err(|_| GitError::invalid("request line is not UTF-8"))
}

fn oid(format: ObjectFormat, hex: &str) -> GitResult<GitOid> {
    format
        .parse_hex(hex.as_bytes())
        .map_err(|_| GitError::invalid(format!("{hex:?} is not a {} object id", format.name())))
}

/// The protocol v2 capability advertisement.
pub fn advertise_v2(format: ObjectFormat) -> Vec<u8> {
    let mut out = Vec::new();
    pktline::text(&mut out, "version 2\n");
    pktline::text(&mut out, &format!("{}\n", agent()));
    pktline::text(&mut out, "ls-refs=unborn\n");
    pktline::text(&mut out, "fetch=shallow filter\n");
    pktline::text(&mut out, "server-option\n");
    pktline::text(&mut out, "object-info\n");
    pktline::text(&mut out, &format!("object-format={}\n", format.name()));
    pktline::flush(&mut out);
    out
}

/// Capabilities of the v0/v1 upload-pack advertisement.
fn upload_caps(refs: &RefList) -> String {
    format!(
        "multi_ack_detailed side-band-64k thin-pack ofs-delta shallow deepen-since deepen-not \
         deepen-relative no-progress include-tag allow-tip-sha1-in-want \
         allow-reachable-sha1-in-want filter symref=HEAD:{} object-format={} {}",
        refs.head,
        refs.format.name(),
        agent()
    )
}

/// Capabilities of the receive-pack advertisement.
pub fn receive_caps(format: ObjectFormat) -> String {
    format!(
        "report-status report-status-v2 delete-refs side-band-64k quiet atomic ofs-delta \
         push-options no-thin object-format={} {}",
        format.name(),
        agent()
    )
}

/// A v0/v1 ref advertisement: the `# service=` header for smart HTTP, `version 1` when asked,
/// then the refs with capabilities on the first line (and peeled tags).
pub fn advertise_v0(service: &str, refs: &RefList, version1: bool, upload: bool) -> Vec<u8> {
    let mut out = Vec::new();
    pktline::text(&mut out, &format!("# service={service}\n"));
    pktline::flush(&mut out);
    out.extend(advertise_refs(refs, version1, upload));
    out
}

/// The v0/v1 ref advertisement itself, as sent first on an SSH session (no `# service=`
/// header): `version 1` when asked, then the refs with capabilities on the first line.
pub fn advertise_refs(refs: &RefList, version1: bool, upload: bool) -> Vec<u8> {
    let mut out = Vec::new();
    if version1 {
        pktline::text(&mut out, "version 1\n");
    }
    let caps = if upload {
        upload_caps(refs)
    } else {
        receive_caps(refs.format)
    };
    let mut lines: Vec<(GitOid, String)> = Vec::new();
    if upload && let Some(h) = refs.head_ref() {
        lines.push((h.oid, "HEAD".into()));
    }
    for r in &refs.refs {
        lines.push((r.oid, r.name.clone()));
        if let Some(p) = r.peeled
            && upload
        {
            lines.push((p, format!("{}^{{}}", r.name)));
        }
    }
    if lines.is_empty() {
        let zero = GitOid::null(refs.format);
        pktline::text(&mut out, &format!("{zero} capabilities^{{}}\0{caps}\n"));
    }
    for (i, (o, name)) in lines.iter().enumerate() {
        if i == 0 {
            pktline::text(&mut out, &format!("{o} {name}\0{caps}\n"));
        } else {
            pktline::text(&mut out, &format!("{o} {name}\n"));
        }
    }
    pktline::flush(&mut out);
    out
}

/// A parsed v2 command request.
struct Command<'a> {
    name: String,
    args: Vec<&'a [u8]>,
}

fn parse_command(body: &[u8]) -> GitResult<Command<'_>> {
    let mut r = Reader::new(body);
    let mut name = None;
    // The command and capability lines, up to the delimiter (or a flush: no arguments).
    let ended_with_delim = loop {
        match r.expect_pkt()? {
            Pkt::Data(d) => {
                let line = utf8(d.strip_suffix(b"\n").unwrap_or(d))?;
                if let Some(c) = line.strip_prefix("command=") {
                    name = Some(c.to_string());
                }
            }
            Pkt::Delim => break true,
            Pkt::Flush => break false,
            Pkt::ResponseEnd => return Err(GitError::invalid("unexpected response-end")),
        }
    };
    let mut args = Vec::new();
    if ended_with_delim {
        loop {
            match r.expect_pkt()? {
                Pkt::Data(d) => args.push(d.strip_suffix(b"\n").unwrap_or(d)),
                Pkt::Flush => break,
                _ => return Err(GitError::invalid("unexpected packet in command arguments")),
            }
        }
    }
    let name = name.ok_or_else(|| GitError::invalid("protocol v2 request without command="))?;
    Ok(Command { name, args })
}

/// Serves a protocol v2 request (`ls-refs` or `fetch`).
pub async fn serve_v2(
    session: &dyn RepoSession,
    view: &mut GitView<'_>,
    refs: &RefList,
    cache: &CacheUse<'_>,
    body: &[u8],
) -> GitResult<Vec<u8>> {
    let cmd = parse_command(body)?;
    match cmd.name.as_str() {
        "ls-refs" => ls_refs(refs, &cmd.args),
        "fetch" => fetch_v2(session, view, refs, cache, &cmd.args).await,
        "object-info" => object_info(view, &cmd.args).await,
        other => Err(GitError::new(
            Reason::Unimplemented,
            format!("unknown protocol v2 command {other:?}"),
        )),
    }
}

/// Most object ids one `object-info` request may ask about.
const MAX_OBJECT_INFO: usize = 10_000;

/// `object-info`: the size of each requested object (`size` attribute, `oid <id>` lines).
/// An object the repo does not have, or that is redacted, gets an empty size, as git does
/// for objects it cannot answer for.
async fn object_info(view: &mut GitView<'_>, args: &[&[u8]]) -> GitResult<Vec<u8>> {
    let format = view.format();
    let mut size = false;
    let mut oids = Vec::new();
    for a in args {
        let a = utf8(a)?;
        if a == "size" {
            size = true;
        } else if let Some(hex) = a.strip_prefix("oid ") {
            oids.push(oid(format, hex)?);
        } else {
            return Err(GitError::invalid(format!(
                "unexpected object-info argument {a:?}"
            )));
        }
    }
    if oids.len() > MAX_OBJECT_INFO {
        return Err(GitError::new(
            Reason::TooLarge,
            format!("at most {MAX_OBJECT_INFO} object ids per object-info request"),
        ));
    }
    let mut out = Vec::new();
    if size {
        pktline::text(&mut out, "size\n");
    }
    for o in &oids {
        let n = if size {
            object_size(view, o).await?
        } else {
            None
        };
        match n {
            Some(n) => pktline::text(&mut out, &format!("{o} {n}\n")),
            None => pktline::text(&mut out, &format!("{o} \n")),
        }
    }
    pktline::flush(&mut out);
    view.flush().await?;
    Ok(out)
}

/// The git size of an object (its content length), `None` when unknown or redacted.
async fn object_size(view: &mut GitView<'_>, o: &GitOid) -> GitResult<Option<u64>> {
    let Some(id) = view.bee_of(o).await? else {
        return Ok(None);
    };
    let r = match id.kind() {
        ObjectKind::Changeset => view.commit_object(&id).await.map(|(_, raw)| raw.data.len()),
        ObjectKind::Tree => view.tree_object(&id).await.map(|t| t.raw.data.len()),
        ObjectKind::Blob => view.blob_bytes(&id).await.map(|b| b.len()),
        _ => return Ok(None),
    };
    match r {
        Ok(n) => Ok(Some(n as u64)),
        Err(e) if e.reason == "REDACTED" || e.reason == "NOT_FOUND" => Ok(None),
        Err(e) => Err(e),
    }
}

fn ls_refs(refs: &RefList, args: &[&[u8]]) -> GitResult<Vec<u8>> {
    let mut peel = false;
    let mut symrefs = false;
    let mut unborn = false;
    let mut prefixes: Vec<String> = Vec::new();
    for a in args {
        let a = utf8(a)?;
        match a {
            "peel" => peel = true,
            "symrefs" => symrefs = true,
            "unborn" => unborn = true,
            _ => match a.strip_prefix("ref-prefix ") {
                Some(p) => prefixes.push(p.to_string()),
                None => {
                    return Err(GitError::invalid(format!(
                        "unexpected ls-refs argument {a:?}"
                    )));
                }
            },
        }
    }
    let wanted = |name: &str| prefixes.is_empty() || prefixes.iter().any(|p| name.starts_with(p));
    let mut out = Vec::new();
    if wanted("HEAD") {
        let symref = if symrefs {
            format!(" symref-target:{}", refs.head)
        } else {
            String::new()
        };
        match refs.head_ref() {
            Some(h) => pktline::text(&mut out, &format!("{} HEAD{symref}\n", h.oid)),
            None if unborn => pktline::text(&mut out, &format!("unborn HEAD{symref}\n")),
            None => {}
        }
    }
    for r in &refs.refs {
        if !wanted(&r.name) {
            continue;
        }
        let mut line = format!("{} {}", r.oid, r.name);
        if peel && let Some(p) = r.peeled {
            line.push_str(&format!(" peeled:{p}"));
        }
        line.push('\n');
        pktline::text(&mut out, &line);
    }
    pktline::flush(&mut out);
    Ok(out)
}

/// Fetch arguments shared by v0 and v2, before deepen-not refs are resolved.
#[derive(Default)]
struct Parsed {
    args: FetchArgs,
    deepen_not: Vec<String>,
    depth: Option<u32>,
    relative: bool,
    since: Option<i64>,
    done: bool,
    no_progress: bool,
    sideband: bool,
}

impl Parsed {
    /// Handles a line both protocols share; false if the line is not one of them.
    fn common(&mut self, format: ObjectFormat, line: &str) -> GitResult<bool> {
        if let Some(h) = line.strip_prefix("have ") {
            self.args.haves.push(oid(format, h.trim())?);
        } else if let Some(s) = line.strip_prefix("shallow ") {
            self.args.shallow.push(oid(format, s.trim())?);
        } else if let Some(d) = line.strip_prefix("deepen ") {
            let n: u32 = d
                .trim()
                .parse()
                .map_err(|_| GitError::invalid(format!("bad deepen {d:?}")))?;
            self.depth = Some(n);
        } else if let Some(t) = line.strip_prefix("deepen-since ") {
            let t: i64 = t
                .trim()
                .parse()
                .map_err(|_| GitError::invalid(format!("bad deepen-since {t:?}")))?;
            self.since = Some(t);
        } else if let Some(r) = line.strip_prefix("deepen-not ") {
            self.deepen_not.push(r.trim().to_string());
        } else if line == "deepen-relative" {
            self.relative = true;
        } else if let Some(f) = line.strip_prefix("filter ") {
            self.args.filter = Some(Filter::parse(f.trim())?);
        } else if line == "done" {
            self.done = true;
        } else {
            return Ok(false);
        }
        Ok(true)
    }

    /// Resolves the deepen request (deepen-not names refs or commit ids).
    async fn finish(&mut self, view: &mut GitView<'_>, refs: &RefList) -> GitResult<()> {
        let format = view.format();
        self.args.deepen = if !self.deepen_not.is_empty() {
            let mut ids = Vec::new();
            for name in &self.deepen_not {
                let target = match refs.refs.iter().find(|r| {
                    r.name == *name
                        || r.name == format!("refs/heads/{name}")
                        || r.name == format!("refs/tags/{name}")
                }) {
                    Some(r) => Some(r.target),
                    None => match format.parse_hex(name.as_bytes()) {
                        Ok(o) => view.bee_of(&o).await?,
                        Err(_) => None,
                    },
                };
                match target {
                    Some(t) if t.kind() == ObjectKind::Changeset => ids.push(t),
                    _ => {
                        return Err(GitError::invalid(format!(
                            "deepen-not names no ref or commit: {name:?}"
                        )));
                    }
                }
            }
            Some(Deepen::Not(ids))
        } else if let Some(t) = self.since {
            Some(Deepen::Since(t))
        } else if let Some(n) = self.depth {
            Some(if self.relative {
                Deepen::Relative(n)
            } else {
                Deepen::Depth(n)
            })
        } else {
            None
        };
        Ok(())
    }
}

/// The pack cache key of a fetch on `session` with `refs` advertised.
fn cache_key(
    session: &dyn RepoSession,
    refs: &RefList,
    args: &FetchArgs,
    common: &[GitOid],
) -> [u8; 32] {
    let tag_ids: Vec<GitOid> = refs.tags.iter().map(|t| t.oid).collect();
    key(
        &format!("{}#{}", session.full_name(), session.content_generation()),
        args,
        common,
        &tag_ids,
    )
}

/// The fetch a stock `git clone` (no options) makes of a repo advertising `refs`, as this
/// gateway parses it over protocol v2 and v0/v1 alike: a want for every advertised branch and
/// tag (the advertised id, so an annotated tag's own id; v2 also repeats `HEAD`'s branch,
/// which the cache key folds), `ofs-delta` and `thin-pack`, no `include-tag` (clone asks for
/// every tag itself), no haves, no shallow or deepen lines and no filter. `no-progress` and
/// the side-band choice do not change the pack, so they are not part of the cache key.
pub fn clone_args(refs: &RefList) -> FetchArgs {
    FetchArgs {
        wants: refs
            .refs
            .iter()
            .filter(|r| r.name.starts_with("refs/heads/") || r.name.starts_with("refs/tags/"))
            .map(|r| r.oid)
            .collect(),
        ofs_delta: true,
        thin_pack: true,
        include_tag: false,
        ..FetchArgs::default()
    }
}

/// Builds the pack a stock `git clone` of `refs` asks for ([`clone_args`]) and stores it in
/// `cache` under the key that clone's request produces on a session with the same repo,
/// content generation and refs, so the clone is served from memory. True when a pack was
/// built and kept; false when the cache is disabled, there is nothing to clone, the pack is
/// already cached, or it is too large for the cache (more than half its capacity).
pub async fn prewarm_clone(
    session: &dyn RepoSession,
    view: &mut GitView<'_>,
    refs: &RefList,
    cache: &PackCache,
) -> GitResult<bool> {
    let args = clone_args(refs);
    if cache.capacity() == 0 || args.wants.is_empty() {
        return Ok(false);
    }
    let k = cache_key(session, refs, &args, &[]);
    if cache.contains(&k) {
        return Ok(false);
    }
    let p = Arc::new(plan(view, &args, &refs.tags).await?);
    view.flush().await?;
    cache.put(k, p);
    Ok(cache.contains(&k))
}

async fn planned(
    session: &dyn RepoSession,
    view: &mut GitView<'_>,
    refs: &RefList,
    cache: &CacheUse<'_>,
    args: &FetchArgs,
    common: &[GitOid],
) -> GitResult<Arc<FetchPlan>> {
    let k = cache_key(session, refs, args, common);
    if let Some(p) = cache.get(&k) {
        return Ok(p);
    }
    let p = Arc::new(plan(view, args, &refs.tags).await?);
    cache.put(k, p.clone());
    Ok(p)
}

/// The haves the repo knows, without planning a pack.
async fn common_haves(view: &mut GitView<'_>, haves: &[GitOid]) -> GitResult<Vec<GitOid>> {
    let mut out = Vec::new();
    for h in haves {
        if let Some(id) = view.bee_of(h).await?
            && id.kind() == ObjectKind::Changeset
            && view.history().has_changeset(&id).await?
            && !out.contains(h)
        {
            out.push(*h);
        }
    }
    Ok(out)
}

fn progress(out: &mut Vec<u8>, quiet: bool, plan: &FetchPlan) {
    if !quiet {
        pktline::sideband(
            out,
            BAND_PROGRESS,
            format!("bee: sending {} objects\n", plan.objects).as_bytes(),
        );
    }
}

async fn fetch_v2(
    session: &dyn RepoSession,
    view: &mut GitView<'_>,
    refs: &RefList,
    cache: &CacheUse<'_>,
    args: &[&[u8]],
) -> GitResult<Vec<u8>> {
    let format = view.format();
    let mut p = Parsed::default();
    for a in args {
        let line = utf8(a)?;
        if let Some(w) = line.strip_prefix("want ") {
            p.args.wants.push(oid(format, w.trim())?);
            continue;
        }
        if p.common(format, line)? {
            continue;
        }
        match line {
            "wait-for-done" | "sideband-all" => {}
            "thin-pack" => p.args.thin_pack = true,
            "ofs-delta" => p.args.ofs_delta = true,
            "no-progress" => p.no_progress = true,
            "include-tag" => p.args.include_tag = true,
            _ if line.starts_with("packfile-uris ") => {}
            _ => {
                return Err(GitError::invalid(format!(
                    "unexpected fetch argument {line:?}"
                )));
            }
        }
    }
    if p.args.wants.is_empty() {
        return Err(GitError::invalid("fetch without any want"));
    }
    p.finish(view, refs).await?;
    let mut out = Vec::new();
    let common = common_haves(view, &p.args.haves).await?;
    if !p.done {
        pktline::text(&mut out, "acknowledgments\n");
        if common.is_empty() {
            pktline::text(&mut out, "NAK\n");
            pktline::flush(&mut out);
            return Ok(out);
        }
        for c in &common {
            pktline::text(&mut out, &format!("ACK {c}\n"));
        }
        pktline::text(&mut out, "ready\n");
        pktline::delim(&mut out);
    }
    let plan = planned(session, view, refs, cache, &p.args, &common).await?;
    if p.args.deepen.is_some() {
        pktline::text(&mut out, "shallow-info\n");
        for s in &plan.shallow {
            pktline::text(&mut out, &format!("shallow {s}\n"));
        }
        for s in &plan.unshallow {
            pktline::text(&mut out, &format!("unshallow {s}\n"));
        }
        pktline::delim(&mut out);
    }
    pktline::text(&mut out, "packfile\n");
    progress(&mut out, p.no_progress, &plan);
    pktline::sideband(&mut out, BAND_DATA, &plan.pack);
    pktline::flush(&mut out);
    Ok(out)
}

/// Serves one v0/v1 stateless-RPC upload-pack request.
pub async fn serve_v0(
    session: &dyn RepoSession,
    view: &mut GitView<'_>,
    refs: &RefList,
    cache: &CacheUse<'_>,
    body: &[u8],
) -> GitResult<Vec<u8>> {
    let format = view.format();
    let mut p = Parsed::default();
    let mut r = Reader::new(body);
    let mut first = true;
    // Wants, shallow and deepen lines up to the first flush.
    loop {
        match r.next_pkt()? {
            Some(Pkt::Data(d)) => {
                let line = utf8(d.strip_suffix(b"\n").unwrap_or(d))?;
                if let Some(rest) = line.strip_prefix("want ") {
                    let mut parts = rest.split(' ');
                    let hex = parts.next().unwrap_or_default();
                    p.args.wants.push(oid(format, hex)?);
                    if first {
                        for cap in parts {
                            match cap {
                                "side-band-64k" | "side-band" => p.sideband = true,
                                "ofs-delta" => p.args.ofs_delta = true,
                                "thin-pack" => p.args.thin_pack = true,
                                "no-progress" => p.no_progress = true,
                                "include-tag" => p.args.include_tag = true,
                                "deepen-relative" => p.relative = true,
                                _ => {}
                            }
                        }
                        first = false;
                    }
                } else if !p.common(format, line)? {
                    return Err(GitError::invalid(format!(
                        "unexpected upload-pack line {line:?}"
                    )));
                }
            }
            Some(Pkt::Flush) | None => break,
            Some(_) => {
                return Err(GitError::invalid(
                    "unexpected packet in upload-pack request",
                ));
            }
        }
    }
    // Haves, then `done` or the end of the request.
    loop {
        match r.next_pkt()? {
            Some(Pkt::Data(d)) => {
                let line = utf8(d.strip_suffix(b"\n").unwrap_or(d))?;
                if !p.common(format, line)? {
                    return Err(GitError::invalid(format!(
                        "unexpected upload-pack line {line:?}"
                    )));
                }
                if p.done {
                    break;
                }
            }
            Some(Pkt::Flush) => {}
            None => break,
            Some(_) => {
                return Err(GitError::invalid(
                    "unexpected packet in upload-pack request",
                ));
            }
        }
    }
    if p.args.wants.is_empty() {
        return Ok(Vec::new());
    }
    p.finish(view, refs).await?;
    let mut out = Vec::new();
    let common = common_haves(view, &p.args.haves).await?;
    let needs_plan = p.done || p.args.deepen.is_some();
    let plan = if needs_plan {
        Some(planned(session, view, refs, cache, &p.args, &common).await?)
    } else {
        None
    };
    if let (Some(plan), Some(_)) = (&plan, &p.args.deepen) {
        for s in &plan.shallow {
            pktline::text(&mut out, &format!("shallow {s}\n"));
        }
        for s in &plan.unshallow {
            pktline::text(&mut out, &format!("unshallow {s}\n"));
        }
        pktline::flush(&mut out);
    }
    for c in &common {
        pktline::text(&mut out, &format!("ACK {c} common\n"));
    }
    if !p.done {
        if let Some(last) = common.last() {
            pktline::text(&mut out, &format!("ACK {last} ready\n"));
        }
        if !p.args.haves.is_empty() {
            pktline::text(&mut out, "NAK\n");
        }
        return Ok(out);
    }
    match common.last() {
        Some(last) => pktline::text(&mut out, &format!("ACK {last}\n")),
        None => pktline::text(&mut out, "NAK\n"),
    }
    let Some(plan) = plan else {
        return Err(GitError::internal("fetch planned no pack"));
    };
    if p.sideband {
        progress(&mut out, p.no_progress, &plan);
        pktline::sideband(&mut out, BAND_DATA, &plan.pack);
        pktline::flush(&mut out);
    } else {
        out.extend_from_slice(&plan.pack);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v2_command_parsing() {
        let mut body = Vec::new();
        pktline::text(&mut body, "command=ls-refs\n");
        pktline::text(&mut body, "agent=git/2.43\n");
        pktline::delim(&mut body);
        pktline::text(&mut body, "peel\n");
        pktline::text(&mut body, "ref-prefix refs/heads/\n");
        pktline::flush(&mut body);
        let c = parse_command(&body).unwrap();
        assert_eq!(c.name, "ls-refs");
        assert_eq!(c.args, vec![&b"peel"[..], &b"ref-prefix refs/heads/"[..]]);
        let mut body = Vec::new();
        pktline::text(&mut body, "agent=git/2.43\n");
        pktline::flush(&mut body);
        assert!(parse_command(&body).is_err());
    }

    #[test]
    fn v2_advertisement_shape() {
        let a = advertise_v2(ObjectFormat::Sha1);
        let text = String::from_utf8_lossy(&a);
        assert!(text.starts_with("000eversion 2\n"));
        assert!(text.contains("fetch=shallow filter\n"));
        assert!(text.contains("object-info\n"));
        assert!(text.contains("object-format=sha1\n"));
        assert!(text.ends_with("0000"));
    }
}
