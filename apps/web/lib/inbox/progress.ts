/**
 * Where an agent's current piece of work is, as one of four calm steps: editing, checks,
 * review, merge. Pure, so the home and repo pages agree and tests can pin it.
 */
import type { AgentSummary, FeedEvent, Lease } from "@/lib/api/types";
import { requiredPending, type ChangeFacts } from "./needs";
import { checkState } from "./words";

export const STEPS = ["Editing", "Checks", "Review", "Merge"] as const;

export interface Progress {
  /** Index into STEPS. */
  step: number;
  /** The step's state in words, e.g. "Checks running". */
  label: string;
  tone: "calm" | "stuck";
}

export interface AgentWork {
  agent: AgentSummary;
  leases: { repo: string; lease: Lease }[];
  /** The agent's most recent open change request, with its facts. */
  change?: ChangeFacts;
  progress: Progress | null;
}

export function progressOf(change: ChangeFacts | undefined, editing: boolean): Progress | null {
  if (change) {
    const states = change.checks.map(checkState);
    if (states.includes("failed")) return { step: 1, label: "Stuck on a check", tone: "stuck" };
    if (requiredPending(change.landing)) return { step: 1, label: "Checks running", tone: "calm" };
    if (change.queued) return { step: 3, label: "In the merge queue", tone: "calm" };
    if (change.landing.allowed) return { step: 3, label: "Ready to merge", tone: "calm" };
    return { step: 2, label: "Waiting for review", tone: "calm" };
  }
  return editing ? { step: 0, label: "Editing", tone: "calm" } : null;
}

/**
 * One row per active agent, busiest first: the agent's newest open change (or its lease) and
 * where that work stands. Agents that hold nothing and have nothing open are left out.
 */
export function agentWork(agents: AgentSummary[], leases: { repo: string; lease: Lease }[], facts: ChangeFacts[]): AgentWork[] {
  const rows: AgentWork[] = [];
  for (const agent of agents) {
    if (agent.status !== "active" || agent.principal.kind !== "PRINCIPAL_KIND_AGENT") continue;
    const id = agent.principal.id;
    const held = leases.filter((l) => l.lease.holder?.id === id);
    const change = facts.filter((f) => f.change.author?.id === id).sort((a, b) => Date.parse(b.change.revisions.at(-1)?.createdAt ?? "") - Date.parse(a.change.revisions.at(-1)?.createdAt ?? ""))[0];
    const progress = progressOf(change, held.length > 0);
    if (!progress) continue;
    rows.push({ agent, leases: held, change, progress });
  }
  // Stuck work first, then the earliest step (most still to do), then by name for a stable order.
  return rows.sort(
    (a, b) => Number(b.progress?.tone === "stuck") - Number(a.progress?.tone === "stuck") || (a.progress?.step ?? 0) - (b.progress?.step ?? 0) || a.agent.principal.display.localeCompare(b.agent.principal.display),
  );
}

export interface DoneGroup {
  repo: string;
  items: FeedEvent[];
}

/** Merges in the caller's repos since `since`, grouped by repo, busiest repo first. */
export function doneSince(events: FeedEvent[], repos: Set<string>, since: number): DoneGroup[] {
  const groups = new Map<string, FeedEvent[]>();
  for (const e of events) {
    const repo = e.subject?.repo && "fullName" in e.subject.repo ? e.subject.repo.fullName : undefined;
    if (e.kind !== "land" || !repo || !repos.has(repo)) continue;
    if (Date.parse(e.createdAt ?? "") < since) continue;
    groups.set(repo, [...(groups.get(repo) ?? []), e]);
  }
  return [...groups.entries()].map(([repo, items]) => ({ repo, items })).sort((a, b) => b.items.length - a.items.length || a.repo.localeCompare(b.repo));
}

/** The quoted title inside a land summary: `X landed "Title" in owner/repo` → `Title`. */
export function landedTitle(e: FeedEvent): string {
  return /“([^”]+)”|"([^"]+)"/.exec(e.summary)?.slice(1).find(Boolean) ?? e.summary;
}

export const DAY = 86_400_000;

/** The Inbox's "This week" margin. */
export interface Week {
  merged: number;
  /** Of `merged`, how many an agent landed. */
  byAgents: number;
  /** Open change requests across the caller's repos. */
  open: number;
  /** What the caller's agents spent over the last seven days, in micro-dollars. */
  spendMicros: number;
}

export function weekOf(events: FeedEvent[], repos: Set<string>, now: number, agents: AgentSummary[], open: number): Week {
  const merged = doneSince(events, repos, now - 7 * DAY).flatMap((g) => g.items);
  return {
    merged: merged.length,
    byAgents: merged.filter((e) => e.actor?.kind === "PRINCIPAL_KIND_AGENT").length,
    open,
    spendMicros: agents.reduce((n, a) => n + Number(a.spend7dUsdMicros || 0), 0),
  };
}
