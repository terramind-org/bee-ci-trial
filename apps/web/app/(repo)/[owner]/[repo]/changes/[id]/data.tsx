/**
 * What the change request page reads, shared by its layout and its four tabs. Every loader is a
 * React `cache`, so the layout and the tab it wraps ask the server once per request for the same
 * thing. The change itself fails loudly (the page cannot exist without it); everything a tab only
 * adds to it (evidence, the checklist, comments, sessions, runs) settles to `{ error }` instead, so
 * one unavailable service costs one section, not the page.
 */
import { cache } from "react";
import { objectIdText, parseObjectId } from "@/lib/api/ids";
import { serverClient } from "@/lib/api/server";
import { BeeError } from "@/lib/api/transport";
import type { BriefChecklist, ChangeRequest, Changeset, LandingEvaluation, ObjectId, Principal, Ref, RepoRef } from "@/lib/api/types";
import type { ChangeMdInput } from "@/lib/changemd";
import type { Op } from "@/lib/copyas";
import { landEntryFor, landedStack, openStack, type StackMember } from "@/lib/infra";
import { branchOf, directoryOf } from "@/lib/pulls";

export type Api = Awaited<ReturnType<typeof serverClient>>;

/** A supplementary read: its value, or the typed error that stood in its way. */
export type Settled<T> = { value: T; error?: undefined } | { value?: undefined; error: BeeError };

async function settle<T>(p: Promise<T>): Promise<Settled<T>> {
  try {
    return { value: await p };
  } catch (err) {
    if (err instanceof BeeError) return { error: err };
    throw err;
  }
}

const refOf = (fullName: string): RepoRef => ({ fullName });

export const api = cache(() => serverClient());

export const loadChange = cache(async (fullName: string, id: string): Promise<ChangeRequest> => (await api()).change.getChange({ repo: refOf(fullName), id }));

/** Landing as policy sees it now, and the change's merge-queue entry when it has one. */
export const loadLanding = cache(async (fullName: string, id: string, targetRef: string) => {
  const a = await api();
  const [evaluation, queue] = await Promise.all([a.change.evaluateLanding({ repo: refOf(fullName), changeRequestId: id }), a.change.listQueue({ repo: refOf(fullName), ref: targetRef })]);
  return { evaluation, queue: queue.entries.find((e) => e.changeRequestId === id) ?? null };
});

export const loadReviews = cache(async (fullName: string, id: string) => (await (await api()).change.listReviews({ repo: refOf(fullName), changeRequestId: id })).reviews);

export const loadThreads = cache(async (fullName: string, id: string) => (await (await api()).change.listThreads({ repo: refOf(fullName), changeRequestId: id })).threads);

/** Check runs reported on one changeset (a revision's head). */
export const loadChecks = cache(async (fullName: string, head: string) => {
  if (!head) return [];
  const id = parseObjectId(head);
  return (await (await api()).check.listChecks({ repo: refOf(fullName), changeset: id ?? undefined })).checks;
});

export const loadDiff = cache(async (fullName: string, change: ChangeRequest, revision: number, since: number) => {
  const rev = change.revisions[revision - 1];
  return (await (await api()).change.getChangeDiff({ repo: refOf(fullName), changeRequestId: change.id, revision, sinceRevision: since, base: rev?.base, head: rev?.head })).items;
});

export const loadEvidence = cache(async (fullName: string, id: string) =>
  settle(
    (async () => {
      const a = await api();
      const out = [];
      let token = "";
      // A change holds at most 64 items (SPEC §11.9); the loop is for a server with a smaller page.
      for (let i = 0; i < 10; i++) {
        const res = await a.brief.listEvidence({ repo: refOf(fullName), changeIds: [id], page: { pageSize: 100, pageToken: token || undefined } });
        out.push(...res.evidence);
        token = res.page?.nextPageToken ?? "";
        if (!token) break;
      }
      return out;
    })(),
  ),
);

export const loadChecklist = cache(async (fullName: string, id: string) => settle((async () => (await api()).brief.getBriefChecklist({ repo: refOf(fullName), changeId: id }))()));

export const loadComments = cache(async (fullName: string, id: string) => settle((async () => (await (await api()).change.listComments({ repo: refOf(fullName), changeRequestId: id, page: { pageSize: 200 } })).comments)()));

export const loadSessions = cache(async (fullName: string, id: string) => settle((async () => (await (await api()).agent.getSessionsForChange({ repo: refOf(fullName), changeRequest: id })).sessions)()));

export const loadRuns = cache(async (fullName: string, id: string) => settle((async () => (await (await api()).workflow.listRuns({ repo: refOf(fullName), changeRequestId: id })).runs)()));

/** Every change request of the repo, for "stacked on" and "stacked on top of this". */
export const loadSiblings = cache(async (fullName: string) => {
  try {
    return (await (await api()).change.listChanges({ repo: refOf(fullName) })).changes;
  } catch (err) {
    if (err instanceof BeeError) return [];
    throw err;
  }
});

/**
 * The members of this change's stack. Open: the first-parent chain of what the head adds to the
 * target (CompareRevisions). Landed: the members the land recorded in the target's ref log, each
 * read back for its parents. Supplementary: when the server cannot answer, the panel is left out
 * rather than guessed.
 */
export const loadStack = cache(async (fullName: string, cr: ChangeRequest): Promise<{ members: StackMember[]; op: Op } | null> => {
  const a = await api();
  const repo = refOf(fullName);
  const headId: ObjectId | undefined = cr.revisions.at(-1)?.head;
  try {
    if (cr.state === "CHANGE_STATE_LANDED") {
      const req = { repo, name: cr.targetRef, page: { pageSize: 100 } };
      const entry = landEntryFor((await a.ref.getRefLog(req)).entries, cr.changeIds);
      if (!entry) return null;
      const sources = entry.landed.flatMap((l) => (l.source ? [l.source] : []));
      const { changesets } = await a.query.getChangesets({ repo, ids: sources, includeMeta: false });
      const parents = new Map(changesets.map((c) => [objectIdText(c.id), c.parents]));
      const members = landedStack(entry, (src) => parents.get(objectIdText(src)) ?? []).reverse();
      return { members, op: { service: "RefService", method: "GetRefLog", request: req, label: "Read the merge from the ref log" } };
    }
    if (cr.state !== "CHANGE_STATE_OPEN" || !headId || !parseObjectId(objectIdText(headId))) return null;
    const req = { repo, base: { expr: cr.targetRef }, head: { expr: objectIdText(headId) } };
    const { changesets } = await a.query.compareRevisions(req);
    const members = openStack(headId, changesets);
    return members.length ? { members, op: { service: "QueryService", method: "CompareRevisions", request: req, label: "List the commits this pull request adds" } } : null;
  } catch {
    return null;
  }
});

/**
 * The commits a pull request merges, with their messages and provenance. When the server lists
 * the stack (`loadStack`), these are its members, oldest first, and `known` is true: the count
 * is the pull request's. When it cannot, they are the head of each revision instead and `known`
 * is false, so the page shows them without claiming a count.
 */
export const loadCommits = cache(async (fullName: string, cr: ChangeRequest): Promise<{ known: boolean; commits: Changeset[] }> => {
  const stack = await loadStack(fullName, cr);
  const wanted = stack ? [...stack.members].reverse().map((m) => parseObjectId(m.id)) : cr.revisions.map((r) => r.head ?? null);
  const ids = wanted.filter((id): id is ObjectId => !!id && !!parseObjectId(objectIdText(id)));
  if (!ids.length) return { known: !!stack, commits: [] };
  try {
    const { changesets } = await (await api()).query.getChangesets({ repo: refOf(fullName), ids, includeMeta: true });
    const by = new Map(changesets.map((c) => [objectIdText(c.id), c]));
    return { known: !!stack, commits: ids.map((id) => by.get(objectIdText(id))).filter((c): c is Changeset => !!c) };
  } catch (err) {
    if (err instanceof BeeError) return { known: !!stack, commits: [] };
    throw err;
  }
});

/** The branch the pull request comes from: the ref at its head. Empty when no ref points there. */
export const loadBranch = cache(async (fullName: string, cr: ChangeRequest): Promise<string> => {
  const refs = await (await api()).ref.listRefs({ repo: refOf(fullName) }).then((r) => r.refs, (): Ref[] => []);
  return branchOf(refs, cr.revisions.at(-1)?.head, cr.targetRef);
});

/**
 * The organisation's people by id, to name the person who answers for an agent. Members only:
 * for anyone else (and when the list cannot be read) it is empty and the person is not named.
 */
export const loadPeople = cache(async (org: string): Promise<Map<string, Principal>> => {
  const members = await (await api()).org.listMembers({ org }).then((r) => r.members.map((m) => m.principal), (): (Principal | undefined)[] => []);
  return directoryOf(members);
});

/** The head's provenance (runtime, model, session, cost) and import marks, for one revision. */
export const loadHead = cache(async (fullName: string, head: string) => {
  const id = parseObjectId(head);
  if (!id) return undefined;
  return (await (await api()).query.getChangesets({ repo: refOf(fullName), ids: [id], includeMeta: true })).changesets[0];
});

async function orEmpty<T>(p: Promise<T>, empty: T): Promise<T> {
  const s = await settle(p);
  return s.error ? empty : s.value;
}

/**
 * Everything `change.md` renders, read with the given client: the For agents tab passes the
 * session's, the `change.md` route the caller's Bearer token. Supplementary reads that fail are
 * left out of the document rather than failing it; the change itself must be readable.
 */
export async function changeMdInput(client: Api, fullName: string, id: string, origin: string): Promise<ChangeMdInput> {
  const repo = refOf(fullName);
  const change = await client.change.getChange({ repo, id });
  const rev = change.revisions.at(-1);
  const [checklist, evidence, checks, reviews, threads, comments, diff, landing] = await Promise.all([
    orEmpty(client.brief.getBriefChecklist({ repo, changeId: id }).then((c): BriefChecklist | undefined => c), undefined),
    orEmpty(client.brief.listEvidence({ repo, changeIds: [id], page: { pageSize: 100 } }).then((r) => r.evidence), []),
    rev?.head ? orEmpty(client.check.listChecks({ repo, changeset: rev.head }).then((r) => r.checks), []) : Promise.resolve([]),
    orEmpty(client.change.listReviews({ repo, changeRequestId: id }).then((r) => r.reviews), []),
    orEmpty(client.change.listThreads({ repo, changeRequestId: id }).then((r) => r.threads), []),
    orEmpty(client.change.listComments({ repo, changeRequestId: id, page: { pageSize: 200 } }).then((r) => r.comments), []),
    rev ? orEmpty(client.change.getChangeDiff({ repo, changeRequestId: id, revision: rev.n, sinceRevision: 0, base: rev.base, head: rev.head }).then((r) => r.items), []) : Promise.resolve([]),
    change.state === "CHANGE_STATE_OPEN" ? orEmpty(client.change.evaluateLanding({ repo, changeRequestId: id }).then((l): LandingEvaluation | undefined => l), undefined) : Promise.resolve(undefined),
  ]);
  return { repo: fullName, change, checklist, evidence, checks, reviews, threads, comments, diff, landing, origin };
}
