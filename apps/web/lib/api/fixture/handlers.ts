/**
 * bee.v1 handlers for the fixture: orgs, repos, change requests, checks, leases, events,
 * the VCS queries, auth and import. Requests and responses are the generated proto3-JSON types;
 * `router.ts` turns them into real messages. Areas composed by the UI live in `views.ts`.
 */
import { blameOrigins, diffText } from "../../diff";
import { classifyChange } from "../../diffclass";
import type { CommentJson } from "../../gen/bee/v1/content_pb";
import { AuthService } from "../../gen/bee/v1/auth_pb";
import { ChangeService, CheckService, EventService, LeaseService, OrgService, RepoService, type OrgJson } from "../../gen/bee/v1/forge_pb";
import { ImportService } from "../../gen/bee/v1/import_pb";
import { ObjectService, QueryService, RefService, WriteService } from "../../gen/bee/v1/vcs_pb";
import { bytesToBase64, objectIdText, pathBytes, pathText } from "../ids";
import { BeeError } from "../transport";
import type * as T from "../types";
import { eventJson, queueEntry, reviewComment, reviewThread } from "./convert";
import { defineService, type Ctx, type ServiceDef } from "./router";
import { digestHex, objectId, ulid } from "./seed";
import { IMPORT_CATEGORIES, type Commit, type FixtureStore, type OrgRecord } from "./store";
import { invalid, overlaps, page } from "./util";
import { VIEW_SERVICES } from "./views";
import { ADMIN_SERVICES } from "./admin";
import { COMMUNITY_SERVICES, createOrg, createRepo } from "./community";
import { forkRepo, isAncestor, listForks, syncFork } from "./forks";
import { BRIEF_SERVICES, listConversation } from "./briefs";
import { POST_SERVICES } from "./posts";
import { safetyService } from "./safety";
import { swarmService } from "./swarms";
import { HUB_SERVICES } from "./hub";
import { repoSettingsImpl } from "./settings";
import { createOrgInvitation, setMember } from "./org-members";

function changesetView(s: FixtureStore, c: Commit, includeMeta: boolean): T.Changeset {
  const author = s.p(c.author);
  return {
    id: c.id,
    parents: c.parent ? [c.parent.id] : [],
    root: objectId("tree", c.key),
    metaId: objectId("meta", c.key),
    changeId: c.changeId,
    delta: c.changed.map((path) => {
      const content = c.files.get(path);
      return content === undefined
        ? { path: pathBytes(path), delete: true }
        : { path: pathBytes(path), put: { mode: "MODE_FILE", blob: objectId("blob", content), size: String(new TextEncoder().encode(content).length) } };
    }),
    meta: includeMeta
      ? {
          author: author.display,
          authorTime: { name: author.display, time: s.iso(c.time), tzOffsetMinutes: 0 },
          committer: author.display,
          committerTime: { name: author.display, time: s.iso(c.time), tzOffsetMinutes: 0 },
          creator: author.id,
          message: c.message,
          provenance: c.provenance,
        }
      : undefined,
    filtered: false,
    generation: String(c.generation),
  };
}

function diffItems(s: FixtureStore, from: Commit, to: Commit, paths: string[] | undefined, includeHunks: boolean, context = 3): T.DiffItem[] {
  const all = [...new Set([...from.files.keys(), ...to.files.keys()])].sort();
  const files = all
    .filter((p) => !paths?.length || paths.some((q) => overlaps(p, q)))
    .map((p) => ({ path: p, oldText: from.files.get(p) ?? null, newText: to.files.get(p) ?? null }))
    .filter((f) => f.oldText !== f.newText);
  const cls = classifyChange(files);
  return files.map((f) => ({
    path: pathBytes(f.path),
    old: f.oldText === null ? undefined : { mode: "MODE_FILE", blob: objectId("blob", f.oldText) },
    new: f.newText === null ? undefined : { mode: "MODE_FILE", blob: objectId("blob", f.newText) },
    binary: false,
    hunks: includeHunks ? diffText(f.oldText ?? "", f.newText ?? "", context) : [],
    diffClass: cls.perPath[f.path] ?? "logic",
  }));
}

function threadOf(s: FixtureStore, repo: ReturnType<FixtureStore["repo"]>, threadId: string) {
  const c = s.crs.find((x) => x.repo === repo.name && x.threads.some((t) => t.id === threadId));
  const t = c?.threads.find((x) => x.id === threadId);
  if (!c || !t) throw new BeeError("not_found", { reason: "THREAD_NOT_FOUND", message: "no review thread " + threadId });
  return { c, t };
}

function resolveThread(s: FixtureStore, req: { repo?: T.RepoRef; threadId?: string }, ctx: Ctx, resolved: boolean) {
  const actor = s.caller(ctx.token);
  const repo = s.repo(req.repo);
  const { c, t } = threadOf(s, repo, req.threadId ?? "");
  t.resolved = resolved;
  s.emit(repo, resolved ? "thread.resolved" : "thread.reopened", actor, { note: { title: c.title, target: "cr:" + c.id } });
  return reviewThread(t);
}

function orgJson(s: FixtureStore, rec: OrgRecord): OrgJson {
  return { ...rec.org, description: rec.description, website: rec.website, location: rec.location, email: rec.email };
}

const org = defineService(OrgService, {
  getOrg: (s, req) => orgJson(s, s.orgRecord(req.nameOrId)),
  listOrgs: (s, req, ctx) => {
    const who = req.principalId ? s.handleOf(req.principalId) : s.caller(ctx.token);
    const orgs = [...s.orgs.values()].filter((o) => s.inOrg(who, o.org.name)).map((o) => orgJson(s, o));
    const pg = page(orgs, req.page, 100);
    return { orgs: pg.items, page: { nextPageToken: pg.next } };
  },
  listMembers: (s, req) => {
    const rec = s.orgRecord(req.org || s.org.name);
    const members = [...rec.members.entries()].map(([h, role]) => ({ principal: s.p(h), role, joinedAt: rec.org.createdAt }));
    const pg = page(members, req.page, 100);
    return { members: pg.items, page: { nextPageToken: pg.next } };
  },
  createOrg,
  createOrgInvitation,
  setMember,
});

const repoSvc = defineService(RepoService, {
  ...repoSettingsImpl,
  getRepo: (s, req) => s.repoView(s.repo(req.repo)),
  listRepos: (s, req, ctx) => {
    const viewer = s.viewer(ctx.token);
    const repos = req.org ? s.reposFor(undefined, req.org, viewer) : s.readableRepos(viewer);
    const pg = page(repos.map((r) => s.repoView(r)), req.page, 100);
    return { repos: pg.items, page: { nextPageToken: pg.next } };
  },
  getContext: (s, req) => s.context(s.repo(req.repo), (req.paths ?? []).map(pathText)),
  createRepo,
  forkRepo,
  listForks,
  syncFork,
});

const change = defineService(ChangeService, {
  openChange: (s, req, ctx) => {
    const actor = s.caller(ctx.token);
    const repo = s.repo(req.repo);
    const title = req.title ?? "";
    if (!title.trim()) throw invalid("a change request needs a title");
    // The web UI opens policy edits as change requests; heads come from the editor.
    const base = s.tip(repo);
    const head = req.head ? s.commits.get(objectIdText(req.head as T.ObjectId)) : undefined;
    if (req.head && !head) throw new BeeError("not_found", { reason: "MISSING_OBJECT", message: "head changeset is not in this repo" });
    const target = head ?? base;
    const id = ulid(s.now(), "cr:open:" + title + ":" + s.crs.length);
    const key = "opened-" + s.crs.length;
    let inspiredBy: string | undefined;
    if (req.inspiredByPostId) {
      const source = s.posts.byId(req.inspiredByPostId);
      if (!source || source.state !== "POST_STATE_PUBLISHED" || !s.posts.visible(s, source, actor)) throw invalid("the post is not published or you cannot read it", "post_not_readable");
      inspiredBy = source.id;
    }
    const cr = {
      id,
      key,
      number: s.nextNumber(repo.name),
      repo: repo.name,
      author: actor,
      title,
      body: req.body ?? "",
      createdAt: s.now(),
      state: "CHANGE_STATE_OPEN" as T.ChangeState,
      dependsOn: req.dependsOn ?? "",
      changeId: target.changeId,
      revisions: [s.makeRev(1, target, target.parent ?? base, s.now(), true)],
      reviews: [],
      threads: [],
      ws: "ws/" + actor + "/" + key,
      inspiredByPostId: inspiredBy,
    };
    s.crs.push(cr);
    if (req.brief) s.briefs.setBrief(s, repo, cr, actor, req.brief as T.ChangeBrief, false);
    const checks = ["test", "lint", "e2e"].map((name) => s.makeCheck(repo, target, name, "CHECK_STATUS_RUNNING", s.now()));
    s.checks.set(target.idText, checks);
    s.pendingChecks.push(target.idText);
    s.emit(repo, "cr.opened", actor, { change: s.crView(cr) });
    s.appendAudit(actor, "change.open", title, repo.name, true, "");
    if (inspiredBy) {
      s.posts.adoptions.push({ post: inspiredBy, repo: repo.name, kind: "change", ref: cr.id, title, by: actor, at: s.now() });
      s.posts.adopted(s, inspiredBy, actor);
    }
    // Agents write a post when they open the change; a person adds one from the page.
    if (req.postText?.trim()) {
      s.posts.draft(s, cr, repo, actor, req.postText);
    }
    return s.crView(cr, actor);
  },
  getChange: (s, req, ctx) => s.crView(s.cr(s.repo(req.repo), req.id ?? ""), s.viewer(ctx.token)),
  getChangeByNumber: (s, req, ctx) => {
    const repo = s.repo(req.repo);
    const c = s.crs.find((x) => x.repo === repo.name && String(x.number) === req.number);
    if (!c) throw new BeeError("not_found", { reason: "CHANGE_NOT_FOUND", message: "no change request #" + req.number + " in " + repo.repo.fullName });
    return s.crView(c, s.viewer(ctx.token));
  },
  listChanges: (s, req, ctx) => {
    const repo = s.repo(req.repo);
    const viewer = s.viewer(ctx.token);
    const all = s.crs
      .filter((c) => c.repo === repo.name)
      .filter((c) => !req.state || req.state === "CHANGE_STATE_UNSPECIFIED" || c.state === req.state)
      .filter((c) => !req.authorId || s.p(c.author).id === req.authorId)
      .sort((a, b) => Math.max(...b.revisions.map((r) => r.createdAt)) - Math.max(...a.revisions.map((r) => r.createdAt)));
    const pg = page(all, req.page);
    return { changes: pg.items.map((c) => s.crView(c, viewer)), page: { nextPageToken: pg.next } };
  },
  closeChange: (s, req, ctx) => {
    const actor = s.caller(ctx.token);
    const repo = s.repo(req.repo);
    const c = s.cr(repo, req.id ?? "");
    if (c.state !== "CHANGE_STATE_OPEN") throw new BeeError("failed_precondition", { reason: "NOT_OPEN", message: "only open change requests can be closed" });
    c.state = "CHANGE_STATE_CLOSED";
    s.posts.onClosed(s, c.id);
    s.emit(repo, "cr.closed", actor, { change: s.crView(c) });
    s.appendAudit(actor, "change.close", c.title + (req.reason ? " (" + req.reason + ")" : ""), repo.name, true, "");
    return s.crView(c);
  },
  submitReview: (s, req, ctx) => {
    const actor = s.caller(ctx.token);
    const repo = s.repo(req.repo);
    const c = s.cr(repo, req.changeRequestId ?? "");
    const cur = s.current(c);
    const revision = req.revision ?? 0;
    const verdict = req.verdict ?? "VERDICT_UNSPECIFIED";
    if (revision !== cur.n) throw new BeeError("failed_precondition", { reason: "STALE_REVISION", message: "review targets r" + revision + " but the change is at r" + cur.n, retryable: true });
    if (verdict === "VERDICT_UNSPECIFIED") throw invalid("pick a verdict");
    if (actor === c.author && verdict === "VERDICT_APPROVE") throw new BeeError("permission_denied", { reason: "SELF_APPROVAL", message: "authors cannot approve their own change" });
    const review: T.Review = {
      id: ulid(s.now(), "review:" + c.id + ":" + c.reviews.length),
      changeId: c.id,
      revision,
      reviewer: s.p(actor),
      verdict,
      body: req.body ?? "",
      comments: (req.comments ?? []).map((cm) => ({
        id: cm.id ?? "",
        path: cm.path ?? "",
        side: cm.side === "old" ? "old" : cm.side === "new" ? "new" : "",
        startLine: cm.startLine ?? 0,
        endLine: cm.endLine ?? 0,
        body: cm.body ?? "",
        revision: cm.revision ?? 0,
      })),
      createdAt: s.iso(s.now()),
    };
    c.reviews.push(review);
    s.emit(repo, "review.submitted", actor, { review });
    s.appendAudit(actor, "review.submit", c.title + " (" + verdict.replace("VERDICT_", "").toLowerCase() + " r" + revision + ")", repo.name, true, "");
    return review;
  },
  listReviews: (s, req) => ({ reviews: s.cr(s.repo(req.repo), req.changeRequestId ?? "").reviews }),
  evaluateLanding: (s, req) => s.evaluate(s.cr(s.repo(req.repo), req.changeRequestId ?? "")),
  diffChangeRevisions: (s, req) => ({
    async *[Symbol.asyncIterator]() {
      const c = s.cr(s.repo(req.repo), req.changeRequestId ?? "");
      const to = c.revisions.find((r) => r.n === (req.toRevision || c.revisions.length));
      const from = c.revisions.find((r) => r.n === req.fromRevision);
      if (!to || !from) throw new BeeError("not_found", { reason: "UNKNOWN_REVISION", message: "no revision r" + (to ? req.fromRevision : req.toRevision) });
      yield* diffItems(s, from.head, to.head, undefined, req.includeHunks ?? false, req.contextLines || 3);
    },
  }),
  listReviewThreads: (s, req) => ({
    threads: s.cr(s.repo(req.repo), req.changeRequestId ?? "").threads.filter((t) => req.resolved === undefined || t.resolved === req.resolved).map(reviewThread),
    page: { nextPageToken: "" },
  }),
  createChangeComment: (s, req, ctx) => {
    const actor = s.caller(ctx.token);
    const repo = s.repo(req.repo);
    const c = s.cr(repo, req.changeRequestId ?? "");
    const body = req.body ?? "";
    const suggestion = req.suggestion?.replacement ?? "";
    if (!body.trim() && !suggestion) throw invalid("a comment needs a body");
    // Without an anchor it is a conversation comment (decisions on a brief's asks are these).
    if (!req.path) return s.briefs.addComment(s, repo, c, actor, body) as unknown as CommentJson;
    const start = req.startLine ?? 0;
    const t: T.ReviewThread = {
      id: ulid(s.now(), "thread:new:" + c.threads.length + ":" + c.id),
      changeRequestId: c.id,
      path: req.path,
      side: req.side === "old" ? "old" : "new",
      startLine: start,
      endLine: req.endLine || start,
      revision: req.revision || s.current(c).n,
      resolved: false,
      outdated: false,
      comments: [{ id: ulid(s.now(), "comment:new:" + c.id + c.threads.length), author: s.p(actor), body, suggestion, createdAt: s.iso(s.now()) }],
    };
    c.threads.push(t);
    s.emit(repo, "review.commented", actor, { note: { title: c.title, target: "cr:" + c.id } });
    const first = t.comments[0] as T.ThreadComment;
    const out: CommentJson = { id: first.id, subject: { kind: "SUBJECT_KIND_CHANGE_REQUEST", repo: { fullName: repo.repo.fullName }, id: c.id }, author: first.author, body: first.body, createdAt: first.createdAt };
    return out;
  },
  listChangeComments: (s, req) => listConversation(s, req),
  replyToReviewThread: (s, req, ctx) => {
    const actor = s.caller(ctx.token);
    const repo = s.repo(req.repo);
    const { t } = threadOf(s, repo, req.threadId ?? "");
    const body = req.body ?? "";
    if (!body.trim()) throw invalid("a reply needs a body");
    const cm: T.ThreadComment = { id: ulid(s.now(), "reply:" + t.id + t.comments.length), author: s.p(actor), body, suggestion: req.suggestion?.replacement ?? "", createdAt: s.iso(s.now()) };
    t.comments.push(cm);
    return reviewComment(t, cm);
  },
  resolveReviewThread: (s, req, ctx) => resolveThread(s, req as { repo?: T.RepoRef; threadId?: string }, ctx, true),
  unresolveReviewThread: (s, req, ctx) => resolveThread(s, req as { repo?: T.RepoRef; threadId?: string }, ctx, false),
  applySuggestions: (s, req, ctx) => {
    const actor = s.caller(ctx.token);
    const repo = s.repo(req.repo);
    const c = s.cr(repo, req.changeRequestId ?? "");
    const head = s.current(c).head;
    const ids = new Set(req.commentIds ?? []);
    const chosen = c.threads.filter((t) => !t.resolved && t.comments.some((cm) => cm.suggestion && ids.has(cm.id)));
    if (chosen.length === 0) throw new BeeError("failed_precondition", { reason: "NO_SUGGESTIONS", message: "none of the selected comments carries an open suggestion" });
    const files: Record<string, string> = {};
    // Apply bottom-up per file so earlier line numbers stay valid.
    for (const t of [...chosen].sort((a, b) => b.startLine - a.startLine)) {
      const path = pathText(t.path);
      const text = files[path] ?? head.files.get(path);
      if (text === undefined) throw new BeeError("failed_precondition", { reason: "SUGGESTION_OUTDATED", message: path + " no longer exists in the head" });
      const lines = text.split("\n");
      const suggestion = t.comments.find((cm) => cm.suggestion && ids.has(cm.id))?.suggestion ?? "";
      lines.splice(t.startLine - 1, t.endLine - t.startLine + 1, ...suggestion.split("\n"));
      files[path] = lines.join("\n");
    }
    s.revise(c, files, actor);
    for (const t of chosen) t.resolved = true;
    s.appendAudit(actor, "change.revise", c.title + " (applied " + chosen.length + " suggestion" + (chosen.length === 1 ? "" : "s") + ")", repo.name, true, "");
    return s.crView(c);
  },
  getMergeQueue: (s, req) => {
    const repo = s.repo(req.repo);
    const list = s.queue.get(repo.name) ?? [];
    return { entries: list.map((q, i) => queueEntry(s, q, i)), landsPerMinute: 4 };
  },
  enqueueChange: (s, req, ctx) => {
    const actor = s.caller(ctx.token);
    const repo = s.repo(req.repo);
    const c = s.cr(repo, req.id ?? "");
    s.enqueue(c, actor);
    const list = s.queue.get(repo.name) ?? [];
    const i = list.findIndex((q) => q.crId === c.id);
    return queueEntry(s, list[i] as (typeof list)[number], i);
  },
});

const check = defineService(CheckService, {
  listChecks: (s, req) => {
    const repo = s.repo(req.repo);
    if (req.changeset) return { checks: s.checks.get(objectIdText(req.changeset as T.ObjectId)) ?? [] };
    return { checks: [...s.checks.values()].flat().filter((c) => c.repoId === repo.repo.id) };
  },
  listCheckAnnotations: (s, req) => {
    // Annotations belong to a run in the store; serve them on the run's test check.
    const check = [...s.checks.values()].flat().find((c) => c.id === req.checkRunId);
    if (!check) throw new BeeError("not_found", { reason: "CHECK_NOT_FOUND", message: "no check run " + req.checkRunId });
    const run = s.runs.find((r) => r.head?.raw === check.changeset?.raw && r.jobs.some((j) => check.url.includes(j.id)));
    const own = run && (check.name === "test" || !run.jobs.some((j) => j.name === "test"));
    return {
      annotations: own ? run.annotations.map((a) => ({ path: pathBytes(a.path), startLine: a.line, endLine: a.line, level: ("ANNOTATION_LEVEL_" + a.level.toUpperCase()) as "ANNOTATION_LEVEL_FAILURE", title: a.title, message: a.message })) : [],
      page: { nextPageToken: "" },
    };
  },
});

const lease = defineService(LeaseService, {
  acquireLease: (s, req, ctx) => {
    const actor = s.caller(ctx.token);
    const repo = s.repo(req.repo);
    const paths = (req.paths ?? []).map(pathText).filter(Boolean);
    const mode = req.mode ?? "LEASE_MODE_UNSPECIFIED";
    if (paths.length === 0) throw invalid("a lease needs at least one path");
    const others = s.leases.filter((l) => l.repoId === repo.repo.id && l.holder?.id !== s.p(actor).id && l.paths.some((lp) => paths.some((p) => overlaps(pathText(lp), p))));
    const blocking = others.filter((l) => l.mode === "LEASE_MODE_EXCLUSIVE" || mode === "LEASE_MODE_EXCLUSIVE");
    if (blocking.length) {
      throw new BeeError("failed_precondition", {
        reason: "LEASED",
        message: "an exclusive lease held by " + (blocking[0]?.holder?.display ?? "another principal") + " overlaps these paths",
        leaseConflict: { leaseIds: blocking.map((l) => l.id), paths: blocking.flatMap((l) => l.paths) },
      });
    }
    const lease: T.Lease = {
      id: ulid(s.now(), "lease:new:" + s.leases.length + paths.join(",")),
      repoId: repo.repo.id,
      ref: req.ref || "main",
      paths: paths.map(pathBytes),
      holder: s.p(actor),
      mode: mode === "LEASE_MODE_UNSPECIFIED" ? "LEASE_MODE_ADVISORY" : mode,
      expiresAt: s.iso(s.now() + (req.ttlSeconds || 7200) * 1000),
      note: req.note ?? "",
    };
    s.leases.push(lease);
    s.emit(repo, "lease.acquired", actor, { lease });
    s.appendAudit(actor, "lease.acquire", paths.join(", ") + " (" + lease.mode.replace("LEASE_MODE_", "").toLowerCase() + ")", repo.name, true, "");
    return { lease, overlapping: others };
  },
  releaseLease: (s, req, ctx) => {
    const actor = s.caller(ctx.token);
    const repo = s.repo(req.repo);
    const lease = s.leases.find((l) => l.id === req.id);
    if (!lease) throw new BeeError("not_found", { reason: "LEASE_NOT_FOUND", message: "no lease " + req.id });
    s.leases = s.leases.filter((l) => l !== lease);
    s.emit(repo, "lease.released", actor, { lease });
    s.appendAudit(actor, "lease.release", lease.paths.map(pathText).join(", "), repo.name, true, "");
    return {};
  },
  listLeases: (s, req, ctx) => {
    const repos = s.reposFor(req.org ? undefined : req.repo, req.org || undefined, s.viewer(ctx.token)).map((r) => r.repo.id);
    const paths = (req.paths ?? []).map(pathText);
    return { leases: s.leases.filter((l) => repos.includes(l.repoId) && (!paths.length || l.paths.some((lp) => paths.some((p) => overlaps(pathText(lp), p))))) };
  },
});

const event = defineService(EventService, {
  subscribe: (s, req, ctx) => ({
    async *[Symbol.asyncIterator]() {
      const repoIds = new Set(s.reposFor(req.org ? undefined : req.repo, req.org || undefined, s.viewer(ctx.token)).map((r) => r.repo.id));
      const kinds = req.kinds ?? [];
      const kindOk = (k: string) => kinds.length === 0 || kinds.some((x) => (x.endsWith(".*") ? k.startsWith(x.slice(0, -1)) : x === k));
      const want = (e: T.Event) => repoIds.has(e.repoId) && kindOk(e.kind);
      const from = BigInt(req.fromSeq || "0");
      const queue: T.Event[] = [];
      let wake: (() => void) | null = null;
      const off = s.onEvent((e) => {
        if (want(e)) {
          queue.push(e);
          wake?.();
        }
      });
      try {
        for (const e of s.events) if (BigInt(e.seq) > from && want(e)) yield eventJson(s, e);
        while (!ctx.signal?.aborted) {
          while (queue.length) yield eventJson(s, queue.shift() as T.Event);
          await new Promise<void>((resolve) => {
            wake = resolve;
            if (ctx.signal?.aborted) return resolve();
            const t = setTimeout(resolve, 15_000);
            ctx.signal?.addEventListener(
              "abort",
              () => {
                clearTimeout(t);
                resolve();
              },
              { once: true },
            );
          });
          wake = null;
        }
      } finally {
        off();
      }
    },
  }),
  listEvents: (s, req, ctx) => {
    const repoIds = new Set(s.reposFor(req.org ? undefined : req.repo, req.org || undefined, s.viewer(ctx.token)).map((r) => r.repo.id));
    const kinds = req.kinds ?? [];
    const kindOk = (k: string) => kinds.length === 0 || kinds.some((x) => (x.endsWith(".*") ? k.startsWith(x.slice(0, -1)) : x === k));
    // Same contract as the server: seq >= fromSeq, default 100, at most 1000.
    const from = BigInt(req.fromSeq || "0");
    const limit = Math.min(req.limit || 100, 1000);
    const out = s.events.filter((e) => BigInt(e.seq) >= from && repoIds.has(e.repoId) && kindOk(e.kind)).slice(0, limit);
    const last = out.at(-1);
    return { events: out.map((e) => eventJson(s, e)), nextSeq: last ? String(BigInt(last.seq) + 1n) : String(from) };
  },
  getEventLogInfo: (s, req, ctx) => {
    const repoIds = new Set(s.reposFor(req.org ? undefined : req.repo, req.org || undefined, s.viewer(ctx.token)).map((r) => r.repo.id));
    const mine = s.events.filter((e) => repoIds.has(e.repoId));
    return { oldestSeq: mine[0]?.seq ?? "0", latestSeq: mine.at(-1)?.seq ?? "0", retentionDays: 90 };
  },
});

const object = defineService(ObjectService, {
  resolveRef: (s, req) => {
    const repo = s.repo(req.repo);
    const ref = repo.refs.get(req.name ?? "");
    if (!ref) throw new BeeError("not_found", { reason: "REF_NOT_FOUND", message: "no ref " + req.name });
    return { ref: { name: req.name, target: ref.target.id, version: String(ref.version) }, rootTree: objectId("tree", ref.target.key), stale: false };
  },
  // The demo world is the cloud edition unless BEE_FIXTURE_EDITION says otherwise.
  getServerInfo: () => ({
    tier: "server",
    protocolVersions: ["bee.v1"],
    edition: process.env.BEE_FIXTURE_EDITION === "enterprise" ? "enterprise" : "cloud",
    connectBaseUrl: "https://hub.bee.example",
    features: ["ssh", "anonymous_read"],
  }),
});

/** Base-to-head diffs of a CR revision use the revision's own paths and classes. */
function revisionDiff(s: FixtureStore, from: Commit, to: Commit) {
  for (const c of s.crs) for (const r of c.revisions) if (r.base === from && r.head === to) return r;
  return undefined;
}

const query = defineService(QueryService, {
  isAncestor,
  getChangesets: (s, req) => ({
    changesets: (req.ids ?? [])
      .map((id) => s.commits.get(objectIdText(id as T.ObjectId)))
      .filter((c): c is Commit => !!c)
      .map((c) => changesetView(s, c, req.includeMeta ?? false)),
  }),
  readPath: (s, req) => ({
    async *[Symbol.asyncIterator]() {
      const repo = s.repo(req.repo);
      const at = s.resolve(repo, req.at?.expr ?? "");
      const path = pathText(req.path ?? "");
      if (repo.restricted.has(path)) throw new BeeError("permission_denied", { reason: "PERMISSION_DENIED", message: "you cannot read " + path + " in " + repo.repo.fullName });
      const text = at.files.get(path);
      if (text === undefined) throw new BeeError("not_found", { reason: "PATH_NOT_FOUND", message: path + " does not exist at " + (req.at?.expr || "main") });
      const bytes = new TextEncoder().encode(text);
      const CHUNK = 2048;
      for (let o = 0; o < Math.max(1, bytes.length); o += CHUNK) {
        const last = o + CHUNK >= bytes.length;
        yield { mode: "MODE_FILE" as const, blob: objectId("blob", text), size: String(bytes.length), data: bytesToBase64(bytes.subarray(o, o + CHUNK)), last };
      }
    },
  }),
  listTree: (s, req) => {
    const repo = s.repo(req.repo);
    const at = s.resolve(repo, req.at?.expr ?? "");
    const dir = pathText(req.path ?? "").replace(/^\/|\/$/g, "");
    const prefix = dir ? dir + "/" : "";
    const entries = new Map<string, T.TreeEntry>();
    for (const [path, content] of at.files) {
      if (!path.startsWith(prefix)) continue;
      const rest = path.slice(prefix.length);
      const slash = rest.indexOf("/");
      if (slash >= 0 && !req.recursive) {
        const name = prefix + rest.slice(0, slash);
        if (!entries.has(name)) entries.set(name, { path: pathBytes(name), mode: "MODE_TREE", id: objectId("tree", at.key + ":" + name), size: "0", restricted: false });
      } else {
        entries.set(path, { path: pathBytes(path), mode: "MODE_FILE", id: objectId("blob", content), size: String(new TextEncoder().encode(content).length), restricted: repo.restricted.has(path) });
      }
    }
    if (entries.size === 0 && dir) throw new BeeError("not_found", { reason: "PATH_NOT_FOUND", message: dir + " does not exist" });
    const sorted = [...entries.values()].sort((a, b) => (a.mode === b.mode ? (pathText(a.path) < pathText(b.path) ? -1 : 1) : a.mode === "MODE_TREE" ? -1 : 1));
    const pg = page(sorted, req.page, 500);
    return { entries: pg.items, page: { nextPageToken: pg.next } };
  },
  log: (s, req) => {
    const repo = s.repo(req.repo);
    const from = s.resolve(repo, req.from?.expr ?? "");
    const path = req.path ? pathText(req.path) : "";
    const out: Commit[] = [];
    for (let c: Commit | null = from; c; c = c.parent) if (!path || c.changed.some((p) => overlaps(p, path))) out.push(c);
    const pg = page(out, req.page);
    return { changesets: pg.items.map((c) => changesetView(s, c, req.includeMeta ?? false)), page: { nextPageToken: pg.next } };
  },
  blame: (s, req) => {
    const repo = s.repo(req.repo);
    const at = s.resolve(repo, req.at?.expr ?? "");
    const path = pathText(req.path ?? "");
    if (!at.files.has(path)) throw new BeeError("not_found", { reason: "PATH_NOT_FOUND", message: path + " does not exist" });
    if (repo.restricted.has(path)) throw new BeeError("permission_denied", { reason: "PERMISSION_DENIED", message: "you cannot read " + path });
    const versions = s.fileVersions(repo, at, path);
    const origins = blameOrigins(versions.map((v) => v.files.get(path) ?? ""));
    const ranges: T.BlameRange[] = [];
    origins.forEach((o, i) => {
      const last = ranges[ranges.length - 1];
      const cs = versions[o]?.id;
      if (last && last.changeset?.raw === cs?.raw && last.startLine + last.lineCount === i + 1) last.lineCount++;
      else ranges.push({ startLine: i + 1, lineCount: 1, changeset: cs, originalStartLine: i + 1 });
    });
    return { ranges };
  },
  diff: (s, req) => ({
    async *[Symbol.asyncIterator]() {
      const repo = s.repo(req.repo);
      const from = s.resolve(repo, req.from?.expr ?? "");
      const to = s.resolve(repo, req.to?.expr ?? "");
      const rev = revisionDiff(s, from, to);
      const paths = rev ? Object.keys(rev.perPath) : (req.paths ?? []).map(pathText);
      for (const item of diffItems(s, from, to, paths, req.includeHunks ?? false, req.contextLines || 3)) {
        if (rev) item.diffClass = rev.perPath[pathText(item.path)] ?? item.diffClass;
        yield item;
      }
    },
  }),
});

const write = defineService(WriteService, {
  land: (s, req, ctx) => {
    const actor = s.caller(ctx.token);
    const repo = s.repo(req.repo);
    if (!req.changeRequestId) throw new BeeError("unimplemented", { reason: "UNIMPLEMENTED", message: "the fixture lands change requests only" });
    return s.land(s.cr(repo, req.changeRequestId), actor);
  },
});

const ref = defineService(RefService, {
  listRefs: (s, req) => {
    const repo = s.repo(req.repo);
    const refs = [...repo.refs.entries()]
      .filter(([name]) => !req.prefix || name.startsWith(req.prefix))
      .map(([name, r]) => ({ name, target: r.target.id, version: String(r.version) }))
      .sort((a, b) => (a.name === "main" ? -1 : b.name === "main" ? 1 : a.name < b.name ? -1 : 1));
    return { refs, page: { nextPageToken: "" } };
  },
});

const auth = defineService(AuthService, {
  login: (s, req) => {
    if (req.githubStart) {
      const code = "dc_" + digestHex("device:" + s.deviceCodes.size, 10);
      s.deviceCodes.set(code, 0);
      return { token: "", verificationUri: "https://github.com/login/device", userCode: "WDJB-MJHT", deviceCode: code, intervalSeconds: 1 };
    }
    if (req.githubDeviceCode) {
      const polls = s.deviceCodes.get(req.githubDeviceCode);
      if (polls === undefined) throw new BeeError("not_found", { reason: "DEVICE_CODE_EXPIRED", message: "this device code has expired; start again" });
      s.deviceCodes.set(req.githubDeviceCode, polls + 1);
      if (polls < 1) throw new BeeError("failed_precondition", { reason: "AUTHORIZATION_PENDING", message: "waiting for you to approve on GitHub", retryable: true });
      return { token: "swt_demo_" + digestHex("login:" + req.githubDeviceCode, 12), verificationUri: "", userCode: "", deviceCode: "", intervalSeconds: 0 };
    }
    throw new BeeError("unimplemented", { reason: "UNIMPLEMENTED", message: "the fixture supports GitHub device flow only" });
  },
  whoAmI: (s, _req, ctx) => {
    const h = s.caller(ctx.token);
    const pr = s.p(h);
    return {
      principal: pr,
      onBehalfOf: pr.ownerId ? [pr.ownerId] : [],
      effectiveScope: { repos: ["acme/*"], readPaths: [], writePaths: [], refs: ["*"], actions: ["read", "commit", "land", "review", "lease", "admin"], session: "" },
      remaining: { maxChangesets: "0", maxLanded: "0", maxUploadBytes: "0" },
    };
  },
  getPrincipal: (s, req) => {
    const p = s.principals.get(s.handleOf(req.principalId ?? ""));
    if (!p) throw new BeeError("not_found", { reason: "PRINCIPAL_NOT_FOUND", message: "no principal " + req.principalId });
    return p;
  },
  listTokens: (s, req, ctx) => {
    const principalId = req.principalId || s.p(s.caller(ctx.token)).id;
    const tokens = s.tokens.filter((t) => t.principalId === principalId && (req.includeRevoked || !t.revoked));
    return {
      tokens: tokens.map((t) => ({ tokenId: t.tokenId, principalId: t.principalId, parentTokenId: t.parentTokenId, scope: t.scope, createdAt: t.createdAt, revoked: t.revoked, description: t.label, used: t.used })),
      page: { nextPageToken: "" },
    };
  },
  mintToken: (s, req, ctx) => {
    const actor = s.caller(ctx.token);
    const targetHandle = req.principalId ? s.handleOf(req.principalId) : actor;
    const target = s.principals.get(targetHandle);
    if (!target) throw new BeeError("not_found", { reason: "PRINCIPAL_NOT_FOUND", message: "no principal " + req.principalId });
    if (target.kind === "PRINCIPAL_KIND_AGENT" && s.handleOf(target.ownerId) !== actor && actor !== "mara")
      throw new BeeError("permission_denied", { reason: "NOT_OWNER", message: "only an agent's owner can mint tokens for it" });
    const sc = req.scope ?? {};
    const actions = sc.actions ?? [];
    const writePaths = sc.writePaths ?? [];
    if (!actions.length) throw invalid("a token needs at least one action");
    const exp = sc.expiresAt ? Date.parse(sc.expiresAt) : NaN;
    if (!Number.isFinite(exp) || exp <= s.now()) throw invalid("expires_at must be in the future", "INVALID_EXPIRY");
    // Attenuation: a minted token can never exceed the root it derives from.
    const root = s.tokens.find((t) => t.principalId === target.id && !t.parentTokenId && !t.revoked);
    if (root) {
      const extra = actions.filter((a) => !root.scope.actions.includes(a));
      if (extra.length) throw new BeeError("permission_denied", { reason: "SCOPE_EXCEEDS_PARENT", message: "the parent token does not allow: " + extra.join(", ") });
      const rootWrites = root.scope.writePaths.map(pathText);
      const wider = writePaths.map(pathText).filter((p) => rootWrites.length && !rootWrites.some((r) => overlaps(p, r) && p.startsWith(r)));
      if (wider.length) throw new BeeError("permission_denied", { reason: "SCOPE_EXCEEDS_PARENT", message: "the parent token cannot write " + wider.join(", ") });
    }
    const tokenId = "tok_" + digestHex("minted:" + s.tokens.length + ":" + targetHandle, 8);
    const secret = "swt_" + digestHex("secret:" + tokenId, 20);
    s.tokens.push({
      tokenId,
      parentTokenId: root?.tokenId ?? "",
      principalId: target.id,
      scope: { repos: sc.repos ?? [], readPaths: sc.readPaths ?? [], writePaths, refs: sc.refs ?? [], actions, expiresAt: sc.expiresAt, budget: sc.budget as T.Budget | undefined, session: sc.session || "" },
      used: { maxChangesets: "0", maxLanded: "0", maxUploadBytes: "0" },
      createdAt: s.iso(s.now()),
      revoked: false,
      label: (sc.session ? sc.session + " · " : "") + "minted from the web",
    });
    s.tokenSecrets.set(secret, tokenId);
    s.appendAudit(actor, "token.mint", tokenId + " for " + target.display, "", true, root ? "attenuated from " + root.label : "");
    return { token: secret, tokenId };
  },
  revoke: (s, req, ctx) => {
    const actor = s.caller(ctx.token);
    s.revoke(req.tokenId ?? "", actor);
    return {};
  },
});

const importSvc = defineService(ImportService, {
  startImport: (s, req, ctx) => {
    const actor = s.caller(ctx.token);
    const source = (req.source ?? "").trim().replace(/^https:\/\/github\.com\//, "").replace(/\/$/, "");
    if (!/^[\w.-]+(\/[\w.-]+)?$/.test(source)) throw invalid('source must look like "owner/repo" or an org name', "INVALID_SOURCE");
    if (!req.org && !source.includes("/")) throw invalid("pass an owner/repo, or set org to import a whole org", "INVALID_SOURCE");
    if (source.endsWith("/private-archive")) throw new BeeError("permission_denied", { reason: "GITHUB_FORBIDDEN", message: "the GitHub App installation cannot read " + source });
    const imp: T.Import = {
      id: ulid(s.now(), "import:" + source + ":" + s.imports.length),
      source: "github:" + source + (req.org ? "/*" : ""),
      repoIds: [],
      phase: req.phase === "IMPORT_PHASE_DRY_RUN" ? "IMPORT_PHASE_DRY_RUN" : "IMPORT_PHASE_MIRROR",
      state: "IMPORT_STATE_QUEUED",
      progress: [],
      rateLimitNote: req.githubInstallationId ? "GitHub App: 15,000 req/h, 0% used" : "Personal token: 5,000 req/h, 0% used (an App installation is 3x faster)",
      startedAt: s.iso(s.now()),
    };
    const scale = 1 + (source.length % 5);
    s.imports.push({ imp, started: s.now(), totals: Object.fromEntries(IMPORT_CATEGORIES.map(([k, v]) => [k, Math.max(1, Math.round((v * scale) / 4))])) });
    s.appendAudit(actor, imp.phase === "IMPORT_PHASE_DRY_RUN" ? "import.dry_run" : "import.start", imp.source, "", true, "");
    return s.importView(s.imports[s.imports.length - 1] as Parameters<FixtureStore["importView"]>[0]);
  },
  getImport: (s, req) => s.importView(importOf(s, req.id)),
  listImports: (s) => ({ imports: s.imports.map((i) => s.importView(i)).reverse(), page: { nextPageToken: "" } }),
  setImportPhase: (s, req, ctx) => {
    const actor = s.caller(ctx.token);
    const st = importOf(s, req.id);
    st.imp.phase = req.phase ?? st.imp.phase;
    s.appendAudit(actor, "import.phase", st.imp.source + " → " + st.imp.phase.replace("IMPORT_PHASE_", "").toLowerCase(), "", true, "");
    return s.importView(st);
  },
  cancelImport: (s, req) => {
    const st = importOf(s, req.id);
    st.imp.state = "IMPORT_STATE_CANCELLED";
    return st.imp;
  },
  getFidelityReport: (s, req) => {
    const st = importOf(s, req.id);
    const drops: Record<string, T.FidelityDrop[]> = {
      commits: [{ ref: "a1f9c03e", reason: "force-pushed commit no longer served by GitHub; kept in the archive only" }],
      review_comments: [
        { ref: "pull/412#review-comment-99", reason: "comment on a line in a force-pushed revision; attached to the file instead" },
        { ref: "pull/977#review-comment-12", reason: "comment on a line in a force-pushed revision; attached to the file instead" },
      ],
      lfs_objects: [],
    };
    return {
      importId: st.imp.id,
      items: Object.entries(st.totals).map(([category, total]) => {
        const d = drops[category] ?? [];
        const archived = category === "pull_requests" ? Math.round(total * 0.002) : 0;
        return { category, imported: String(total - d.length - archived), archived: String(archived), dropped: String(d.length), drops: d };
      }),
      manualSteps: ["Re-enter 6 repository secrets (values are never exported by GitHub)", "Enable 2 webhooks after verifying their endpoints", "Install the Beehive GitHub App on acme for bridge mode"],
      generatedAt: s.iso(s.now()),
    };
  },
});

function importOf(s: FixtureStore, id: string | undefined) {
  const st = s.imports.find((i) => i.imp.id === id);
  if (!st) throw new BeeError("not_found", { reason: "IMPORT_NOT_FOUND", message: "no import " + id });
  return st;
}

/** Every service the fixture serves. */
export const SERVICES: ServiceDef[] = [org, repoSvc, change, check, lease, event, object, query, write, ref, auth, importSvc, swarmService, safetyService, ...VIEW_SERVICES, ...COMMUNITY_SERVICES, ...POST_SERVICES, ...BRIEF_SERVICES, ...ADMIN_SERVICES, ...HUB_SERVICES];
