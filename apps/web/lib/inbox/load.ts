/**
 * Server-side loads for Needs you. Each runs once per request (React `cache`), so the rail's
 * count and the Needs you page share one scan of the caller's repositories and always show the
 * same number.
 *
 * The scan is bounded so a full page load stays cheap: the first `MAX_REPOS` repositories (newest
 * push first, as ListMyRepos sorts them) and, across them, the `MAX_CHANGES` most recently revised
 * open change requests. `truncated` says when that cut anything, and the page says so.
 */
import { cache } from "react";
import { pathBytes, pathText } from "@/lib/api/ids";
import { serverClient, sessionState } from "@/lib/api/server";
import type { AgentSummary, ChangeRequest, FeedEvent, Lease, Principal, Ref, Repo, SafetyAttention } from "@/lib/api/types";
import { branchOf, directoryOf } from "@/lib/pulls";
import { needsOf, safetyNeeds, type ChangeFacts, type Need, type SafetyNeed } from "./needs";
import { parseApproval } from "./words";

export const MAX_REPOS = 20;
export const MAX_CHANGES = 60;
/** Open change requests read per repository; more than this marks the scan truncated. */
const OPEN_PER_REPO = 50;
/** Recently merged change requests read per repository, for posts waiting on approval. */
const LANDED_PER_REPO = 20;

export interface NeedsScan {
  me: Principal;
  /** Every repository the caller can work in. */
  repos: Repo[];
  /** The repositories the scan read. */
  checked: string[];
  /** Repositories whose changes could not be read; the page names them. */
  failed: string[];
  facts: ChangeFacts[];
  /** Open change requests across the checked repositories, before the `MAX_CHANGES` cut. */
  open: number;
  truncated: boolean;
  needs: Need[];
  safety: SafetyNeed[];
  /** Repositories whose safety inbox could not be read. */
  safetyFailed: string[];
  /** The people of the caller's organisations, by id: who answers for an agent. */
  directory: Map<string, Principal>;
}

/**
 * How many things need the caller. The rail's count and the page's own heading both call
 * this on the scan `loadNeeds` returns, so they read one number from one snapshot.
 */
export const needCount = (scan: NeedsScan): number => scan.needs.length + scan.safety.length;

type Api = Awaited<ReturnType<typeof serverClient>>;

interface RepoScan {
  repo: Repo;
  open: ChangeRequest[];
  more: boolean;
  queued: Set<string>;
  landed: ChangeRequest[];
  refs: Ref[];
}

async function scanRepo(api: Api, repo: Repo): Promise<RepoScan> {
  const ref = { fullName: repo.fullName };
  const [open, queue, landed, refs] = await Promise.all([
    api.change.listChanges({ repo: ref, state: "CHANGE_STATE_OPEN", page: { pageSize: OPEN_PER_REPO } }),
    // A repository without a merge queue simply has nothing queued.
    api.change.listQueue({ repo: ref, ref: repo.defaultRef || "main" }).catch(() => ({ entries: [] })),
    api.change.listChanges({ repo: ref, state: "CHANGE_STATE_LANDED", page: { pageSize: LANDED_PER_REPO } }),
    // Only names the branch a pull request comes from; without it the row shows the target alone.
    api.ref.listRefs({ repo: ref }).catch(() => ({ refs: [] as Ref[] })),
  ]);
  return { repo, open: open.changes, more: !!open.page?.nextPageToken, queued: new Set(queue.entries.map((e) => e.changeRequestId)), landed: landed.changes, refs: refs.refs };
}

async function readSafetyAttention(api: Api, fullName: string): Promise<SafetyAttention[]> {
  const items: SafetyAttention[] = [];
  let pageToken: string | undefined;
  do {
    const reply = await api.safety.listSafetyAttention({ repo: { fullName }, page: { pageSize: 100, pageToken } });
    items.push(...reply.items);
    pageToken = reply.page?.nextPageToken;
  } while (pageToken);
  return items;
}

const revisedAt = (c: ChangeRequest) => Date.parse(c.revisions.at(-1)?.createdAt ?? c.createdAt ?? "") || 0;

async function factsOf(api: Api, scan: RepoScan, change: ChangeRequest, owners: Map<string, Promise<string[]>>): Promise<ChangeFacts> {
  const ref = { fullName: scan.repo.fullName };
  const [landing, { checks }, { reviews }] = await Promise.all([
    api.change.evaluateLanding({ repo: ref, changeRequestId: change.id }),
    api.check.listChecks({ repo: ref, changeset: change.revisions.at(-1)?.head }),
    api.change.listReviews({ repo: ref, changeRequestId: change.id }),
  ]);
  const byPath: Record<string, string[]> = {};
  for (const m of landing.missing) {
    const a = parseApproval(m);
    if (a?.from !== "owner") continue;
    const key = scan.repo.fullName + "\n" + a.scope;
    if (!owners.has(key))
      owners.set(
        key,
        api.repo.getContext({ repo: ref, paths: [pathBytes(a.scope)] }).then((c) => c.owners.find((o) => pathText(o.path) === a.scope)?.owners ?? c.owners[0]?.owners ?? []),
      );
    byPath[a.scope] = await owners.get(key)!;
  }
  return { repo: scan.repo.fullName, change, landing, checks, reviews, owners: byPath, queued: scan.queued.has(change.id), branch: branchOf(scan.refs, change.revisions.at(-1)?.head, change.targetRef) };
}

/** What needs the signed-in caller. Throws when signed out or when their repositories cannot be listed. */
export const loadNeeds = cache(async (): Promise<NeedsScan> => {
  const { me, error } = await sessionState();
  if (!me) throw error ?? new Error("Needs you is for a signed-in caller.");
  const api = await serverClient();
  const [{ repos }, directory] = await Promise.all([api.repo.listMyRepos({}), loadDirectory(api)]);
  const failed: string[] = [];
  const safetyFailed: string[] = [];
  const checkedRepos = repos.slice(0, MAX_REPOS);
  const [changes, safetyItems] = await Promise.all([
    Promise.all(checkedRepos.map((r) => scanRepo(api, r).catch(() => {
      failed.push(r.fullName);
      return null;
    }))),
    Promise.all(checkedRepos.map(async (r) => {
      try {
        return safetyNeeds(r.fullName, await readSafetyAttention(api, r.fullName), me);
      } catch {
        safetyFailed.push(r.fullName);
        return [];
      }
    })),
  ]);
  const scans = changes.filter((s): s is RepoScan => s !== null);
  const safety = safetyItems.flat().sort((a, b) =>
    (a.kind === "tripwire" ? 0 : 1) - (b.kind === "tripwire" ? 0 : 1)
    || (b.at ?? "").localeCompare(a.at ?? "")
    || a.id.localeCompare(b.id));

  // The most recently revised open changes across every repository, newest first; ties by id so
  // the cut is the same on every load.
  const open = scans.flatMap((s) => s.open.map((change) => ({ scan: s, change })));
  open.sort((a, b) => revisedAt(b.change) - revisedAt(a.change) || (a.change.id < b.change.id ? -1 : 1));
  const owners = new Map<string, Promise<string[]>>();
  const facts = (
    await Promise.all(
      open.slice(0, MAX_CHANGES).map(({ scan, change }) =>
        factsOf(api, scan, change, owners).catch(() => {
          if (!failed.includes(scan.repo.fullName)) failed.push(scan.repo.fullName);
          return null;
        }),
      ),
    )
  ).filter((f): f is ChangeFacts => f !== null);

  const landed = scans.flatMap((s) => s.landed.map((change) => ({ repo: s.repo.fullName, change })));
  return {
    me,
    repos,
    checked: scans.map((s) => s.repo.fullName),
    failed: failed.sort(),
    facts,
    open: open.length,
    truncated: repos.length > MAX_REPOS || open.length > MAX_CHANGES || scans.some((s) => s.more),
    needs: needsOf(facts, landed, me, directory),
    safety,
    safetyFailed: safetyFailed.sort(),
    directory,
  };
});

/**
 * The members of the caller's organisations. An agent's owner is one of them; when a list
 * cannot be read the owner is simply not named.
 */
async function loadDirectory(api: Api): Promise<Map<string, Principal>> {
  const orgs = await api.org.listOrgs({}).then((r) => r.orgs, () => []);
  const members = await Promise.all(orgs.map((o) => api.org.listMembers({ org: o.name }).then((r) => r.members.map((m) => m.principal), () => [])));
  return directoryOf(...members);
}

/** The caller's agents (across their orgs) and the leases held in the scanned repositories. */
export const loadAgents = cache(async (): Promise<{ agents: AgentSummary[]; leases: { repo: string; lease: Lease }[] }> => {
  const api = await serverClient();
  const [{ orgs }, scan] = await Promise.all([api.org.listOrgs({}), loadNeeds()]);
  const [agents, leases] = await Promise.all([
    Promise.all(orgs.map(async (o) => (await api.agent.listAgents({ org: o.name })).agents)).then((r) => r.flat()),
    Promise.all(scan.checked.map(async (repo) => (await api.lease.listLeases({ repo: { fullName: repo } })).leases.map((lease) => ({ repo, lease })))).then((r) => r.flat()),
  ]);
  // An agent working across two orgs is listed once.
  const seen = new Set<string>();
  return { agents: agents.filter((a) => !seen.has(a.principal.id) && seen.add(a.principal.id) !== undefined), leases };
});

/** Recent merges the caller can see, newest first: enough for "Done today" and "This week". */
export const loadMerges = cache(async (): Promise<FeedEvent[]> => {
  const api = await serverClient();
  return (await api.user.listHomeFeed({ kinds: ["land"], page: { pageSize: 100 } })).events;
});

/** One clock reading per request, so every section measures "today" and "this week" from it. */
export const requestNow = cache((): number => Date.now());
