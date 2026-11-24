/**
 * "Who did it, what did it cost?" (docs/spec/web-ui.md §3): the agents and people behind a change
 * request with their roles, the models the agents ran, what their sessions cost and how long the
 * work took. Built from what the change page already loads: the change, its reviews and
 * conversation, and GetSessionsForChange. Elapsed time is measured between recorded timestamps,
 * never against the clock.
 */
import type { AgentSession, ChangeRequest, Comment, Principal, Review } from "./api/types";
import { listing } from "./checkwords";

const RUNTIMES: Record<string, string> = { "claude-code": "Claude Code", codex: "Codex", cursor: "Cursor", aider: "Aider", "gemini-cli": "Gemini CLI" };

/** "claude-code/3.2" reads as "Claude Code"; an unknown runtime is title-cased from its slug. */
export function runtimeName(runtime: string): string {
  const base = (runtime.split("/")[0] ?? "").trim().toLowerCase();
  if (!base) return "";
  return RUNTIMES[base] ?? base.split(/[-_\s]+/).filter(Boolean).map((w) => w.charAt(0).toUpperCase() + w.slice(1)).join(" ");
}

export type Role = "author" | "brief" | "approved" | "requested changes" | "reviewed" | "commented" | "ran a session";

const ROLE_ORDER: Role[] = ["author", "brief", "ran a session", "approved", "requested changes", "reviewed", "commented"];

const ROLE_WORDS: Record<Role, string> = {
  author: "Wrote the change",
  brief: "Wrote the brief",
  approved: "Approved",
  "requested changes": "Asked for changes",
  reviewed: "Reviewed",
  commented: "Commented",
  "ran a session": "Ran a session",
};

export interface Member {
  id: string;
  /** Undefined when only an id is known (a session whose agent the page did not load). */
  principal?: Principal;
  agent: boolean;
  roles: Role[];
  /** "Wrote the change and the brief" style, built from roles. */
  rolesText: string;
  models: string[];
  sessions: number;
  costMicros: number;
  inputTokens: number;
  outputTokens: number;
  /** From this member's first to last recorded act, in ms; 0 with a single act. */
  elapsedMs: number;
}

export interface Team {
  members: Member[];
  /** Every model any session ran, sorted. */
  models: string[];
  costMicros: number;
  /** From the change's creation to the last recorded act on it, in ms. */
  elapsedMs: number;
}

export interface TeamInput {
  change: Pick<ChangeRequest, "author" | "createdAt" | "revisions" | "brief">;
  reviews?: Review[];
  comments?: Comment[];
  sessions?: AgentSession[];
  /** Principals the page knows, to name session agents. */
  directory?: Principal[];
}

function rolesText(roles: Role[]): string {
  const words = roles.map((r) => ROLE_WORDS[r]);
  if (roles.includes("author") && roles.includes("brief")) {
    const rest = roles.filter((r) => r !== "author" && r !== "brief").map((r) => ROLE_WORDS[r].toLowerCase());
    return listing(["Wrote the change and the brief", ...rest]);
  }
  return listing(words.map((w, i) => (i === 0 ? w : w.toLowerCase())));
}

const ms = (iso: string | undefined) => (iso ? Date.parse(iso) : NaN);

export function teamOf({ change, reviews = [], comments = [], sessions = [], directory = [] }: TeamInput): Team {
  const known = new Map<string, Principal>();
  for (const p of [change.author, change.brief?.author, ...reviews.map((r) => r.reviewer), ...comments.map((c) => c.author), ...directory]) if (p?.id && !known.has(p.id)) known.set(p.id, p);

  type Acc = { roles: Set<Role>; times: number[]; models: Set<string>; sessions: number; cost: number; inTok: number; outTok: number };
  const acc = new Map<string, Acc>();
  const order: string[] = [];
  const touch = (id: string | undefined, role: Role, at?: string) => {
    if (!id) return;
    let a = acc.get(id);
    if (!a) {
      acc.set(id, (a = { roles: new Set(), times: [], models: new Set(), sessions: 0, cost: 0, inTok: 0, outTok: 0 }));
      order.push(id);
    }
    a.roles.add(role);
    const t = ms(at);
    if (Number.isFinite(t)) a.times.push(t);
  };

  touch(change.author?.id, "author", change.createdAt);
  for (const r of change.revisions) touch(change.author?.id, "author", r.createdAt);
  if (change.brief?.author) touch(change.brief.author.id, "brief", change.brief.updatedAt);
  for (const s of [...sessions].sort((a, b) => (a.createdAt ?? "").localeCompare(b.createdAt ?? "") || a.session.localeCompare(b.session))) {
    touch(s.agentPrincipalId, "ran a session", s.createdAt);
    const a = acc.get(s.agentPrincipalId);
    if (!a) continue;
    a.sessions++;
    if (s.model) a.models.add(s.model);
    a.cost += Number(s.cost?.usdMicros ?? 0) || 0;
    a.inTok += Number(s.cost?.inputTokens ?? 0) || 0;
    a.outTok += Number(s.cost?.outputTokens ?? 0) || 0;
  }
  for (const r of reviews) {
    const role: Role = r.verdict === "VERDICT_APPROVE" ? "approved" : r.verdict === "VERDICT_REQUEST_CHANGES" ? "requested changes" : "reviewed";
    touch(r.reviewer?.id, role, r.createdAt);
  }
  for (const c of comments) touch(c.author?.id, "commented", c.createdAt);

  const members: Member[] = order.map((id) => {
    const a = acc.get(id) as Acc;
    const principal = known.get(id);
    // A person who approved and then asked for changes keeps both; "reviewed" adds nothing then.
    let roles = ROLE_ORDER.filter((r) => a.roles.has(r));
    if (roles.includes("approved") || roles.includes("requested changes")) roles = roles.filter((r) => r !== "reviewed");
    // Someone who wrote the change is not also listed as having only commented on it.
    if (roles.length > 1) roles = roles.filter((r) => r !== "commented" || roles.every((x) => x === "commented"));
    const agent = principal ? principal.kind === "PRINCIPAL_KIND_AGENT" : a.sessions > 0;
    return {
      id,
      principal,
      agent,
      roles,
      rolesText: rolesText(roles),
      models: [...a.models].sort(),
      sessions: a.sessions,
      costMicros: a.cost,
      inputTokens: a.inTok,
      outputTokens: a.outTok,
      elapsedMs: a.times.length > 1 ? Math.max(...a.times) - Math.min(...a.times) : 0,
    };
  });
  // Agents first (they did the work), then people; each in the order they first appeared.
  members.sort((x, y) => Number(y.agent) - Number(x.agent));

  const all = [...acc.values()].flatMap((a) => a.times);
  const start = ms(change.createdAt);
  const end = all.length ? Math.max(...all) : NaN;
  return {
    members,
    models: [...new Set(members.flatMap((m) => m.models))].sort(),
    costMicros: members.reduce((n, m) => n + m.costMicros, 0),
    elapsedMs: Number.isFinite(start) && Number.isFinite(end) && end > start ? end - start : 0,
  };
}
