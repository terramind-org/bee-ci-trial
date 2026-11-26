/**
 * The pull request page as data (docs/spec/web-ui.md §3, "A pull request"): which decisions the
 * reader may take, who did the work, and the whole page as labelled lines of text for the
 * agent's view. Pure functions of what the page loads, so the person's page and the agent's
 * cannot say different things, and each one is unit tested.
 */
import type { AgentSession, ChangeRequest, CheckRun, Comment, DiffItem, Evidence, LandingEvaluation, Principal, Provenance, QueueEntry, Review, BriefChecklist } from "./api/types";
import { askStates } from "./asks";
import { riskSentence } from "./change-page";
import { checkState, checksSentence, listing } from "./checkwords";
import { titleOf } from "./evidence-view";
import { duration, usd } from "./format";
import { worthALook } from "./hunks";
import { digestText, mergeVerb, otherHolds, whyPerson } from "./pulls";
import type { Status } from "./status";
import { runtimeName, teamOf } from "./team";
import { changeSize, fromDiff } from "./whatchanged";

/* ------------------------------------------------------------------ decisions */

export interface PullDecisions {
  /** The screen's one solid button, or none. */
  primary: "approve-and-merge" | "merge" | null;
  /** The reader may send it back with a note. */
  canRequestChanges: boolean;
  canClose: boolean;
  /** One sentence beside the buttons: what happens next, or why there is nothing to press. */
  note: string;
}

export interface DecisionInput {
  change: Pick<ChangeRequest, "state" | "author" | "revisions">;
  viewer: Principal | null;
  reviews: Review[];
  landing?: Pick<LandingEvaluation, "allowed" | "missing">;
  queue?: Pick<QueueEntry, "position"> | null;
}

/**
 * What the reader may decide. A person who did not write it approves and merges in one press:
 * the approval is given, and it merges if the rules then allow it. Its author, or someone who
 * already approved the latest revision, merges once the rules allow. Nobody decides on a merged
 * or closed pull request, and a signed-out reader is asked to sign in.
 */
export function decisionsFor({ change, viewer, reviews, landing, queue }: DecisionInput): PullDecisions {
  const none = { primary: null, canRequestChanges: false, canClose: false } as const;
  if (change.state === "CHANGE_STATE_LANDED") return { ...none, note: "It merged. Nothing needs you here." };
  if (change.state === "CHANGE_STATE_CLOSED") return { ...none, note: "It was closed without merging." };
  if (!viewer) return { ...none, note: "Sign in to approve it, request changes or close it." };
  const latest = change.revisions.at(-1)?.n ?? 0;
  const mine = change.author?.id === viewer.id;
  const approved = reviews.some((r) => r.reviewer?.id === viewer.id && r.verdict === "VERDICT_APPROVE" && r.revision === latest);
  const base = { canRequestChanges: !mine, canClose: true };
  if (queue) return { ...base, primary: null, note: "It is in the merge queue at position " + queue.position + ". The rules are checked again at the front." };
  if (!mine && !approved) return { ...base, primary: "approve-and-merge", note: landing?.allowed ? "The rules already allow merging it." : "Your approval is recorded first; it merges if the rules then allow it." };
  if (landing?.allowed) return { ...base, primary: "merge", note: mine ? "The rules allow merging it." : "You approved revision " + latest + ". The rules allow merging it." };
  return { ...base, primary: null, note: mine ? "You opened this one. Someone else has to approve it." : "You approved revision " + latest + ". It still waits on something else." };
}

/* ------------------------------------------------------------------ who did it */

export interface WhoDidIt {
  /** The author's name. */
  author: string;
  agent: boolean;
  /** The agent's id, for the link to its page. */
  agentId?: string;
  /** The person who answers for it; absent when the page could not name one. */
  person?: string;
  models: string[];
  runtimes: string[];
  costMicros: number;
  sessions: number;
  revisions: number;
  /** Every distinct prompt digest the head's provenance and the sessions carry, as hex. */
  promptDigests: string[];
  /** Checks the agent says it ran itself before pushing, from the head's provenance. */
  ownChecks: string[];
  elapsedMs: number;
}

export interface WhoInput {
  change: Pick<ChangeRequest, "author" | "createdAt" | "revisions" | "brief">;
  answers?: Principal | null;
  sessions?: AgentSession[];
  /** Provenance of the latest revision's head, when it has any. */
  provenance?: Provenance;
  reviews?: Review[];
  comments?: Comment[];
}

const distinct = (items: string[]) => [...new Set(items.filter(Boolean))];

/**
 * Who did the work and what it cost, from the sessions the server links to the pull request and
 * the provenance on its head. A line with no data stays empty and the screen leaves it out.
 * Cost is the sessions' when there are any (they cover every revision), else the head's own.
 */
export function whoDidIt({ change, answers, sessions = [], provenance, reviews, comments }: WhoInput): WhoDidIt {
  const author = change.author;
  const agent = author?.kind === "PRINCIPAL_KIND_AGENT";
  const team = teamOf({ change, reviews, comments, sessions });
  const sessionCost = sessions.reduce((n, s) => n + (Number(s.cost?.usdMicros ?? 0) || 0), 0);
  return {
    author: author?.display || author?.id || "",
    agent,
    agentId: agent ? author?.id : undefined,
    person: answers?.display || undefined,
    models: distinct([...sessions.map((s) => s.model), provenance?.model ?? ""]),
    runtimes: distinct([...sessions.map((s) => runtimeName(s.runtime)), runtimeName(provenance?.runtime ?? ""), agent && !sessions.length && !provenance ? runtimeName(author?.runtime ?? "") : ""]),
    costMicros: sessions.length ? sessionCost : Number(provenance?.cost?.usdMicros ?? 0) || 0,
    sessions: sessions.length,
    revisions: change.revisions.length,
    promptDigests: distinct([digestText(provenance?.promptDigest), ...sessions.map((s) => digestText(s.promptDigest))]),
    ownChecks: (provenance?.checksRun ?? []).map((c) => c.name + " " + c.result),
    elapsedMs: team.elapsedMs,
  };
}

/* ------------------------------------------------------------------ the agent's view */

/** A group of the agent's view, as plain strings (`components/bee/page-facts.tsx` renders it). */
export interface TextGroup {
  title: string;
  facts?: ({ label: string; value: string } | null)[];
  lines?: string[];
  empty?: string;
}

export interface CommitText {
  short: string;
  line: string;
}

export interface PullFactsInput {
  repo: string;
  change: ChangeRequest;
  status: Status;
  branch: string;
  answers?: Principal | null;
  /** The commits it would merge, when the server lists them. */
  commits?: { known: boolean; lines: string[] };
  landing?: LandingEvaluation;
  owners?: Record<string, string[]>;
  checks: CheckRun[];
  evidence: Evidence[];
  diff: DiffItem[];
  reviews: Review[];
  comments: Comment[];
  checklist?: BriefChecklist;
  who: WhoDidIt;
  decisions: PullDecisions;
  /** Path of this pull request's page, and of its full text for agents. */
  href: string;
  changeMdHref: string;
}

const fact = (label: string, value: string | undefined | null | false) => (value ? { label, value } : null);

export function pullFacts(i: PullFactsInput): TextGroup[] {
  const cr = i.change;
  const latest = cr.revisions.at(-1);
  const author = cr.author;
  const files = fromDiff(i.diff);
  const picks = worthALook(i.diff, { highlights: cr.brief?.highlights });
  const asks = askStates(cr.brief?.asks ?? [], i.comments);
  const owed = (i.checklist?.items ?? []).filter((x) => !x.satisfied).map((x) => x.detail);
  const missing = i.landing?.missing ?? [];
  const who = i.who;
  const groups: TextGroup[] = [
    {
      title: "facts",
      facts: [
        fact("pull request", i.repo + " #" + cr.number),
        fact("title", cr.title),
        fact("state", i.status.sentence),
        fact("author", who.author + (who.agent ? " (agent)" : " (person)")),
        fact("answers for it", who.person),
        fact("merge", (who.author || "Someone") + " " + mergeVerb(cr.state, i.commits?.known ? i.commits.lines.length : undefined) + " " + cr.targetRef + (i.branch ? " from " + i.branch : "")),
        fact("revision", latest ? "r" + latest.n + (latest.diffClass ? " · " + latest.diffClass : "") : ""),
        fact("outcome", cr.brief?.outcome),
        fact("risk", riskSentence(latest?.diffClass ?? "", cr.brief)),
        fact("opened", cr.createdAt),
        fact("stacked on", cr.dependsOn),
        fact("page", i.href),
        fact("full text", i.changeMdHref),
      ],
    },
  ];
  if (cr.state === "CHANGE_STATE_OPEN") {
    groups.push({
      title: "why a person",
      lines: [
        ...whyPerson(missing, i.owners),
        ...otherHolds(missing),
        ...missing.map((m) => "missing: " + m),
        ...(i.landing?.satisfied ?? []).map((m) => "met: " + m),
        ...asks.map((a) => "asked: " + a.ask.question + " (" + a.ask.options.join(" / ") + ")" + (a.answer !== undefined ? " · decided: " + a.answer : " · not decided")),
        ...(owed.length ? [(author?.kind === "PRINCIPAL_KIND_AGENT" ? "The agent" : "The author") + " still owes: " + listing(owed) + "."] : []),
        i.decisions.note,
      ],
      empty: "No rule sends this to a person.",
    });
  }
  groups.push(
    {
      title: "checks",
      lines: [checksSentence(i.checks), ...i.checks.map((c) => c.name + ": " + checkState(c) + (c.summary ? " · " + c.summary : "")), ...i.evidence.map((e) => "evidence: " + titleOf(e) + " · r" + e.revision + (e.stale ? " · stale" : ""))],
    },
    {
      title: "files changed",
      lines: [...(files.length ? [changeSize(files)] : []), ...picks.map((p) => "worth a look: " + p.path + " lines " + p.hunk.newStart + "–" + (p.hunk.newStart + Math.max(p.hunk.newLen - 1, 0)) + " · " + p.reason), ...files.map((f) => f.path + " · " + (f.binary ? "binary" : "+" + f.added + " −" + f.removed) + " · " + (f.change === "modified" ? f.diffClass : f.change))],
      empty: "This revision changes no files.",
    },
    {
      title: "commits",
      lines: i.commits?.lines ?? [],
      empty: "The server did not list its commits.",
    },
    {
      title: "who did it",
      facts: [
        fact(who.agent ? "agent" : "person", who.author),
        who.agent ? fact("for", who.person) : null,
        fact("model", who.models.join(", ")),
        fact("runtime", who.runtimes.join(", ")),
        fact("cost", who.costMicros ? usd(who.costMicros) : ""),
        fact("sessions", who.sessions ? String(who.sessions) : ""),
        fact("revisions", String(who.revisions)),
        fact("prompt digest", who.promptDigests.join(", ")),
        fact("checks it ran itself", who.ownChecks.join(", ")),
        fact("time", who.elapsedMs >= 60_000 ? duration(who.elapsedMs) + " from opening to the latest step" : ""),
      ],
    },
    {
      title: "conversation",
      lines: [
        ...i.reviews.map((r) => (r.reviewer?.display || r.reviewer?.id || "Someone") + " " + (r.verdict === "VERDICT_APPROVE" ? "approved" : r.verdict === "VERDICT_REQUEST_CHANGES" ? "requested changes on" : "commented on") + " r" + r.revision + (r.body ? ": " + r.body.split("\n")[0] : "")),
        ...i.comments.map((c) => (c.author?.display || c.author?.id || "Someone") + ": " + (c.body.split("\n")[0] ?? "")),
      ],
      empty: "No reviews or comments yet.",
    },
  );
  return groups;
}
