/**
 * What needs the signed-in person: a pure classification of one open change request from the
 * facts the pages load (landing evaluation, checks, reviews, path owners, merge queue), plus
 * agent-written posts on merged change requests that wait for their approval. Only work this
 * person can act on is returned; everything else is somebody else's to move.
 *
 * `needsOf` is the one entry point: the Needs you list and the rail's count both call it, so
 * the two never disagree.
 */
import type { ChangeRequest, CheckRun, LandingEvaluation, Post, Principal, Review, SafetyAttention } from "@/lib/api/types";
import { answerableFor, whyPerson } from "../pulls";
import { repoHref } from "../routes";
import { checkState, isAgent, list, missingInWords, parseApproval, riskOf } from "./words";

export interface ChangeFacts {
  repo: string;
  change: ChangeRequest;
  landing: LandingEvaluation;
  checks: CheckRun[];
  reviews: Review[];
  /** Owners (display names) of each path an `approval:owner:<path>` rule names. */
  owners: Record<string, string[]>;
  /** The change is already in the merge queue. */
  queued: boolean;
  /** The branch it comes from: the ref at its head. Empty when no ref points there. */
  branch?: string;
}

export type NeedKind = "review" | "failing" | "merge" | "blocked" | "post";

export interface Need {
  kind: NeedKind;
  repo: string;
  change: ChangeRequest;
  /** Who is asking: the change's author. */
  who?: Principal;
  /** The caller wrote the change. */
  mine: boolean;
  /** The caller asked for changes on an earlier revision and the author has since revised it. */
  revisedAfterYou: boolean;
  /** Names of failing checks, for "failing". */
  failing: string[];
  /** Outstanding landing rules, in words. */
  reasons: string[];
  /** The same rules as the server lists them (`LandingEvaluation.missing`). */
  missing: string[];
  /** Owners of each path an owner rule names, when the scan read them. */
  owners: Record<string, string[]>;
  /** The branch it comes from, when a ref still points at its head. */
  branch?: string;
  /** The person who answers for the work: the author, or the agent's owner. */
  answers?: Principal;
  /** When the latest revision was pushed; for "post", when the change merged. */
  at?: string;
  /** The agent's post waiting for approval, for "post". */
  post?: Post;
}

/** A tripwire or an agent's concern addressed to the signed-in person. */
export interface SafetyNeed {
  id: string;
  kind: "tripwire" | "concern";
  repo: string;
  title: string;
  reason: string;
  href: string;
  answers?: Principal;
  at?: string;
}

export function safetyNeeds(repo: string, items: SafetyAttention[], me: Principal): SafetyNeed[] {
  return items.flatMap((item) => {
    const { work, tripwire, concern } = item;
    if (!work?.id || item.acknowledgedAt || item.answerable?.id !== me.id || (!tripwire && !concern)) return [];
    const subject = work.kind === "SAFETY_WORK_KIND_SWARM" ? "Swarm" : "Agent run";
    const params = new URLSearchParams({ work: work.id, kind: work.kind });
    return [{
      id: item.id,
      kind: tripwire ? "tripwire" : "concern",
      repo,
      title: tripwire
        ? tripwire.pausedSwarm ? "Swarm paused" : tripwire.stoppedRunIds.length ? "Agent run stopped" : subject + " needs attention"
        : (concern?.raisedBy?.display ?? "An agent") + " raised a concern",
      reason: tripwire?.reason ?? concern?.text ?? "",
      href: repoHref(repo, "safety") + "?" + params,
      answers: item.answerable,
      at: item.createdAt,
    }];
  });
}

/** Within one level of risk: what blocks an agent first, then what only waits on you. */
export const KIND_ORDER: Record<NeedKind, number> = { failing: 0, review: 1, blocked: 2, merge: 3, post: 4 };

export const ACTION: Record<NeedKind, string> = { review: "Review", failing: "Open", merge: "Merge", blocked: "Open", post: "Read post" };

export function classify(f: ChangeFacts, me: Principal): Need | null {
  const { change, landing, checks, reviews } = f;
  const mine = change.author?.id === me.id;
  const latest = change.revisions.at(-1);
  const base = { repo: f.repo, change, who: change.author, mine, at: latest?.createdAt ?? change.createdAt, reasons: landing.missing.map(missingInWords), missing: landing.missing, owners: f.owners, branch: f.branch || undefined };
  const failing = checks.filter((c) => checkState(c) === "failed").map((c) => c.name);
  // Only checks the policy requires hold a change back; an optional e2e run does not.
  const pending = requiredPending(landing);

  // A failing check stops an agent, or the caller's own change; a colleague fixes their own.
  if (failing.length) return mine || isAgent(change.author) ? { ...base, kind: "failing", failing, revisedAfterYou: false } : null;
  // Still testing: it is in progress, not waiting on anyone yet.
  if (pending) return null;

  const myReviews = reviews.filter((r) => r.reviewer?.id === me.id);
  const approvedLatest = myReviews.some((r) => r.verdict === "VERDICT_APPROVE" && r.revision === (latest?.n ?? 0));
  const askedEarlier = myReviews.some((r) => r.verdict === "VERDICT_REQUEST_CHANGES" && r.revision < (latest?.n ?? 0));
  const canApprove = landing.missing.some((m) => {
    const a = parseApproval(m);
    if (!a || mine) return false;
    if (a.from === "owner") return (f.owners[a.scope] ?? []).includes(me.display);
    return true;
  });
  if (canApprove && !approvedLatest) return { ...base, kind: "review", failing: [], revisedAfterYou: askedEarlier };

  if (landing.allowed) return isAgent(change.author) && !f.queued ? { ...base, kind: "merge", failing: [], revisedAfterYou: false } : null;

  // Blocked by a rule nobody can approve away (a lease, a stack parent): the author or the
  // agent's supervisor has to step in.
  const rules = landing.missing.filter((m) => !parseApproval(m) && !m.startsWith("check:"));
  if (rules.length && (mine || isAgent(change.author))) return { ...base, kind: "blocked", failing: [], revisedAfterYou: false, reasons: rules.map(missingInWords) };
  return null;
}

/** A required check has not finished: EvaluateLanding lists it as `check:<name> (running)`. */
export function requiredPending(landing: LandingEvaluation): boolean {
  return landing.missing.some((m) => /^check:\S+ \((running|queued|missing)\)$/.test(m));
}

/** How risky the latest revision is, as its place among the diff classes; 0 when unclassified. */
export function riskStep(need: Need): number {
  return riskOf(need.change.revisions.at(-1)?.diffClass ?? "").step;
}

/**
 * Highest risk first: a change to a sensitive path before a dependency bump before a docs fix.
 * Within one risk, what blocks an agent comes before what only waits, then the newest. A post
 * about merged work carries no risk any more, so posts come last.
 */
export function sortNeeds(needs: Need[]): Need[] {
  const risk = (n: Need) => (n.kind === "post" ? -1 : riskStep(n));
  return [...needs].sort((a, b) => risk(b) - risk(a) || KIND_ORDER[a.kind] - KIND_ORDER[b.kind] || Date.parse(b.at ?? "") - Date.parse(a.at ?? "") || (a.change.id < b.change.id ? -1 : a.change.id > b.change.id ? 1 : 0));
}

/**
 * A need as one sentence in two parts: what the author did to the pull request ("wants to
 * merge"), and why it is in front of a person. The screen puts the author before the verb and
 * the title after it; the agent's view prints the same words.
 */
export function needWords(need: Need): { who: string; verb: string; why: string } {
  const who = need.mine ? "You" : (need.who?.display ?? "Someone");
  switch (need.kind) {
    case "review": {
      const rules = whyPerson(need.missing, need.owners);
      return { who, verb: "wants to merge", why: (rules.length ? rules.join(" ") : "It needs your review.") + (need.revisedAfterYou ? " It was revised after you asked for changes." : "") };
    }
    case "failing":
      return { who, verb: need.mine ? "opened" : "is stuck on", why: "It failed the " + list(need.failing) + (need.failing.length === 1 ? " check." : " checks.") };
    case "merge":
      return { who, verb: "finished", why: "Every rule passes. It waits for a person to merge it." };
    case "blocked":
      return { who, verb: "cannot merge", why: "It " + list(need.reasons) + "." };
    case "post":
      return { who, verb: "wrote a post about", why: "The pull request merged. The post waits for your approval to publish." };
  }
}

/**
 * A merged change whose agent-written post waits for a person, when this person may approve it.
 * The server decides who may (`viewer.canApprove`: a human with repo Write who did not write it);
 * this only reads that answer and the state, so a draft on an open change is not asked about yet.
 */
export function postNeed(repo: string, change: ChangeRequest, me: Principal): Need | null {
  const post = change.post;
  if (!post || post.state !== "POST_STATE_PENDING_APPROVAL" || !post.viewer?.canApprove || post.author?.id === me.id) return null;
  return { kind: "post", repo, change, post, who: post.author, mine: false, revisedAfterYou: false, failing: [], reasons: [], missing: [], owners: {}, at: post.updatedAt ?? post.createdAt };
}

/** Everything that needs this person, in the order Needs you lists it, each with who answers for it. */
export function needsOf(open: ChangeFacts[], landed: { repo: string; change: ChangeRequest }[], me: Principal, directory: Map<string, Principal> = new Map()): Need[] {
  const needs: Need[] = [];
  for (const f of open) {
    const n = classify(f, me);
    if (n) needs.push(n);
  }
  for (const l of landed) {
    const n = postNeed(l.repo, l.change, me);
    if (n) needs.push(n);
  }
  // A post is answered for by its author's person, like the pull request it is about.
  return sortNeeds(needs.map((n) => ({ ...n, answers: answerableFor(n.who, directory) ?? undefined })));
}
