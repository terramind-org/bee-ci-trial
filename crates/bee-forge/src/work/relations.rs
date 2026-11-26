//! Sub-issues, dependencies and the development panel (links to change requests, branches).

use std::collections::BTreeSet;

use bee_proto::v1::{
    Issue, IssueDependencyDirection, IssueDependencySummary, IssueDevelopment, IssueLinkSource,
    IssueLinkedBranch, IssueLinkedChange, IssueSubIssueProgress, IssueSummary,
    IssueTimelineEventKind as Tk, Principal, SubjectKind,
};

use super::issues::{
    Cache, IssueTarget, add_timeline, is_open, load_issue, load_issue_id, load_target, render,
    save_issue, summary,
};
use super::model::{IssueRec, LinkRec};
use super::*;
use crate::ids::ts;

const GAP: i64 = 1000;

/// A change request to link, as the service or listener resolved it.
#[derive(Clone, Debug, Default)]
pub struct ChangeLink {
    /// Change request id.
    pub id: String,
    /// Repo the change request lives in.
    pub repo_id: String,
    /// Its number.
    pub number: u64,
    /// Title (may be empty before the first event arrives).
    pub title: String,
    /// `open`, `landed`, `closed` or `draft`.
    pub state: String,
}

/// Direct counts and the percentage over all descendants.
pub(crate) async fn sub_progress(tx: &mut Tx, issue_id: &str) -> Result<IssueSubIssueProgress> {
    let kids = tx
        .scan(
            &SUBS,
            &Scan::scope(issue_id, limits::SUB_ISSUES + 1).by_n1(),
        )
        .await?;
    let (mut total, mut completed) = (0u32, 0u32);
    let mut stack: Vec<String> = Vec::new();
    for k in &kids {
        total += 1;
        stack.push(k.id.clone());
        if let Some((c, _)) = load_issue_id(tx, &k.id, false).await?
            && !is_open(&c)
        {
            completed += 1;
        }
    }
    // Every descendant counts once; the walk is bounded.
    let (mut all, mut done) = (0u32, 0u32);
    let mut seen = BTreeSet::new();
    while let Some(id) = stack.pop() {
        if !seen.insert(id.clone()) || seen.len() > 2000 {
            continue;
        }
        all += 1;
        if let Some((c, _)) = load_issue_id(tx, &id, false).await? {
            if !is_open(&c) {
                done += 1;
            }
            if c.child_count > 0 {
                for g in tx
                    .scan(&SUBS, &Scan::scope(id.clone(), limits::SUB_ISSUES + 1))
                    .await?
                {
                    stack.push(g.id);
                }
            }
        }
    }
    Ok(IssueSubIssueProgress {
        total,
        completed,
        percent_completed: (done * 100).checked_div(all).unwrap_or(0),
    })
}

/// Dependency counts.
pub(crate) async fn dep_summary(tx: &mut Tx, rec: &IssueRec) -> Result<IssueDependencySummary> {
    let mut open = 0;
    if rec.blocked_by_count > 0 {
        for r in dep_rows(tx, &rec.id, "by:").await? {
            if let Some((b, _)) = load_issue_id(tx, &r.id["by:".len()..], false).await?
                && is_open(&b)
            {
                open += 1;
            }
        }
    }
    Ok(IssueDependencySummary {
        blocked_by: rec.blocked_by_count,
        blocked_by_open: open,
        blocking: rec.blocking_count,
    })
}

async fn dep_rows(tx: &mut Tx, issue_id: &str, prefix: &str) -> Result<Vec<Row>> {
    Ok(scan_all(tx, &DEPS, Scan::scope(issue_id, 1))
        .await?
        .into_iter()
        .filter(|r| r.id.starts_with(prefix))
        .collect())
}

fn same_org(a: &IssueRec, b: &IssueRec) -> Result<()> {
    if a.org_id != b.org_id {
        return Err(ForgeError::invalid(
            "different_org",
            "issues relate only within one org",
        ));
    }
    Ok(())
}

/// Number of nodes from `rec` up to the root, `rec` included.
async fn depth_up(tx: &mut Tx, rec: &IssueRec) -> Result<usize> {
    let mut n = 1;
    let mut cur = rec.parent_id.clone();
    while !cur.is_empty() && n <= limits::SUB_DEPTH + 1 {
        n += 1;
        cur = match load_issue_id(tx, &cur, false).await? {
            Some((p, _)) => p.parent_id,
            None => String::new(),
        };
    }
    Ok(n)
}

/// Nodes on the longest downward path from `id`, `id` included (bounded).
async fn height_down(tx: &mut Tx, id: &str) -> Result<usize> {
    let mut best = 1;
    let mut stack = vec![(id.to_string(), 1usize)];
    let mut visits = 0;
    while let Some((cur, d)) = stack.pop() {
        visits += 1;
        best = best.max(d);
        if visits > 2000 || d > limits::SUB_DEPTH + 1 {
            break;
        }
        for k in tx
            .scan(&SUBS, &Scan::scope(cur, limits::SUB_ISSUES + 1))
            .await?
        {
            stack.push((k.id, d + 1));
        }
    }
    Ok(best)
}

async fn positions(tx: &mut Tx, parent_id: &str) -> Result<Vec<(String, i64)>> {
    Ok(tx
        .scan(
            &SUBS,
            &Scan::scope(parent_id, limits::SUB_ISSUES + 1).by_n1(),
        )
        .await?
        .into_iter()
        .map(|r| (r.id, r.n1.unwrap_or(0)))
        .collect())
}

async fn renumber(tx: &mut Tx, parent_id: &str) -> Result<()> {
    for (i, (id, _)) in positions(tx, parent_id).await?.into_iter().enumerate() {
        tx.put(&SUBS, &Row::new(parent_id, id).n1((i as i64 + 1) * GAP))
            .await?;
    }
    Ok(())
}

/// The position for a child placed after `after_id` (empty: at the end) or before `before_id`.
async fn place(
    tx: &mut Tx,
    parent_id: &str,
    after_id: &str,
    before_id: &str,
    moving: &str,
) -> Result<i64> {
    let mut list = positions(tx, parent_id).await?;
    list.retain(|(id, _)| id != moving);
    if list.is_empty() {
        return Ok(GAP);
    }
    let idx = if !after_id.is_empty() {
        let i = list
            .iter()
            .position(|(id, _)| id == after_id)
            .ok_or_else(|| {
                ForgeError::invalid(
                    "unknown_sibling",
                    "`after_id` is not a sub-issue of this parent",
                )
            })?;
        Some(i + 1)
    } else if !before_id.is_empty() {
        Some(
            list.iter()
                .position(|(id, _)| id == before_id)
                .ok_or_else(|| {
                    ForgeError::invalid(
                        "unknown_sibling",
                        "`before_id` is not a sub-issue of this parent",
                    )
                })?,
        )
    } else {
        None
    };
    let Some(i) = idx else {
        return Ok(list.last().map(|(_, p)| p + GAP).unwrap_or(GAP));
    };
    let lo = if i == 0 { 0 } else { list[i - 1].1 };
    let hi = list.get(i).map(|(_, p)| *p);
    match hi {
        None => Ok(lo + GAP),
        Some(h) if h - lo >= 2 => Ok(lo + (h - lo) / 2),
        Some(_) => {
            renumber(tx, parent_id).await?;
            Box::pin(place(tx, parent_id, after_id, before_id, moving)).await
        }
    }
}

/// Makes `child` a sub-issue of `parent` (all checks included). Events are the caller's.
pub(crate) async fn attach_in_tx(
    tx: &mut Tx,
    _repo: &RepoInfo,
    actor: &Principal,
    parent: &IssueTarget,
    child: &IssueTarget,
    replace_parent: bool,
    after_id: &str,
) -> Result<()> {
    let (mut p, prow) = load_target(tx, parent, true).await?;
    let (mut c, crow) = load_target(tx, child, true).await?;
    same_org(&p, &c)?;
    if p.id == c.id {
        return Err(ForgeError::invalid(
            "cycle",
            "an issue cannot be its own sub-issue",
        ));
    }
    if c.parent_id == p.id {
        return Ok(());
    }
    // A cycle exists when the child is an ancestor of the parent.
    let mut up = p.parent_id.clone();
    let mut guard = 0;
    while !up.is_empty() && guard < 100 {
        if up == c.id {
            return Err(ForgeError::invalid(
                "cycle",
                "that would make an issue its own ancestor",
            ));
        }
        up = match load_issue_id(tx, &up, false).await? {
            Some((a, _)) => a.parent_id,
            None => String::new(),
        };
        guard += 1;
    }
    if !c.parent_id.is_empty() {
        if !replace_parent {
            return Err(ForgeError::invalid_state(
                "the issue already has a parent; set `replace_parent` to move it",
            ));
        }
        let old_parent = c.parent_id.clone();
        detach_in_tx(tx, &old_parent, &c.id, actor).await?;
        // The detach rewrote both rows; read the child again.
        let (c2, _) = load_issue_id(tx, &c.id, true)
            .await?
            .ok_or_else(|| ForgeError::not_found("issue"))?;
        c = c2;
        if p.id == old_parent {
            let (p2, _) = load_issue_id(tx, &p.id, true)
                .await?
                .ok_or_else(|| ForgeError::not_found("issue"))?;
            p = p2;
        }
    }
    let crow2 = tx.get(&ISSUES, &c.repo_id, &c.id).await?.unwrap_or(crow);
    let prow2 = tx.get(&ISSUES, &p.repo_id, &p.id).await?.unwrap_or(prow);
    if p.child_count as usize >= limits::SUB_ISSUES {
        return Err(exhausted(
            "sub_issues",
            limits::SUB_ISSUES,
            p.child_count as usize + 1,
        ));
    }
    let depth = depth_up(tx, &p).await? + height_down(tx, &c.id).await?;
    if depth > limits::SUB_DEPTH {
        return Err(exhausted("sub_issue_depth", limits::SUB_DEPTH, depth));
    }
    let pos = place(tx, &p.id, after_id, "", &c.id).await?;
    tx.put(&SUBS, &Row::new(p.id.clone(), c.id.clone()).n1(pos))
        .await?;
    let (old_c, old_p) = (c.clone(), p.clone());
    c.parent_id = p.id.clone();
    p.child_count += 1;
    let mut cache = Cache::bare(&actor.id);
    let ps = summary(tx, &mut cache, &p).await?;
    let cs = summary(tx, &mut cache, &c).await?;
    add_timeline(tx, &c.id, Tk::ParentAdded, actor, |e| {
        e.related_issue = Some(ps)
    })
    .await?;
    add_timeline(tx, &p.id, Tk::SubIssueAdded, actor, |e| {
        e.related_issue = Some(cs)
    })
    .await?;
    save_issue(tx, Some(&old_c), &mut c, crow2.version).await?;
    save_issue(tx, Some(&old_p), &mut p, prow2.version).await?;
    Ok(())
}

/// Detaches a child from its parent (rows, counters, timeline).
pub(crate) async fn detach_in_tx(
    tx: &mut Tx,
    parent_id: &str,
    child_id: &str,
    actor: &Principal,
) -> Result<()> {
    if !tx.delete(&SUBS, parent_id, child_id).await? {
        return Err(ForgeError::not_found("sub-issue relation"));
    }
    if let Some((mut c, row)) = load_issue_id(tx, child_id, true).await? {
        let old = c.clone();
        c.parent_id.clear();
        save_issue(tx, Some(&old), &mut c, row.version).await?;
        let mut cache = Cache::bare(&actor.id);
        if let Some((p, _)) = load_issue_id(tx, parent_id, false).await? {
            let ps = summary(tx, &mut cache, &p).await?;
            add_timeline(tx, child_id, Tk::ParentRemoved, actor, |e| {
                e.related_issue = Some(ps)
            })
            .await?;
        }
        let cs = summary(tx, &mut cache, &c).await?;
        add_timeline(tx, parent_id, Tk::SubIssueRemoved, actor, |e| {
            e.related_issue = Some(cs)
        })
        .await?;
    }
    if let Some((mut p, row)) = load_issue_id(tx, parent_id, true).await? {
        let old = p.clone();
        p.child_count = p.child_count.saturating_sub(1);
        save_issue(tx, Some(&old), &mut p, row.version).await?;
    }
    Ok(())
}

/// Detaches an issue from all relations before it is deleted.
pub(crate) async fn detach_all(tx: &mut Tx, rec: &IssueRec) -> Result<()> {
    let system = Principal {
        id: "system".into(),
        ..Default::default()
    };
    if !rec.parent_id.is_empty() && tx.get(&SUBS, &rec.parent_id, &rec.id).await?.is_some() {
        detach_in_tx(tx, &rec.parent_id, &rec.id, &system).await?;
    }
    for k in scan_all(tx, &SUBS, Scan::scope(rec.id.clone(), 1)).await? {
        // Children become top-level issues; the parent row is going away anyway.
        tx.delete(&SUBS, &rec.id, &k.id).await?;
        if let Some((mut c, row)) = load_issue_id(tx, &k.id, true).await? {
            let old = c.clone();
            c.parent_id.clear();
            save_issue(tx, Some(&old), &mut c, row.version).await?;
        }
    }
    for r in scan_all(tx, &DEPS, Scan::scope(rec.id.clone(), 1)).await? {
        tx.delete(&DEPS, &rec.id, &r.id).await?;
        let (other, mirror, by_side) = if let Some(o) = r.id.strip_prefix("by:") {
            (o.to_string(), format!("ing:{}", rec.id), true)
        } else if let Some(o) = r.id.strip_prefix("ing:") {
            (o.to_string(), format!("by:{}", rec.id), false)
        } else {
            continue;
        };
        tx.delete(&DEPS, &other, &mirror).await?;
        if let Some((mut o, row)) = load_issue_id(tx, &other, true).await? {
            let old = o.clone();
            if by_side {
                o.blocking_count = o.blocking_count.saturating_sub(1);
            } else {
                o.blocked_by_count = o.blocked_by_count.saturating_sub(1);
            }
            save_issue(tx, Some(&old), &mut o, row.version).await?;
        }
    }
    Ok(())
}

fn rel_change<'a>(
    kind: &'a str,
    action: &'a str,
    repo: &'a RepoInfo,
    rec: &'a IssueRec,
    payload: String,
) -> Change<'a> {
    Change {
        kind,
        action,
        org_id: &repo.org_id,
        repo_id: &repo.id,
        subject_kind: SubjectKind::Issue,
        subject_id: &rec.id,
        payload,
        after: serde_json::json!({"number": rec.number}).to_string(),
    }
}

async fn paged_summaries(
    tx: &mut Tx,
    repo: &RepoInfo,
    ids: Vec<String>,
    viewer: &str,
) -> Result<Vec<IssueSummary>> {
    let mut cache = Cache::new(repo, viewer);
    let mut out = Vec::new();
    for id in ids {
        if let Some((r, _)) = load_issue_id(tx, &id, false).await? {
            out.push(summary(tx, &mut cache, &r).await?);
        }
    }
    Ok(out)
}

impl Work {
    // ---- sub-issues ----

    /// Makes `child` a sub-issue of issue `number`.
    #[allow(clippy::too_many_arguments)]
    pub async fn add_sub_issue(
        &self,
        cx: &Cx,
        repo: &RepoInfo,
        actor: &Actor,
        number: u64,
        child: IssueTarget,
        replace_parent: bool,
        after_id: &str,
    ) -> Result<Issue> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let parent = IssueTarget::Number {
            repo_id: repo.id.clone(),
            number,
        };
        attach_in_tx(
            &mut tx,
            repo,
            &actor.principal,
            &parent,
            &child,
            replace_parent,
            after_id,
        )
        .await?;
        let (rec, row) = load_issue(&mut tx, &repo.id, number, false).await?;
        let mut cache = Cache::new(repo, &cx.actor_id);
        let issue = render(&mut tx, &mut cache, &rec, row.version).await?;
        record(
            &mut tx,
            rel_change(
                "issue.sub_issue_changed",
                "issue.add_sub_issue",
                repo,
                &rec,
                pjson!(&issue),
            ),
        )
        .await?;
        tx.commit().await?;
        Ok(issue)
    }

    /// Detaches a sub-issue from its parent.
    pub async fn remove_sub_issue(
        &self,
        cx: &Cx,
        repo: &RepoInfo,
        actor: &Actor,
        number: u64,
        child: IssueTarget,
    ) -> Result<Issue> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (p, _) = load_issue(&mut tx, &repo.id, number, true).await?;
        let (c, _) = load_target(&mut tx, &child, true).await?;
        if c.parent_id != p.id {
            return Err(ForgeError::not_found("sub-issue relation"));
        }
        detach_in_tx(&mut tx, &p.id, &c.id, &actor.principal).await?;
        let (rec, row) = load_issue(&mut tx, &repo.id, number, false).await?;
        let mut cache = Cache::new(repo, &cx.actor_id);
        let issue = render(&mut tx, &mut cache, &rec, row.version).await?;
        record(
            &mut tx,
            rel_change(
                "issue.sub_issue_changed",
                "issue.remove_sub_issue",
                repo,
                &rec,
                pjson!(&issue),
            ),
        )
        .await?;
        tx.commit().await?;
        Ok(issue)
    }

    /// Moves a sub-issue within its parent's list.
    pub async fn reorder_sub_issue(
        &self,
        cx: &Cx,
        repo: &RepoInfo,
        number: u64,
        child: IssueTarget,
        after_id: &str,
        before_id: &str,
    ) -> Result<Issue> {
        if after_id.is_empty() == before_id.is_empty() {
            return Err(ForgeError::invalid(
                "position_required",
                "set exactly one of `after_id` and `before_id`",
            ));
        }
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (p, _) = load_issue(&mut tx, &repo.id, number, true).await?;
        let (c, _) = load_target(&mut tx, &child, false).await?;
        if c.parent_id != p.id {
            return Err(ForgeError::not_found("sub-issue relation"));
        }
        let pos = place(&mut tx, &p.id, after_id, before_id, &c.id).await?;
        tx.put(&SUBS, &Row::new(p.id.clone(), c.id.clone()).n1(pos))
            .await?;
        let (rec, row) = load_issue(&mut tx, &repo.id, number, false).await?;
        let mut cache = Cache::new(repo, &cx.actor_id);
        let issue = render(&mut tx, &mut cache, &rec, row.version).await?;
        record(
            &mut tx,
            rel_change(
                "issue.sub_issue_changed",
                "issue.reorder_sub_issue",
                repo,
                &rec,
                pjson!(&issue),
            ),
        )
        .await?;
        tx.commit().await?;
        Ok(issue)
    }

    /// A parent's sub-issues in order, with progress.
    pub async fn list_sub_issues(
        &self,
        cx: &Cx,
        repo: &RepoInfo,
        number: u64,
        limit: usize,
        after: &str,
    ) -> Result<(Vec<IssueSummary>, IssueSubIssueProgress, String)> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (p, _) = load_issue(&mut tx, &repo.id, number, false).await?;
        let progress = sub_progress(&mut tx, &p.id).await?;
        let all = positions(&mut tx, &p.id).await?;
        let after_pos: Option<(i64, String)> = after
            .split_once(':')
            .and_then(|(a, b)| a.parse().ok().map(|n| (n, b.to_string())));
        let rest: Vec<(String, i64)> = all
            .into_iter()
            .filter(|(id, pos)| {
                after_pos
                    .as_ref()
                    .is_none_or(|(ap, aid)| (*pos, id.as_str()) > (*ap, aid.as_str()))
            })
            .collect();
        let next = if rest.len() > limit {
            format!("{}:{}", rest[limit - 1].1, rest[limit - 1].0)
        } else {
            String::new()
        };
        let ids: Vec<String> = rest.into_iter().take(limit).map(|(id, _)| id).collect();
        let sums = paged_summaries(&mut tx, repo, ids, &cx.actor_id).await?;
        Ok((sums, progress, next))
    }

    /// An issue's parent.
    pub async fn get_parent(
        &self,
        cx: &Cx,
        repo: &RepoInfo,
        number: u64,
    ) -> Result<Option<IssueSummary>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (c, _) = load_issue(&mut tx, &repo.id, number, false).await?;
        if c.parent_id.is_empty() {
            return Ok(None);
        }
        let mut cache = Cache::new(repo, &cx.actor_id);
        match load_issue_id(&mut tx, &c.parent_id, false).await? {
            Some((p, _)) => Ok(Some(summary(&mut tx, &mut cache, &p).await?)),
            None => Ok(None),
        }
    }

    // ---- dependencies ----

    /// Records that issue `number` is blocked by `blocked_by`.
    pub async fn add_dependency(
        &self,
        cx: &Cx,
        repo: &RepoInfo,
        actor: &Actor,
        number: u64,
        blocked_by: IssueTarget,
    ) -> Result<Issue> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut blocked, brow) = load_issue(&mut tx, &repo.id, number, true).await?;
        let (mut blocker, krow) = load_target(&mut tx, &blocked_by, true).await?;
        same_org(&blocked, &blocker)?;
        if blocked.id == blocker.id {
            return Err(ForgeError::invalid("cycle", "an issue cannot block itself"));
        }
        if tx
            .get(&DEPS, &blocked.id, &format!("by:{}", blocker.id))
            .await?
            .is_some()
        {
            return Err(ForgeError::already_exists("dependency"));
        }
        if blocked.blocked_by_count as usize >= limits::DEPENDENCIES {
            return Err(exhausted(
                "blocked_by",
                limits::DEPENDENCIES,
                blocked.blocked_by_count as usize + 1,
            ));
        }
        if blocker.blocking_count as usize >= limits::DEPENDENCIES {
            return Err(exhausted(
                "blocking",
                limits::DEPENDENCIES,
                blocker.blocking_count as usize + 1,
            ));
        }
        // A cycle exists when the blocker is already (transitively) blocked by `blocked`.
        let mut stack = vec![blocker.id.clone()];
        let mut seen = BTreeSet::new();
        while let Some(id) = stack.pop() {
            if id == blocked.id {
                return Err(ForgeError::invalid(
                    "cycle",
                    "that dependency would form a cycle",
                ));
            }
            if !seen.insert(id.clone()) || seen.len() > 5000 {
                continue;
            }
            for r in dep_rows(&mut tx, &id, "by:").await? {
                stack.push(r.id["by:".len()..].to_string());
            }
        }
        let now = tx.now_ms() as i64;
        tx.insert(
            &DEPS,
            &Row::new(blocked.id.clone(), format!("by:{}", blocker.id)).n1(now),
        )
        .await?;
        tx.insert(
            &DEPS,
            &Row::new(blocker.id.clone(), format!("ing:{}", blocked.id)).n1(now),
        )
        .await?;
        let (ob, ok) = (blocked.clone(), blocker.clone());
        blocked.blocked_by_count += 1;
        blocker.blocking_count += 1;
        let mut cache = Cache::bare(&cx.actor_id);
        let bs = summary(&mut tx, &mut cache, &blocker).await?;
        let ds = summary(&mut tx, &mut cache, &blocked).await?;
        add_timeline(
            &mut tx,
            &blocked.id,
            Tk::BlockedByAdded,
            &actor.principal,
            |e| e.related_issue = Some(bs),
        )
        .await?;
        add_timeline(
            &mut tx,
            &blocker.id,
            Tk::BlockingAdded,
            &actor.principal,
            |e| e.related_issue = Some(ds),
        )
        .await?;
        let v = save_issue(&mut tx, Some(&ob), &mut blocked, brow.version).await?;
        save_issue(&mut tx, Some(&ok), &mut blocker, krow.version).await?;
        let mut cache = Cache::new(repo, &cx.actor_id);
        let issue = render(&mut tx, &mut cache, &blocked, v).await?;
        record(
            &mut tx,
            rel_change(
                "issue.dependency_changed",
                "issue.add_dependency",
                repo,
                &blocked,
                pjson!(&issue),
            ),
        )
        .await?;
        tx.commit().await?;
        Ok(issue)
    }

    /// Removes a blocked-by edge.
    pub async fn remove_dependency(
        &self,
        cx: &Cx,
        repo: &RepoInfo,
        actor: &Actor,
        number: u64,
        blocked_by: IssueTarget,
    ) -> Result<Issue> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut blocked, brow) = load_issue(&mut tx, &repo.id, number, true).await?;
        let (mut blocker, krow) = load_target(&mut tx, &blocked_by, true).await?;
        if !tx
            .delete(&DEPS, &blocked.id, &format!("by:{}", blocker.id))
            .await?
        {
            return Err(ForgeError::not_found("dependency"));
        }
        tx.delete(&DEPS, &blocker.id, &format!("ing:{}", blocked.id))
            .await?;
        let (ob, ok) = (blocked.clone(), blocker.clone());
        blocked.blocked_by_count = blocked.blocked_by_count.saturating_sub(1);
        blocker.blocking_count = blocker.blocking_count.saturating_sub(1);
        let mut cache = Cache::bare(&cx.actor_id);
        let bs = summary(&mut tx, &mut cache, &blocker).await?;
        let ds = summary(&mut tx, &mut cache, &blocked).await?;
        add_timeline(
            &mut tx,
            &blocked.id,
            Tk::BlockedByRemoved,
            &actor.principal,
            |e| e.related_issue = Some(bs),
        )
        .await?;
        add_timeline(
            &mut tx,
            &blocker.id,
            Tk::BlockingRemoved,
            &actor.principal,
            |e| e.related_issue = Some(ds),
        )
        .await?;
        let v = save_issue(&mut tx, Some(&ob), &mut blocked, brow.version).await?;
        save_issue(&mut tx, Some(&ok), &mut blocker, krow.version).await?;
        let mut cache = Cache::new(repo, &cx.actor_id);
        let issue = render(&mut tx, &mut cache, &blocked, v).await?;
        record(
            &mut tx,
            rel_change(
                "issue.dependency_changed",
                "issue.remove_dependency",
                repo,
                &blocked,
                pjson!(&issue),
            ),
        )
        .await?;
        tx.commit().await?;
        Ok(issue)
    }

    /// Issues blocking, or blocked by, an issue.
    pub async fn list_dependencies(
        &self,
        cx: &Cx,
        repo: &RepoInfo,
        number: u64,
        direction: IssueDependencyDirection,
        limit: usize,
        after: &str,
    ) -> Result<(Vec<IssueSummary>, String)> {
        let prefix = match direction {
            IssueDependencyDirection::BlockedBy => "by:",
            IssueDependencyDirection::Blocking => "ing:",
            IssueDependencyDirection::Unspecified => {
                return Err(ForgeError::missing("direction"));
            }
        };
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (rec, _) = load_issue(&mut tx, &repo.id, number, false).await?;
        let mut rows = dep_rows(&mut tx, &rec.id, prefix).await?;
        rows.sort_by(|a, b| a.id.cmp(&b.id));
        let rest: Vec<String> = rows
            .into_iter()
            .map(|r| r.id[prefix.len()..].to_string())
            .filter(|id| after.is_empty() || id.as_str() > after)
            .collect();
        let next = if rest.len() > limit {
            rest[limit - 1].clone()
        } else {
            String::new()
        };
        let ids: Vec<String> = rest.into_iter().take(limit).collect();
        let sums = paged_summaries(&mut tx, repo, ids, &cx.actor_id).await?;
        Ok((sums, next))
    }

    // ---- development panel ----

    /// The repo and number of a change request id, if it holds a number.
    pub async fn locate_change(&self, cx: &Cx, cr_id: &str) -> Result<Option<(String, u64)>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let rows = tx
            .scan(
                &NUMBERS,
                &Scan {
                    key: Some(Key::K1(cr_id.to_string())),
                    limit: 4,
                    ..Scan::default()
                },
            )
            .await?;
        for r in rows {
            let e: model::NumberRec = r.decode()?;
            if e.kind() == NumberKind::ChangeRequest {
                return Ok(Some((r.scope.clone(), r.id.parse().unwrap_or(0))));
            }
        }
        Ok(None)
    }

    /// Links a change request to issue `number`.
    #[allow(clippy::too_many_arguments)]
    pub async fn link_change(
        &self,
        cx: &Cx,
        repo: &RepoInfo,
        actor: &Principal,
        number: u64,
        change: ChangeLink,
        closes: bool,
        source: IssueLinkSource,
    ) -> Result<IssueLinkedChange> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (rec, _) = load_issue(&mut tx, &repo.id, number, true).await?;
        let link = link_in_tx(&mut tx, &rec.id, actor, &change, closes, source).await?;
        let (rec, row) = load_issue(&mut tx, &repo.id, number, false).await?;
        let mut cache = Cache::new(repo, &cx.actor_id);
        let issue = render(&mut tx, &mut cache, &rec, row.version).await?;
        record(
            &mut tx,
            rel_change(
                "issue.change_linked",
                "issue.link_change",
                repo,
                &rec,
                serde_json::json!({
                    "issue": serde_json::from_str::<serde_json::Value>(&pjson!(&issue)).unwrap_or_default(),
                    "change_request_id": change.id, "closes": closes, "linked": true,
                })
                .to_string(),
            ),
        )
        .await?;
        tx.commit().await?;
        Ok(link)
    }

    /// Removes a manual link.
    pub async fn unlink_change(
        &self,
        cx: &Cx,
        repo: &RepoInfo,
        actor: &Principal,
        number: u64,
        change_id: &str,
    ) -> Result<()> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (rec, _) = load_issue(&mut tx, &repo.id, number, true).await?;
        let (l, _) = tx
            .get_msg::<LinkRec>(&LINKS, &rec.id, change_id)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("link to {change_id}")))?;
        let manual = l
            .change
            .as_ref()
            .is_some_and(|c| c.source == IssueLinkSource::Manual as i32);
        if !manual {
            return Err(ForgeError::invalid_state(
                "only manual links can be removed here; edit the change request body to drop a closing keyword",
            ));
        }
        unlink_in_tx(&mut tx, &rec.id, actor, change_id).await?;
        record(
            &mut tx,
            rel_change(
                "issue.change_linked",
                "issue.unlink_change",
                repo,
                &rec,
                serde_json::json!({"issue_id": rec.id, "change_request_id": change_id, "linked": false}).to_string(),
            ),
        )
        .await?;
        tx.commit().await
    }

    /// The development panel.
    pub async fn get_development(
        &self,
        cx: &Cx,
        repo: &RepoInfo,
        number: u64,
    ) -> Result<IssueDevelopment> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (rec, _) = load_issue(&mut tx, &repo.id, number, false).await?;
        let mut changes = Vec::new();
        for (l, _) in
            scan_all_msgs::<LinkRec>(&mut tx, &LINKS, Scan::scope(rec.id.clone(), 1)).await?
        {
            if let Some(mut c) = l.change {
                c.repo_full_name = if l.repo_id == repo.id {
                    repo.full_name.clone()
                } else {
                    placeholder(&l.repo_id)
                };
                changes.push(c);
            }
        }
        changes.sort_by(|a, b| {
            a.linked_at
                .as_ref()
                .map(|t| (t.seconds, t.nanos))
                .cmp(&b.linked_at.as_ref().map(|t| (t.seconds, t.nanos)))
                .then(a.change_request_id.cmp(&b.change_request_id))
        });
        let mut branches = Vec::new();
        for (b, _) in
            scan_all_msgs::<IssueLinkedBranch>(&mut tx, &BRANCHES, Scan::scope(rec.id.clone(), 1))
                .await?
        {
            branches.push(b);
        }
        Ok(IssueDevelopment { changes, branches })
    }

    /// Records a branch created for an issue (the VCS side is the caller's).
    #[allow(clippy::too_many_arguments)]
    pub async fn record_branch(
        &self,
        cx: &Cx,
        repo: &RepoInfo,
        actor: &Actor,
        number: u64,
        ref_name: &str,
        base_ref: &str,
        workspace_id: &str,
        branch_repo_name: &str,
    ) -> Result<IssueLinkedBranch> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (rec, _) = load_issue(&mut tx, &repo.id, number, true).await?;
        if tx.get(&BRANCHES, &rec.id, ref_name).await?.is_some() {
            return Err(ForgeError::already_exists(format!("branch {ref_name:?}")));
        }
        let b = IssueLinkedBranch {
            repo_full_name: branch_repo_name.to_string(),
            ref_name: ref_name.to_string(),
            base_ref: base_ref.to_string(),
            workspace_id: workspace_id.to_string(),
            created_by: Some(actor.principal.clone()),
            created_at: Some(ts(tx.now_ms())),
        };
        tx.insert(&BRANCHES, &Row::new(rec.id.clone(), ref_name).msg(&b))
            .await?;
        let r = ref_name.to_string();
        add_timeline(&mut tx, &rec.id, Tk::BranchCreated, &actor.principal, |e| {
            e.ref_name = r
        })
        .await?;
        record(
            &mut tx,
            rel_change(
                "issue.branch_created",
                "issue.create_branch",
                repo,
                &rec,
                serde_json::json!({"issue_id": rec.id, "number": rec.number, "ref": ref_name, "base_ref": base_ref, "workspace_id": workspace_id}).to_string(),
            ),
        )
        .await?;
        tx.commit().await?;
        Ok(b)
    }
}

/// A branch name derived from the number and title: `issue-12-fix-the-login-bug`.
pub fn default_branch_name(number: u64, title: &str) -> String {
    let mut slug = String::new();
    for ch in title.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
        } else if !slug.ends_with('-') && !slug.is_empty() {
            slug.push('-');
        }
        if slug.len() >= 40 {
            break;
        }
    }
    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        format!("issue-{number}")
    } else {
        format!("issue-{number}-{slug}")
    }
}

/// Upserts a link inside a transaction.
pub(crate) async fn link_in_tx(
    tx: &mut Tx,
    issue_id: &str,
    actor: &Principal,
    change: &ChangeLink,
    closes: bool,
    source: IssueLinkSource,
) -> Result<IssueLinkedChange> {
    let existing = tx.get_msg::<LinkRec>(&LINKS, issue_id, &change.id).await?;
    let linked = IssueLinkedChange {
        change_request_id: change.id.clone(),
        repo_full_name: placeholder(&change.repo_id),
        number: change.number,
        title: change.title.clone(),
        state: if change.state.is_empty() {
            "open".into()
        } else {
            change.state.clone()
        },
        closes,
        source: match &existing {
            // A manual link stays manual; a keyword cannot downgrade it.
            Some((l, _))
                if l.change
                    .as_ref()
                    .is_some_and(|c| c.source == IssueLinkSource::Manual as i32) =>
            {
                IssueLinkSource::Manual as i32
            }
            _ => source as i32,
        },
        linked_by: Some(actor.clone()),
        linked_at: Some(ts(tx.now_ms())),
    };
    match existing {
        Some((old, row)) => {
            let mut keep = linked.clone();
            if let Some(o) = &old.change {
                keep.linked_by = o.linked_by.clone();
                keep.linked_at = o.linked_at;
            }
            let rec = LinkRec {
                change: Some(keep.clone()),
                repo_id: change.repo_id.clone(),
            };
            tx.update(
                &LINKS,
                &Row::new(issue_id, change.id.clone())
                    .k1(change.id.clone())
                    .version(row.version)
                    .msg(&rec),
            )
            .await?;
            Ok(keep)
        }
        None => {
            let rec = LinkRec {
                change: Some(linked.clone()),
                repo_id: change.repo_id.clone(),
            };
            tx.insert(
                &LINKS,
                &Row::new(issue_id, change.id.clone())
                    .k1(change.id.clone())
                    .msg(&rec),
            )
            .await?;
            if let Some((mut i, row)) = load_issue_id(tx, issue_id, true).await? {
                let old = i.clone();
                if !i.linked_change_ids.contains(&change.id) {
                    i.linked_change_ids.push(change.id.clone());
                }
                save_issue(tx, Some(&old), &mut i, row.version).await?;
            }
            let cid = change.id.clone();
            add_timeline(tx, issue_id, Tk::ChangeLinked, actor, |e| {
                e.change_request_id = cid
            })
            .await?;
            Ok(linked)
        }
    }
}

/// Removes a link inside a transaction.
pub(crate) async fn unlink_in_tx(
    tx: &mut Tx,
    issue_id: &str,
    actor: &Principal,
    change_id: &str,
) -> Result<()> {
    if !tx.delete(&LINKS, issue_id, change_id).await? {
        return Ok(());
    }
    if let Some((mut i, row)) = load_issue_id(tx, issue_id, true).await? {
        let old = i.clone();
        i.linked_change_ids.retain(|c| c != change_id);
        save_issue(tx, Some(&old), &mut i, row.version).await?;
    }
    let cid = change_id.to_string();
    add_timeline(tx, issue_id, Tk::ChangeUnlinked, actor, |e| {
        e.change_request_id = cid
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::default_branch_name;

    #[test]
    fn branch_names_are_slugs() {
        assert_eq!(
            default_branch_name(12, "Fix the login bug!"),
            "issue-12-fix-the-login-bug"
        );
        assert_eq!(default_branch_name(3, "???"), "issue-3");
    }
}
