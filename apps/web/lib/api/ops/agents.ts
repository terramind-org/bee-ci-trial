/** The agent registry, tokens and provenance (agents.proto, auth.proto). */
import { AgentService, type AgentPrincipalSummaryJson } from "../../gen/bee/v1/agents_pb";
import { AuthService } from "../../gen/bee/v1/auth_pb";
import type { PrincipalJson } from "../../gen/bee/v1/common_pb";
import { ChangeService, LeaseService, OrgService, RepoService } from "../../gen/bee/v1/forge_pb";
import { QueryService } from "../../gen/bee/v1/vcs_pb";
import { objectIdText, parseObjectId, pathText } from "../ids";
import type { Rpc } from "../rpc";
import { BeeError, type CallOptions } from "../transport";
import type * as T from "../types";
import { list, mapLimit, notFound, num, pageOf, pass, str, wire, type PartialOps } from "./shared";

const DAY = 86_400_000;
const ACTIVE_WINDOW = 6 * 3_600_000;

/** The organization directory and its tokens must retain every page the wire offers. */
async function allRows<T>(read: (page: T.PageRequest) => Promise<{ rows?: T[]; next?: string }>): Promise<T[]> {
  const rows: T[] = [];
  let pageToken = "";
  do {
    const result = await read({ pageSize: 200, pageToken });
    rows.push(...list(result.rows));
    pageToken = str(result.next);
  } while (pageToken);
  return rows;
}

/** Owners are principals too; resolve each id once. Unknown ids become undefined. */
export async function principals(rpc: Rpc, ids: string[], opts?: CallOptions): Promise<Map<string, T.Principal>> {
  const unique = [...new Set(ids.filter(Boolean))];
  const found = await mapLimit(unique, 8, async (principalId) => {
    try {
      return (await rpc.call(AuthService.method.getPrincipal, { principalId }, opts)) as T.Principal;
    } catch {
      return undefined;
    }
  });
  return new Map(unique.flatMap((id, i) => (found[i] ? [[id, found[i] as T.Principal]] : [])));
}

/**
 * Some messages carry a principal as a bare id (lease holders, run actors, audit actors). Returns
 * a function that gives each its directory entry (AuthService.GetPrincipal), one call per id.
 */
export async function namer(rpc: Rpc, ps: (T.Principal | { id?: string; display?: string } | undefined)[], opts?: CallOptions): Promise<(p: T.Principal | undefined) => T.Principal | undefined> {
  const bare = ps.filter((p) => p?.id && !p.display).map((p) => str(p?.id));
  const names = bare.length ? await principals(rpc, bare, opts) : new Map<string, T.Principal>();
  return (p) => (p?.id && !p.display ? (names.get(p.id) ?? p) : p);
}

export function agentView(a: AgentPrincipalSummaryJson, owner: T.Principal | undefined, now = Date.now()): T.AgentSummary {
  const stats = a.stats ?? {};
  const leases = num(stats.activeLeases);
  const last = a.lastActiveAt ? Date.parse(a.lastActiveAt) : 0;
  return {
    principal: a.principal as T.Principal,
    owner,
    purpose: str(a.purpose),
    status: leases > 0 || num(stats.activeRuns) > 0 || (last && now - last < ACTIVE_WINDOW) ? "active" : "idle",
    model: str(a.lastModel),
    session: str(a.lastSession),
    openChanges: num(a.openChangeRequests),
    landed7d: num(stats.outcomes?.changeRequestsLanded),
    spend7dUsdMicros: str(stats.cost?.usdMicros) || "0",
    activeLeases: leases,
    lastSeen: a.lastActiveAt,
    registered: true,
  };
}

/** The repo's default ref, for history reads. */
async function defaultRef(rpc: Rpc, repo: T.RepoRef, opts?: CallOptions): Promise<string> {
  return str((await rpc.call(RepoService.method.getRepo, { repo }, opts)).defaultRef) || "main";
}

/**
 * Agent principals that are org members but have no registry definition. Their numbers come
 * from what the repo itself records: landed changesets by the agent (QueryService.Log meta, with
 * provenance), its open change requests and the leases it holds.
 */
async function unregisteredAgents(rpc: Rpc, org: string, repo: T.RepoRef | undefined, known: Set<string>, principalIds: string[], opts?: CallOptions, now = Date.now()): Promise<T.AgentSummary[]> {
  const allMembers = await allRows(async (page) => {
    const response = await rpc.call(OrgService.method.listMembers, { org, page }, opts);
    return { rows: response.members, next: response.page?.nextPageToken };
  });
  const members = allMembers
    .map((m) => m.principal as T.Principal | undefined)
    .filter((p): p is T.Principal => !!p && p.kind === "PRINCIPAL_KIND_AGENT" && !known.has(p.id) && (!principalIds.length || principalIds.includes(p.id)));
  if (!members.length) return [];
  const owners = await principals(rpc, members.map((p) => str(p.ownerId)), opts);
  const at = repo ? await defaultRef(rpc, repo, opts) : "";
  const [leases, open] = repo
    ? await Promise.all([
        rpc.call(LeaseService.method.listLeases, { repo }, opts),
        rpc.call(ChangeService.method.listChanges, { repo, state: "CHANGE_STATE_OPEN", page: { pageSize: 200 } }, opts),
      ])
    : [undefined, undefined];
  return mapLimit(members, 4, async (p): Promise<T.AgentSummary> => {
    const history = repo ? list((await rpc.call(QueryService.method.log, { repo, from: { expr: at }, author: p.id, includeMeta: true, page: { pageSize: 200 } }, opts)).changesets) : [];
    const mine = history.filter((c) => c.meta?.creator === p.id);
    const week = mine.filter((c) => now - Date.parse(str(c.meta?.authorTime?.time)) < 7 * DAY);
    const latest = mine.find((c) => c.meta?.provenance?.model);
    const held = list(leases?.leases).filter((l) => l.holder?.id === p.id).length;
    const last = mine[0]?.meta?.authorTime?.time;
    return {
      principal: p,
      owner: owners.get(str(p.ownerId)),
      purpose: "",
      status: held > 0 || (last && now - Date.parse(last) < ACTIVE_WINDOW) ? "active" : "idle",
      model: str(latest?.meta?.provenance?.model),
      session: str(latest?.meta?.provenance?.session),
      openChanges: list(open?.changes).filter((c) => c.author?.id === p.id).length,
      landed7d: week.length,
      spend7dUsdMicros: String(week.reduce((n, c) => n + num(c.meta?.provenance?.cost?.usdMicros), 0)),
      activeLeases: held,
      lastSeen: last,
      registered: false,
    };
  });
}

async function listAgents(rpc: Rpc, org: string, repo: T.RepoRef | undefined, principalIds: string[] = [], opts?: CallOptions): Promise<T.AgentSummary[]> {
  const windowStart = new Date(Date.now() - 7 * DAY).toISOString();
  const allAgents = await allRows(async (page) => {
    const response = await rpc.call(AgentService.method.listAgentPrincipals, { org, windowStart, principalIds, page }, opts);
    return { rows: response.agents, next: response.page?.nextPageToken };
  });
  const idOf = (a: AgentPrincipalSummaryJson) => str((a.principal as PrincipalJson | undefined)?.id);
  const agents = allAgents.filter((a) => !principalIds.length || principalIds.includes(idOf(a)));
  const owners = await principals(rpc, agents.map((a) => str((a.principal as PrincipalJson | undefined)?.ownerId)), opts);
  const registered = agents.map((a) => agentView(a, owners.get(str((a.principal as PrincipalJson | undefined)?.ownerId))));
  const rest = await unregisteredAgents(rpc, org, repo, new Set(agents.map(idOf)), principalIds, opts);
  return [...registered, ...rest];
}

/**
 * Provenance of an agent without a registry definition (ListAgentWorkItems answers NOT_FOUND):
 * its landed changesets from the repo's history and the heads of its change requests.
 */
async function provenanceFromRepo(rpc: Rpc, r: T.ListProvenanceRequest, repo: T.RepoRef, opts?: CallOptions): Promise<T.ListProvenanceResponse> {
  const at = await defaultRef(rpc, repo, opts);
  const [log, crs] = await Promise.all([
    rpc.call(QueryService.method.log, { repo, from: { expr: at }, author: r.principalId, includeMeta: true, page: { pageSize: 100 } }, opts),
    rpc.call(ChangeService.method.listChanges, { repo, authorId: r.principalId, page: { pageSize: 100 } }, opts),
  ]);
  const name = "fullName" in repo && repo.fullName ? repo.fullName : str(repo.id);
  const landed = list(log.changesets).filter((c) => c.meta?.creator === r.principalId);
  const open = list(crs.changes).filter((c) => c.state !== "CHANGE_STATE_LANDED");
  const heads = open.flatMap((c) => list(c.revisions).map((rev) => ({ cr: c, n: num(rev.n), head: rev.head as T.ObjectId | undefined })));
  const got = heads.length ? list((await rpc.call(QueryService.method.getChangesets, { repo, ids: heads.flatMap((h) => (h.head ? [h.head] : [])), includeMeta: true }, opts)).changesets) : [];
  const byId = new Map(got.map((c) => [objectIdText(c.id as T.ObjectId | undefined), c]));
  const record = (c: (typeof got)[number], extra: { message: string; changeRequestId: string; landed: boolean }): T.ProvenanceRecord => ({
    changeset: c.id as T.ObjectId | undefined,
    repo: name,
    ...extra,
    provenance: (c.meta?.provenance as T.Provenance | undefined) ?? { principalChain: [], runtime: "", model: "", session: "", promptDigest: "", checksRun: [] },
    createdAt: c.meta?.authorTime?.time,
  });
  const records = [
    ...heads.flatMap((h) => {
      const c = byId.get(objectIdText(h.head));
      return c ? [record(c, { message: str(h.cr.title) + " (r" + h.n + ")", changeRequestId: str(h.cr.id), landed: false })] : [];
    }),
    ...landed.map((c) => record(c, { message: str(c.meta?.message).split("\n")[0] ?? "", changeRequestId: "", landed: true })),
  ].sort((a, b) => Date.parse(str(b.createdAt)) - Date.parse(str(a.createdAt)));
  return { records, page: { nextPageToken: "" } };
}

async function listProvenance(rpc: Rpc, r: T.ListProvenanceRequest, opts?: CallOptions): Promise<T.ListProvenanceResponse> {
  let res;
  try {
    res = await rpc.call(AgentService.method.listAgentWorkItems, { principalId: r.principalId, page: r.page }, opts);
  } catch (err) {
    if (r.repo && err instanceof BeeError && err.code === "not_found") return provenanceFromRepo(rpc, r, r.repo, opts);
    throw err;
  }
  const items = list(res.items).filter((i) => i.subject?.kind === "SUBJECT_KIND_CHANGESET");
  const repos = [...new Set(items.map((i) => str(i.subject?.repo?.fullName ?? i.subject?.repo?.id)))];
  const perRepo = await mapLimit(repos, 4, async (name) => {
    const repo: T.RepoRef = name.includes("/") ? { fullName: name } : { id: name };
    const ids = items.filter((i) => str(i.subject?.repo?.fullName ?? i.subject?.repo?.id) === name).flatMap((i) => parseObjectId(str(i.subject?.id)) ?? []);
    const [cs, crs] = await Promise.all([
      rpc.call(QueryService.method.getChangesets, { repo, ids, includeMeta: true }, opts),
      rpc.call(ChangeService.method.listChanges, { repo, authorId: r.principalId, page: { pageSize: 200 } }, opts),
    ]);
    // A changeset that heads a revision belongs to that change request.
    const heads = new Map<string, { id: string; n: number }>();
    for (const c of list(crs.changes)) for (const rev of list(c.revisions)) heads.set(objectIdText(rev.head as T.ObjectId | undefined), { id: str(c.id), n: num(rev.n) });
    return { cs: new Map(list(cs.changesets).map((c) => [objectIdText(c.id as T.ObjectId | undefined), c])), heads };
  });
  const records = items.map((i): T.ProvenanceRecord => {
    const name = str(i.subject?.repo?.fullName ?? i.subject?.repo?.id);
    const at = perRepo[repos.indexOf(name)];
    const id = str(i.subject?.id);
    const meta = at?.cs.get(id)?.meta;
    const cr = at?.heads.get(id);
    const provenance = (meta?.provenance as T.Provenance | undefined) ?? {
      principalChain: [],
      runtime: "",
      model: "",
      session: str(i.session),
      promptDigest: "",
      checksRun: [],
      cost: i.cost as T.Cost | undefined,
    };
    return {
      changeset: parseObjectId(id) ?? undefined,
      repo: name,
      message: str(i.title) + (cr ? " (r" + cr.n + ")" : ""),
      changeRequestId: cr?.id ?? "",
      landed: i.state === "landed",
      provenance,
      createdAt: i.createdAt,
    };
  });
  return { records, page: pageOf(res.page) };
}

function instructionsRequest(r: T.ResolveAgentInstructionsRequest) {
  return { repo: r.repo, ...(r.revision ? { revision: { expr: r.revision } } : {}) };
}

export const agentOps = {
  AgentService: {
    ResolveAgentInstructions: {
      wire: (r) => wire(AgentService.method.resolveAgentInstructions, instructionsRequest(r)),
      run: async (rpc, r, opts) => {
        const res = await rpc.call(AgentService.method.resolveAgentInstructions, instructionsRequest(r), opts);
        return {
          files: list(res.files).map((f): T.AgentInstructionFile => ({
            path: pathText(str(f.path)),
            source: f.source ?? "AGENT_INSTRUCTION_SOURCE_UNSPECIFIED",
            name: str(f.name),
            appliesTo: list(f.appliesTo),
            content: str(f.content),
            includes: list(f.includes).map(pathText),
          })),
          combined: str(res.combined),
          changeset: res.changeset as T.ObjectId | undefined,
        };
      },
    },
    GetAgentBudget: pass(AgentService.method.getAgentBudget),
    SetAgentBudget: pass(AgentService.method.setAgentBudget),
    GetAgentRun: pass(AgentService.method.getAgentRun),
    ListAgentRuns: pass(AgentService.method.listAgentRuns),
    ListAgentRunEvents: pass(AgentService.method.listAgentRunEvents),
    SendAgentRunMessage: pass(AgentService.method.sendAgentRunMessage),
    CancelAgentRun: pass(AgentService.method.cancelAgentRun),
    RetryAgentRun: pass(AgentService.method.retryAgentRun),
    ListAgents: {
      wire: (r) => wire(AgentService.method.listAgentPrincipals, { org: r.org }),
      run: async (rpc, r, opts) => ({ agents: await listAgents(rpc, r.org, r.repo, [], opts) }),
    },
    GetAgent: {
      wire: (r) => wire(AgentService.method.listAgentPrincipals, { org: r.org, principalIds: [r.principalId] }),
      run: async (rpc, r, opts) => {
        const a = (await listAgents(rpc, r.org, r.repo, [r.principalId], opts))[0];
        if (!a) throw notFound("NOT_FOUND", "no agent " + r.principalId);
        return a;
      },
    },
    ListTokens: {
      wire: (r) => wire(AuthService.method.listTokens, { principalId: r.principalId, includeRevoked: true }),
      run: async (rpc, r, opts) => {
        const tokens = await allRows(async (page) => {
          const response = await rpc.call(AuthService.method.listTokens, { principalId: r.principalId, includeRevoked: true, page }, opts);
          return { rows: response.tokens, next: response.page?.nextPageToken };
        });
        return {
          tokens: tokens.map((t): T.TokenInfo => ({
            tokenId: str(t.tokenId),
            parentTokenId: str(t.parentTokenId),
            principalId: str(t.principalId),
            scope: {
              repos: list(t.scope?.repos),
              readPaths: list(t.scope?.readPaths),
              writePaths: list(t.scope?.writePaths),
              refs: list(t.scope?.refs),
              actions: list(t.scope?.actions),
              session: str(t.scope?.session),
              expiresAt: t.scope?.expiresAt,
              budget: t.scope?.budget as T.Budget | undefined,
            },
            used: t.used as T.Budget | undefined,
            createdAt: t.createdAt,
            revoked: !!t.revoked,
            label: str(t.description),
          })),
        };
      },
    },
    ListProvenance: {
      wire: (r) => wire(AgentService.method.listAgentWorkItems, { principalId: r.principalId, page: r.page }),
      run: (rpc, r, opts) => listProvenance(rpc, r, opts),
    },
  },
} satisfies PartialOps<"AgentService">;
