/**
 * `change.md` (docs/spec/web-ui.md §3, "For agents"): one deterministic Markdown document with
 * everything about a change request an agent needs: the brief, the checklist, the evidence with
 * links, checks, reviews, threads, the conversation, what changed by area, the file list, the full
 * unified diff, and the CLI, HTTP and MCP calls that act on it. The same inputs always give the
 * same bytes: every list is sorted here, nothing reads a clock, and timestamps are printed as the
 * server sent them.
 */
import type { BriefChecklist, ChangeRequest, CheckRun, Comment, DiffItem, Evidence, LandingEvaluation, Principal, Review, ReviewThread } from "./api/types";
import { pathText } from "./api/ids";
import { askStates, decisionComment } from "./asks";
import { checkState, groupChecks } from "./checkwords";
import { httpLine, toCli, toMcp, type Op } from "./copyas";
import { classInfo } from "./diffclass";
import { benchmarkWords, galleryOrder, KIND_LABEL, kindOf, sourceOf, staleness, testReportWords, titleOf } from "./evidence-view";
import { missingInWords, statusOf } from "./status";
import { fromDiff, whatChanged } from "./whatchanged";

export interface ChangeMdInput {
  /** owner/name. */
  repo: string;
  change: ChangeRequest;
  checklist?: BriefChecklist;
  evidence?: Evidence[];
  checks?: CheckRun[];
  reviews?: Review[];
  threads?: ReviewThread[];
  comments?: Comment[];
  /** The latest revision's diff, with hunks. */
  diff?: DiffItem[];
  landing?: LandingEvaluation;
  /** Absolute origin for links ("https://bee.example"); links stay root-relative without it. */
  origin?: string;
}

const cmp = (a: string, b: string) => (a < b ? -1 : a > b ? 1 : 0);

function who(p: Principal | undefined): string {
  if (!p) return "someone";
  const name = p.display || p.id;
  return p.kind === "PRINCIPAL_KIND_AGENT" ? name + " (agent" + (p.runtime ? ", " + p.runtime : "") + ")" : name;
}

/** A fence longer than any backtick run inside `body`, so content can never close it early. */
function fence(body: string, lang = ""): string {
  const longest = Math.max(2, ...(body.match(/`+/g) ?? []).map((r) => r.length));
  const f = "`".repeat(longest + 1);
  return f + lang + "\n" + body + (body.endsWith("\n") ? "" : "\n") + f;
}

/** Block text as a Markdown quote, so headings inside it stay inside it. */
function quote(text: string): string {
  return text
    .replace(/\r\n/g, "\n")
    .trimEnd()
    .split("\n")
    .map((l) => (l ? "> " + l : ">"))
    .join("\n");
}

/** Text on one line: runs of whitespace, newlines included, become one space. */
function inline(text: string): string {
  return text.replace(/\s+/g, " ").trim();
}

const STATE: Record<string, string> = { CHANGE_STATE_OPEN: "open", CHANGE_STATE_LANDED: "merged", CHANGE_STATE_CLOSED: "closed", CHANGE_STATE_UNSPECIFIED: "unknown" };

/** The unified diff of one file, git-style headers, hunks as sent. */
export function unifiedDiff(items: DiffItem[]): string {
  const out: string[] = [];
  for (const d of [...items].sort((a, b) => cmp(pathText(a.path), pathText(b.path)))) {
    const p = pathText(d.path);
    out.push("diff --bee a/" + p + " b/" + p);
    out.push("--- " + (d.old ? "a/" + p : "/dev/null"));
    out.push("+++ " + (d.new ? "b/" + p : "/dev/null"));
    if (d.binary) {
      out.push("Binary file changed");
      continue;
    }
    for (const h of d.hunks) {
      out.push("@@ -" + h.oldStart + "," + h.oldLen + " +" + h.newStart + "," + h.newLen + " @@");
      out.push(...h.lines);
    }
  }
  return out.join("\n");
}

export interface ChangeAction {
  title: string;
  op: Op;
}

/** The calls that act on the change, in the order a supervisor would take them. */
export function changeActions({ repo, change, comments = [] }: Pick<ChangeMdInput, "repo" | "change" | "comments">): ChangeAction[] {
  const r = { fullName: repo };
  const latest = change.revisions.at(-1)?.n ?? 0;
  const out: ChangeAction[] = [
    { title: "Read the change request", op: { service: "ChangeService", method: "GetChange", request: { repo: r, id: change.id } } },
    { title: "Read what the brief and evidence still owe", op: { service: "BriefService", method: "GetBriefChecklist", request: { repo: r, changeId: change.id } } },
    { title: "List the evidence", op: { service: "BriefService", method: "ListEvidence", request: { repo: r, changeIds: [change.id] } } },
    { title: "Read the diff of r" + latest, op: { service: "ChangeService", method: "GetChangeDiff", request: { repo: r, changeRequestId: change.id, revision: latest } } },
  ];
  for (const s of askStates(change.brief?.asks ?? [], comments)) {
    const option = s.answer ?? s.ask.options[0] ?? "";
    out.push({
      title: (s.answer ? "Change the answer to" : "Answer") + " “" + inline(s.ask.question) + "”",
      op: { service: "ChangeService", method: "CommentChange", request: { repo: r, changeRequestId: change.id, body: decisionComment(s.ask.question, option) } },
    });
  }
  if (change.state === "CHANGE_STATE_OPEN") {
    out.push({ title: "Approve r" + latest, op: { service: "ChangeService", method: "SubmitReview", request: { repo: r, changeRequestId: change.id, revision: latest, verdict: "VERDICT_APPROVE", body: "", comments: [] } } });
    out.push({ title: "Ask for changes on r" + latest, op: { service: "ChangeService", method: "SubmitReview", request: { repo: r, changeRequestId: change.id, revision: latest, verdict: "VERDICT_REQUEST_CHANGES", body: "What to change.", comments: [] } } });
    out.push({ title: "Ask policy whether it can land", op: { service: "ChangeService", method: "EvaluateLanding", request: { repo: r, changeRequestId: change.id } } });
    out.push({ title: "Land through the merge queue", op: { service: "ChangeService", method: "Enqueue", request: { repo: r, changeRequestId: change.id } } });
  }
  return out;
}

export function renderChangeMd(input: ChangeMdInput): string {
  const { repo, change, checklist, origin = "" } = input;
  const evidence = galleryOrder([...(input.evidence ?? [])]);
  const checks = [...(input.checks ?? [])].sort((a, b) => cmp(a.name, b.name) || cmp(a.id, b.id));
  const reviews = [...(input.reviews ?? [])].sort((a, b) => cmp(a.createdAt ?? "", b.createdAt ?? "") || cmp(a.id, b.id));
  const threads = [...(input.threads ?? [])].sort((a, b) => cmp(pathText(a.path), pathText(b.path)) || a.startLine - b.startLine || cmp(a.id, b.id));
  const comments = input.comments ?? [];
  const diff = [...(input.diff ?? [])].sort((a, b) => cmp(pathText(a.path), pathText(b.path)));
  const latest = change.revisions.at(-1)?.n ?? 0;
  const base = origin.replace(/\/$/, "") + "/" + repo + "/changes/" + change.id;
  const brief = change.brief;
  const o: string[] = [];
  const trim = () => {
    while (o[o.length - 1] === "") o.pop();
  };
  const section = (title: string) => {
    trim();
    o.push("", "## " + title, "");
  };

  o.push("# " + inline(change.title) + " (#" + change.number + ")", "");
  o.push("- Repository: " + repo);
  o.push("- Change request: " + change.id + " (" + base + ")");
  o.push("- State: " + (STATE[change.state] ?? change.state));
  o.push("- Author: " + who(change.author));
  o.push("- Target: " + change.targetRef);
  o.push("- Revision: r" + latest + (change.revisions.length ? " (" + classInfo(change.revisions.at(-1)?.diffClass ?? "").label + ")" : ""));
  o.push("- Status: " + statusOf({ change, landing: input.landing, checks, reviews }).sentence);
  if (change.createdAt) o.push("- Opened: " + change.createdAt);
  if (change.inspiredBy?.postId) o.push("- Inspired by: post " + change.inspiredBy.postId + (change.inspiredBy.repo && "fullName" in change.inspiredBy.repo && change.inspiredBy.repo.fullName ? " in " + change.inspiredBy.repo.fullName : ""));

  section("Brief");
  if (!brief || (!brief.outcome && !brief.highlights.length)) o.push("No brief yet.");
  else {
    if (brief.outcome) o.push("**Outcome:** " + inline(brief.outcome), "");
    if (brief.writtenForRevision) o.push("Written for r" + brief.writtenForRevision + (brief.author ? " by " + who(brief.author) : "") + (brief.updatedAt ? " at " + brief.updatedAt : "") + (brief.writtenForRevision < latest ? " (older than r" + latest + ")" : "") + ".", "");
    if (brief.highlights.length) {
      o.push("### Highlights", "");
      for (const h of brief.highlights) o.push("- " + inline(h.text) + (h.paths.length ? " (" + h.paths.map((p) => "`" + p + "`").join(", ") + ")" : ""));
      o.push("");
    }
    if (brief.asks.length) {
      o.push("### Asks", "");
      for (const s of askStates(brief.asks, comments)) {
        o.push("- " + inline(s.ask.question) + " Options: " + s.ask.options.map((x) => "“" + inline(x) + "”").join(", ") + ".");
        o.push("  " + (s.answer !== undefined ? "Decided: “" + s.answer + "” by " + who(s.by) + (s.at ? " at " + s.at : "") + "." : "Not answered yet. Answer with the comment `" + decisionComment(s.ask.question, "<option>") + "`."));
      }
      o.push("");
    }
    if (brief.risks.length) {
      o.push("### Risks", "");
      for (const r of brief.risks) o.push("- " + inline(r.text) + (r.mitigation ? " Mitigation: " + inline(r.mitigation) : ""));
      o.push("");
    }
    if (brief.rollback) o.push("**Rollback:** " + inline(brief.rollback), "");
    if (brief.tryItUrl) o.push("**Try it:** " + brief.tryItUrl, "");
    trim();
  }

  section("Checklist");
  if (!checklist) o.push("Not loaded.");
  else if (!checklist.items.length) o.push("Nothing is owed for r" + checklist.revision + ".");
  else {
    o.push((checklist.complete ? "Complete" : "Incomplete") + " for r" + checklist.revision + ".", "");
    for (const i of checklist.items) o.push("- [" + (i.satisfied ? "x" : " ") + "] `" + i.id + "`: " + i.detail + (i.paths.length ? " Paths: " + i.paths.map((p) => "`" + p + "`").join(", ") + "." : "") + (i.evidenceIds.length ? " Evidence: " + i.evidenceIds.join(", ") + "." : ""));
  }

  section("Evidence");
  if (!evidence.length) o.push("None attached.");
  for (const e of evidence) {
    const stale = staleness(e, latest);
    o.push("### " + inline(titleOf(e)) + " (" + e.id + ")", "");
    o.push("- Kind: " + KIND_LABEL[kindOf(e)] + (e.spec?.hero ? ", hero" : ""));
    o.push("- Source: " + sourceOf(e).label + (e.checkRunId ? " (check run " + e.checkRunId + ")" : "") + (e.workflowArtifactId ? " (artifact " + e.workflowArtifactId + ")" : "") + (e.uploader ? ", added by " + who(e.uploader) : ""));
    o.push("- Revision: r" + e.revision + (stale ? " — " + stale.label + ". " + stale.title + "." : ""));
    if (e.spec?.caption) o.push("- Caption: " + inline(e.spec.caption));
    if (e.spec?.paths.length) o.push("- Paths: " + e.spec.paths.map((p) => "`" + p + "`").join(", "));
    if (e.spec?.url) o.push("- URL: " + e.spec.url);
    for (const s of e.spec?.benchmark ?? []) o.push("- " + inline(s.name) + ": " + s.baseline + " → " + s.value + (s.unit ? " " + s.unit : "") + "; " + benchmarkWords(s) + ".");
    if (e.spec?.testReport) {
      const t = e.spec.testReport;
      o.push("- Tests: " + testReportWords(t) + "." + (t.summary ? " " + inline(t.summary) : ""));
      for (const f of t.failures) o.push("  - Failed: " + inline(f.name) + (f.message ? ": " + inline(f.message) : ""));
    }
    for (const a of e.assets) {
      const role = a.role.replace("EVIDENCE_ASSET_ROLE_", "").toLowerCase();
      o.push("- " + role + ": " + (a.url ? origin.replace(/\/$/, "") + a.url : "(no link)") + " (" + a.contentType + (a.width ? ", " + a.width + "×" + a.height : "") + (Number(a.durationMs) ? ", " + a.durationMs + " ms" : "") + ", " + a.digest + ")" + (a.alt ? ". Alt: " + inline(a.alt) : ""));
    }
    o.push("");
  }
  trim();

  section("Checks");
  if (!checks.length) o.push("No checks have run.");
  for (const g of groupChecks(checks)) {
    o.push(g.sentence);
    for (const c of g.checks) o.push("- " + c.name + ": " + checkState(c) + (c.summary ? ". " + inline(c.summary) : "") + (c.url ? " (" + c.url + ")" : ""));
    o.push("");
  }
  trim();
  if (input.landing) {
    o.push("", "Landing: " + (input.landing.allowed ? "allowed" : "blocked") + ".");
    for (const m of input.landing.missing) o.push("- Missing: `" + m + "` (it " + missingInWords(m) + ")");
    for (const m of input.landing.satisfied) o.push("- Met: `" + m + "`");
  }

  section("Reviews");
  if (!reviews.length) o.push("No reviews yet.");
  for (const r of reviews) {
    const verdict = r.verdict === "VERDICT_APPROVE" ? "approved" : r.verdict === "VERDICT_REQUEST_CHANGES" ? "asked for changes on" : "commented on";
    o.push("- " + who(r.reviewer) + " " + verdict + " r" + r.revision + (r.revision < latest ? " (an earlier revision)" : "") + (r.createdAt ? " at " + r.createdAt : "") + (r.body.trim() ? ":" : "."));
    if (r.body.trim()) o.push(quote(r.body).replace(/^/gm, "  "));
  }

  section("Threads");
  if (!threads.length) o.push("No line threads.");
  for (const t of threads) {
    const lines = t.startLine === t.endLine ? "line " + t.startLine : "lines " + t.startLine + "–" + t.endLine;
    o.push("### `" + pathText(t.path) + "` " + lines + " (" + t.side + ", r" + t.revision + ")" + (t.resolved ? ", resolved" : "") + (t.outdated ? ", outdated" : ""), "");
    for (const c of t.comments) {
      o.push("- " + who(c.author) + (c.createdAt ? " at " + c.createdAt : "") + ":");
      if (c.body.trim()) o.push(quote(c.body).replace(/^/gm, "  "));
      if (c.suggestion) o.push(fence(c.suggestion, "suggestion").replace(/^/gm, "  "));
    }
    o.push("");
  }
  trim();

  section("Conversation");
  if (change.body.trim()) o.push("**Description** by " + who(change.author) + ":", "", quote(change.body), "");
  if (!comments.length) o.push("No comments.");
  for (const c of comments) {
    o.push("- " + who(c.author) + (c.createdAt ? " at " + c.createdAt : "") + (c.edited ? " (edited)" : "") + ":");
    o.push(quote(c.body).replace(/^/gm, "  "));
  }

  const files = fromDiff(diff);
  section("What changed");
  if (!files.length) o.push("No files.");
  for (const a of whatChanged(files, brief?.highlights ?? [])) o.push("- " + a.sentence + (a.highlights.length ? " " + a.highlights.map((h) => "“" + inline(h) + "”").join(" ") : ""));

  section("Files");
  if (!files.length) o.push("No files.");
  for (const f of files) o.push("- `" + f.path + "` " + f.change + ", " + classInfo(f.diffClass).label.toLowerCase() + (f.binary ? ", binary" : ", +" + f.added + " −" + f.removed));

  section("Diff");
  o.push(diff.length ? fence(unifiedDiff(diff), "diff") : "No diff.");

  section("Act on it");
  o.push("Every call below needs `Authorization: Bearer $BEE_TOKEN`. MCP tools take the same arguments as the JSON body.");
  for (const a of changeActions({ repo, change, comments })) {
    o.push("", "### " + a.title, "");
    o.push(fence(toCli(a.op), "sh"), "");
    o.push("HTTP: `" + httpLine(a.op) + "`", "");
    o.push(fence(toMcp(a.op), "json"));
  }

  return o.join("\n") + "\n";
}
