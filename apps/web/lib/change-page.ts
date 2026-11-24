/**
 * Pure rules of the change request page (docs/spec/web-ui.md §3 "Change detail"): its tabs, where
 * `?rev` and `?since` go, the risk sentence, the conversation's order, the check output's words and
 * the Bearer token `change.md` accepts. No clock and no I/O, so each one is unit tested.
 */
import { pathText } from "./api/ids";
import type { ChangeBrief, CheckAnnotation, CheckRun, Comment, Evidence, Review } from "./api/types";
import { countWord, listing } from "./checkwords";
import { classInfo } from "./diffclass";
import { isReservedOwner, repoHref } from "./routes";

export type ChangeTab = "brief" | "code" | "checks" | "agents";

/** The tabs in order: the segment under `changes/[id]/` ("" for the Brief) and the label. */
export const CHANGE_TABS: readonly { tab: ChangeTab; segment: string; label: string }[] = [
  { tab: "brief", segment: "", label: "Brief" },
  { tab: "code", segment: "code", label: "Code" },
  { tab: "checks", segment: "checks", label: "Checks" },
  { tab: "agents", segment: "agents", label: "For agents" },
];

/** The tab a layout segment names; anything unknown is the Brief. */
export function tabOf(segment: string | null | undefined): ChangeTab {
  return CHANGE_TABS.find((t) => t.segment && t.segment === segment)?.tab ?? "brief";
}

/** `/{repo}/changes/{id}[/{tab}]`. */
export function changeHref(repo: string, id: string, tab: ChangeTab = "brief", query = ""): string {
  const seg = CHANGE_TABS.find((t) => t.tab === tab)?.segment ?? "";
  return repoHref(repo, "changes/" + id + (seg ? "/" + seg : "") + (query ? "?" + query.replace(/^\?/, "") : ""));
}

/** The Code tab at revision `rev`, optionally showing only what changed since `since`. */
export function codeHref(repo: string, id: string, rev?: number, since?: number): string {
  const q = new URLSearchParams();
  if (rev) q.set("rev", String(rev));
  if (since) q.set("since", String(since));
  return changeHref(repo, id, "code", q.toString());
}

/**
 * `?rev` and `?since` belong to the Code tab: the Brief answers them with the Code tab's URL,
 * keeping both. Null when neither is present.
 */
export function codeRedirect(repo: string, id: string, sp: Record<string, string | string[] | undefined>): string | null {
  const one = (k: string) => {
    const v = sp[k];
    return (Array.isArray(v) ? v[0] : v) ?? "";
  };
  const rev = one("rev");
  const since = one("since");
  if (!rev && !since) return null;
  const q = new URLSearchParams();
  if (rev) q.set("rev", rev);
  if (since) q.set("since", since);
  return changeHref(repo, id, "code", q.toString());
}

/** The revision the Code and Checks tabs show: `?rev` clamped to 1..latest, else the latest. */
export function viewedRevision(raw: string, latest: number): number {
  const n = Math.trunc(Number(raw));
  if (!latest) return 0;
  return Math.min(latest, Math.max(1, Number.isFinite(n) && n > 0 ? n : latest));
}

/** `?since` when it names an earlier revision than the one viewed, else 0. */
export function sinceRevision(raw: string, viewing: number): number {
  const n = Math.trunc(Number(raw));
  return Number.isFinite(n) && n > 0 && n < viewing ? n : 0;
}

/** The token of an `Authorization: Bearer <token>` header, or undefined. */
export function bearerToken(header: string | null | undefined): string | undefined {
  const m = /^Bearer\s+(\S+)\s*$/i.exec(header ?? "");
  return m?.[1];
}

const RISK_WORDS: Record<string, string> = {
  high: "High risk",
  elevated: "Some risk",
  medium: "Some risk",
  low: "Low risk",
};

const CLASS_SENTENCE: Record<string, string> = {
  sensitive: "it touches a path the repository marks sensitive",
  logic: "it changes how the code behaves",
  dependency: "it changes the dependencies",
  generated: "only generated files changed",
  docs: "only documentation changed",
  comment_only: "only comments and whitespace changed",
  test_only: "only tests changed",
  rename_only: "files moved without changing",
};

/**
 * "Is it safe?" in one sentence: the revision's class in words, then what the brief offers
 * against it. "High risk: it touches a path the repository marks sensitive. The author lists two
 * risks with mitigations and a rollback."
 */
export function riskSentence(diffClass: string, brief: ChangeBrief | undefined): string {
  const info = classInfo(diffClass);
  const head = (RISK_WORDS[info.risk] ?? "Some risk") + ": " + (CLASS_SENTENCE[diffClass] ?? (diffClass ? "the change is classed " + info.label.toLowerCase() : "the change is not classified yet")) + ".";
  const risks = brief?.risks ?? [];
  const mitigated = risks.filter((r) => r.mitigation.trim()).length;
  const offers: string[] = [];
  if (risks.length) offers.push(countWord(risks.length) + (risks.length === 1 ? " risk" : " risks") + (mitigated === risks.length ? " with " + (risks.length === 1 ? "a mitigation" : "mitigations") : mitigated ? ", " + countWord(mitigated) + " mitigated" : ""));
  if (brief?.rollback.trim()) offers.push("a rollback");
  if (!offers.length) return info.risk === "low" ? head : head + " The author names no risks and no rollback.";
  return head + " The author lists " + listing(offers) + ".";
}

export type ConversationItem = { kind: "comment"; at: string; comment: Comment } | { kind: "review"; at: string; review: Review };

/**
 * The Brief's conversation: comments and reviews, oldest first, ties by id so the order never
 * depends on the server's. Inline review comments stay on the Code tab with their lines.
 */
export function conversationOf(comments: Comment[], reviews: Review[]): ConversationItem[] {
  const items: ConversationItem[] = [
    ...comments.map((c) => ({ kind: "comment" as const, at: c.createdAt ?? "", comment: c })),
    ...reviews.map((r) => ({ kind: "review" as const, at: r.createdAt ?? "", review: r })),
  ];
  const id = (i: ConversationItem) => (i.kind === "comment" ? i.comment.id : i.review.id);
  return items.sort((a, b) => (a.at < b.at ? -1 : a.at > b.at ? 1 : id(a) < id(b) ? -1 : id(a) > id(b) ? 1 : 0));
}

/** Evidence a check run attached (CHECK_RUN source), by check run id. */
export function evidenceByCheck(evidence: Evidence[]): Map<string, Evidence[]> {
  const out = new Map<string, Evidence[]>();
  for (const e of evidence) {
    if (!e.checkRunId) continue;
    out.set(e.checkRunId, [...(out.get(e.checkRunId) ?? []), e]);
  }
  return out;
}

/** CI evidence that no check run on this revision claims: workflow artifacts and older runs. */
export function unclaimedCiEvidence(evidence: Evidence[], checks: Pick<CheckRun, "id">[]): Evidence[] {
  const ids = new Set(checks.map((c) => c.id));
  return evidence.filter((e) => e.source !== "EVIDENCE_SOURCE_UPLOADED" && e.source !== "EVIDENCE_SOURCE_UNSPECIFIED" && !(e.checkRunId && ids.has(e.checkRunId)));
}

export interface AnnotationView {
  path: string;
  line: number;
  level: "failure" | "warning" | "notice";
  title: string;
  message: string;
}

/** An annotation in words-ready form; failures first, then by path and line. */
export function annotationViews(items: CheckAnnotation[] = []): AnnotationView[] {
  const rank = { failure: 0, warning: 1, notice: 2 };
  return items
    .map((a) => ({
      path: pathText(a.path ?? ""),
      line: a.startLine ?? 0,
      level: a.level === "ANNOTATION_LEVEL_FAILURE" ? ("failure" as const) : a.level === "ANNOTATION_LEVEL_WARNING" ? ("warning" as const) : ("notice" as const),
      title: a.title ?? "",
      message: a.message ?? "",
    }))
    .sort((a, b) => rank[a.level] - rank[b.level] || (a.path < b.path ? -1 : a.path > b.path ? 1 : 0) || a.line - b.line);
}

/** "2 failures and 1 warning point at code." Empty when there are none. */
export function annotationsSentence(items: AnnotationView[]): string {
  if (!items.length) return "";
  const n = (level: AnnotationView["level"], one: string, many: string) => {
    const k = items.filter((a) => a.level === level).length;
    return k ? countWord(k) + " " + (k === 1 ? one : many) : "";
  };
  const parts = [n("failure", "failure", "failures"), n("warning", "warning", "warnings"), n("notice", "notice", "notices")].filter(Boolean);
  const said = listing(parts);
  return said.charAt(0).toUpperCase() + said.slice(1) + (items.length === 1 ? " points" : " point") + " at code.";
}

/** A check image's link, only when it is https; anything else is dropped. */
export function safeImageLink(url: string | undefined): string | null {
  try {
    const u = new URL(url ?? "");
    return u.protocol === "https:" ? u.toString() : null;
  } catch {
    return null;
  }
}

/**
 * For `proxy.ts`: `/{owner}/{repo}/changes/{id}?rev=…&since=…` answers with the Code tab's URL
 * before anything renders, so a client without JavaScript gets a real redirect. Null for any other
 * path, or when neither parameter is present.
 */
export function changeCodePath(pathname: string, search: URLSearchParams): string | null {
  const m = /^\/([^/]+)\/([^/]+)\/changes\/([^/]+)$/.exec(pathname);
  if (!m || !m[1] || isReservedOwner(m[1])) return null;
  const q = new URLSearchParams();
  const rev = search.get("rev");
  const since = search.get("since");
  if (rev) q.set("rev", rev);
  if (since) q.set("since", since);
  return q.size ? pathname + "/code?" + q.toString() : null;
}
