/** Safety evidence and oversight in the acme/api demo. */
import { SafetyService, type SafetyRefJson } from "../../gen/bee/v1/safety_pb";
import { BeeError, type ConnectCode } from "../transport";
import type {
  AgentRun, PageRequest, Principal, SafetyAsk, SafetyAttention, SafetyConcern, SafetyEvidence,
  SafetyFinding, SafetyFindingReport, SafetyPeriod, SafetyRecord, SafetyRef, SafetyReview, SafetySeverity,
  SafetySignalCount, SafetySignalKind, SafetyStop, SafetyVerdict, Swarm,
} from "../types";
import { defineService, type Ctx } from "./router";
import { digestHex, ulid } from "./seed";
import type { CrState, FixtureStore, RepoState } from "./store";

const RUN = "SAFETY_WORK_KIND_RUN";
const SWARM = "SAFETY_WORK_KIND_SWARM";
const AS_ASKED = "SAFETY_VERDICT_AS_ASKED";
const MORE = "SAFETY_VERDICT_MORE_THAN_ASKED";
const LESS = "SAFETY_VERDICT_LESS_THAN_ASKED";
const ELSE = "SAFETY_VERDICT_SOMETHING_ELSE";
const LOOK = "SAFETY_VERDICT_NEEDS_A_LOOK";
const NOTE = "SAFETY_SEVERITY_NOTE";
const OPEN = "SAFETY_FINDING_STATE_OPEN";
const DISMISSED = "SAFETY_FINDING_STATE_DISMISSED";
const CONFIRMED = "SAFETY_FINDING_STATE_CONFIRMED";
const OUTSIDE = "SAFETY_SIGNAL_KIND_OUTSIDE_TASK";
const SAID = "SAFETY_SIGNAL_KIND_SAID_VS_SHOWN";
const HARM = "SAFETY_SIGNAL_KIND_HARM_TO_OTHERS";
const SEVERE = "SAFETY_SEVERITY_SEVERE";
const CANCELLED = "AGENT_RUN_STATE_CANCELLED";
const TRIPWIRE_KINDS: ReadonlySet<SafetySignalKind> = new Set([HARM, "SAFETY_SIGNAL_KIND_OWN_LEASH", "SAFETY_SIGNAL_KIND_SECRETS_AND_EXIT"]);

const error = (code: ConnectCode, reason: string, subcode: string, message: string) => new BeeError(code, { reason, subcode, message });
const denied = (message: string) => error("permission_denied", "PERMISSION_DENIED", "safety_permission", message);
const invalid = (message: string) => error("invalid_argument", "INVALID_ARGUMENT", "safety_input", message);
const notFound = (message: string) => error("not_found", "NOT_FOUND", "safety_not_found", message);
const badState = (message: string) => error("failed_precondition", "INVALID_STATE", "safety_state", message);
const publicPrincipal = (p: Principal): Principal => ({ ...p, externalIds: [] });
const repoRef = (repo: RepoState) => ({ id: repo.repo.id });
const live = (run: AgentRun) => run.state === "AGENT_RUN_STATE_QUEUED" || run.state === "AGENT_RUN_STATE_RUNNING";
const sortUnique = (values: string[]) => [...new Set(values)].sort();

type RecordRow = SafetyRecord & { work: SafetyRef };

function runAsk(repo: RepoState, run: AgentRun): SafetyAsk[] {
  const text = run.task?.prompt;
  return text ? [{ text, source: { kind: "SUBJECT_KIND_AGENT_RUN", repo: repoRef(repo), id: run.id } }] : [];
}

function swarmAsk(s: FixtureStore, repo: RepoState, swarm: Swarm): SafetyAsk[] {
  const issue = s.issues.find((i) => i.repoId === repo.repo.id && String(i.number) === swarm.issue);
  const source = { kind: "SUBJECT_KIND_SWARM" as const, repo: repoRef(repo), id: swarm.id };
  return [
    { text: swarm.goal, source },
    ...(issue ? [{ text: [issue.title, issue.body].filter(Boolean).join("\n\n"), source: { kind: "SUBJECT_KIND_ISSUE" as const, repo: repoRef(repo), id: issue.id } }] : []),
    ...swarm.tasks.map(({ title }) => ({ text: title, source })),
  ];
}

function deriveVerdict(row: RecordRow): SafetyVerdict {
  if (row.override) return row.override.verdict;
  const active = row.findings.filter((f) => f.state !== DISMISSED);
  if (active.some((f) => f.report && (f.report.severity !== NOTE || (f.report.kind !== OUTSIDE && f.report.kind !== SAID)))) return LOOK;
  const outside = active.some((f) => f.report?.kind === OUTSIDE);
  const incomplete = active.some((f) => f.report?.kind === SAID);
  if (outside && incomplete) return ELSE;
  if (outside) return MORE;
  return incomplete ? LESS : AS_ASKED;
}

function commandsFor(s: FixtureStore, runs: AgentRun[]): string[] {
  return sortUnique(runs.flatMap((run) => (s.agentRunEvents.get(run.id ?? "") ?? []).flatMap((event) => {
    if (event.kind !== "AGENT_RUN_EVENT_KIND_TOOL_RESULT" || !["Bash", "exec_command", "command_execution", "shell_command"].includes(event.toolName ?? "")) return [];
    try {
      const payload = JSON.parse(event.toolPayload ?? "null") as { command?: unknown; cmd?: unknown } | null;
      const command = payload?.command ?? payload?.cmd;
      return typeof command === "string" && command.trim() ? [command] : [];
    } catch {
      return [];
    }
  })));
}

function tokenScope(s: FixtureStore, ctx: Ctx) {
  const tokenId = ctx.token && s.tokenSecrets.get(ctx.token);
  return s.tokens.find((t) => t.tokenId === tokenId)?.scope;
}

function readableEvidence(s: FixtureStore, ctx: Ctx): void {
  if (tokenScope(s, ctx)?.readPaths.length) throw denied("Safety records require read access across the repository");
}

function scopedRun(s: FixtureStore, ctx: Ctx, run: AgentRun, actor: string): boolean {
  return !!run.session && s.p(actor).id === run.agent?.id && tokenScope(s, ctx)?.session === run.session;
}

function runsOf(s: FixtureStore, repo: RepoState, work: SafetyRef): AgentRun[] {
  if (work.kind === RUN) {
    const run = s.swarms.repoRuns(s, repo).find((r) => r.id === work.id);
    if (!run) throw notFound("No agent run " + work.id);
    return [run];
  }
  const swarm = s.swarms.get(repo, work.id);
  return swarm.memberRunIds.map((id) => s.swarms.run(repo, id));
}

function outcomesIn(s: FixtureStore, repo: RepoState, runs: AgentRun[]): CrState[] {
  const numbers = new Set(runs.flatMap((r) => r.changeRequests ?? []));
  return s.crs.filter((c) => c.repo === repo.name && numbers.has(String(c.number)));
}

function findingReport(value: {
  kind?: SafetySignalKind; severity?: SafetySeverity; evidence?: Partial<SafetyEvidence>;
} | undefined): SafetyFindingReport {
  const observation = required(value?.evidence?.observation, "Evidence");
  if (!value?.kind || value.kind === "SAFETY_SIGNAL_KIND_UNSPECIFIED" || !value.severity || value.severity === "SAFETY_SEVERITY_UNSPECIFIED") {
    throw invalid("A signal kind and severity are required");
  }
  return {
    kind: value.kind,
    severity: value.severity,
    evidence: { observation, subjects: value.evidence?.subjects ?? [], paths: sortUnique(value.evidence?.paths ?? []) },
  };
}

function required(value: string | undefined, label: string): string {
  if (!value?.trim()) throw invalid(label + " is required");
  return value;
}

function chosenVerdict(verdict?: SafetyVerdict): SafetyVerdict {
  if (!verdict || verdict === "SAFETY_VERDICT_UNSPECIFIED") throw invalid("Choose a verdict");
  return verdict;
}

function periodOf(period: Partial<SafetyPeriod> | undefined) {
  const start = Date.parse(period?.start ?? "");
  const end = Date.parse(period?.end ?? "");
  if (!Number.isFinite(start) || !Number.isFinite(end) || end <= start) throw invalid("A period with a start before its end is required");
  return { start, end, contains: (time?: string) => Date.parse(time ?? "") >= start && Date.parse(time ?? "") < end };
}

function pageOf<T>(rows: T[], page: PageRequest | undefined, key: (row: T) => string) {
  const cursor = page?.pageToken;
  const offset = cursor ? rows.findIndex((r) => key(r) === cursor) + 1 : 0;
  if (cursor && !offset) throw invalid("Invalid safety page token");
  const size = Math.min(page?.pageSize || 50, 500);
  const items = rows.slice(offset, offset + size);
  return { items, page: { nextPageToken: rows.length > offset + size ? key(items[size - 1]!) : "" } };
}

function countsOf(findings: SafetyFinding[]): SafetySignalCount[] {
  const counts = new Map<SafetySignalKind, number>();
  for (const { report } of findings) if (report) counts.set(report.kind, (counts.get(report.kind) ?? 0) + 1);
  return [...counts].sort(([a], [b]) => a.localeCompare(b)).map(([kind, count]) => ({ kind, count: String(count) }));
}

function interrupt(s: FixtureStore, repo: RepoState, actor: string, run: AgentRun, reason: string): string[] {
  if (!run.id || !live(run)) return [];
  const id = run.id;
  const time = s.iso(s.now());
  const seq = String(BigInt(run.lastEventSeq ?? "0") + 1n);
  Object.assign(run, { state: CANCELLED, finishedAt: time, failureReason: "CANCELLED", summary: reason, lastEventSeq: seq });
  s.agentRunEvents.set(id, [...(s.agentRunEvents.get(id) ?? []), { runId: id, seq, time, state: CANCELLED, kind: "AGENT_RUN_EVENT_KIND_STATE_CHANGED", text: reason }]);
  s.emit(repo, "agent_run.completed", actor, { subject: { kind: "SUBJECT_KIND_AGENT_RUN", repo: repoRef(repo), id } });
  return [id];
}

function pause(s: FixtureStore, repo: RepoState, actor: string, swarm: Swarm): boolean {
  if (swarm.state !== "SWARM_STATE_WORKING" && swarm.state !== "SWARM_STATE_PLANNING") return false;
  swarm.state = "SWARM_STATE_PAUSED";
  s.swarms.emit(s, repo, actor, swarm, "swarm.state_changed", { state: swarm.state });
  return true;
}

export class SafetyWorld {
  private readonly byWork = new Map<string, RecordRow>();
  readonly attention: SafetyAttention[] = [];
  readonly frozen = new Set<string>();
  private nextId = 0;

  key(s: FixtureStore, kind: string): string {
    return ulid(s.now(), "safety:" + kind + ":" + ++this.nextId);
  }

  list(repo: RepoState): RecordRow[] {
    return [...this.byWork.values()].filter((row) => row.work.repo.id === repo.repo.id)
      .sort((a, b) => (b.updatedAt ?? "").localeCompare(a.updatedAt ?? "") || b.work.id.localeCompare(a.work.id));
  }

  record(s: FixtureStore, repo: RepoState, work: SafetyRef): RecordRow {
    const runs = runsOf(s, repo, work);
    const swarm = work.kind === SWARM ? s.swarms.get(repo, work.id) : undefined;
    const distinctIds = sortUnique(runs.flatMap((r) => r.agent?.id ? [r.agent.id] : []));
    const outcome = outcomesIn(s, repo, runs);
    const ownerId = swarm?.owner?.id || runs[0]?.startedBy?.id || runs[0]?.agent?.ownerId;
    const key = [repo.repo.id, work.kind, work.id].join("/");
    const row: RecordRow = this.byWork.get(key) ?? {
      work: { ...work, repo: repoRef(repo) },
      asked: [], findings: [], verdict: AS_ASKED, agents: [], reviews: [],
    };
    row.asked = swarm ? swarmAsk(s, repo, swarm) : runAsk(repo, runs[0]!);
    row.agents = distinctIds.map((id) => publicPrincipal(s.p(s.handleOf(id))));
    row.answerable = ownerId ? publicPrincipal(s.p(s.handleOf(ownerId))) : undefined;
    row.updatedAt = [row.updatedAt, swarm?.updatedAt, ...runs.map((r) => r.finishedAt ?? r.startedAt ?? r.createdAt)]
      .filter((time): time is string => !!time).sort().at(-1);
    row.done = {
      paths: sortUnique(outcome.flatMap((c) => Object.keys(s.current(c).perPath))),
      commands: commandsFor(s, runs),
      networkDestinations: [],
      mergedChangeRequestIds: sortUnique(outcome.filter((c) => c.state === "CHANGE_STATE_LANDED").map((c) => c.id)),
    };
    this.byWork.set(key, row);
    return row;
  }

  emit(s: FixtureStore, repo: RepoState, actor: string, row: RecordRow, kind: string, id: string, reason = "") {
    row.updatedAt = s.iso(s.now());
    s.emit(repo, kind, actor, {
      subject: { kind: "SUBJECT_KIND_SAFETY_RECORD", repo: repoRef(repo), id: row.work.id },
      payloadJson: JSON.stringify({ id, work_kind: row.work.kind }),
    });
    s.appendAudit(actor, kind, id, repo.name, true, reason);
  }

  addFinding(s: FixtureStore, repo: RepoState, actor: string, row: RecordRow, report: SafetyFindingReport, review?: SafetyReview) {
    const id = this.key(s, "finding");
    const finding = { id, report, state: OPEN, reportedBy: publicPrincipal(s.p(actor)), createdAt: s.iso(s.now()), resolutionReason: "" } satisfies SafetyFinding;
    row.findings.push(finding);
    review?.findingIds.push(id);
    row.verdict = deriveVerdict(row);
    this.emit(s, repo, actor, row, "safety.finding_reported", id);
    if (report.severity === SEVERE && TRIPWIRE_KINDS.has(report.kind)) {
      const reason = report.evidence.observation;
      const pausedSwarm = row.work.kind === SWARM && pause(s, repo, actor, s.swarms.get(repo, row.work.id));
      const stoppedRunIds = runsOf(s, repo, row.work).flatMap((run) => interrupt(s, repo, actor, run, reason));
      const attentionId = this.key(s, "tripwire");
      this.attention.unshift({
        id: attentionId, work: row.work, answerable: row.answerable, createdAt: finding.createdAt,
        tripwire: { findingId: id, kind: report.kind, reason, stoppedRunIds, pausedSwarm },
      });
      this.emit(s, repo, actor, row, "safety.tripwire", attentionId, report.kind);
    }
    return finding;
  }

  seed(s: FixtureStore): void {
    const repo = s.repo({ fullName: "acme/api" });
    const at = (ago: number) => s.iso(s.ago(ago));
    const [rotation, health] = s.swarms.swarms;
    if (!rotation || !health) throw new Error("Safety fixture requires the API swarms");
    const mara = publicPrincipal(s.p("mara"));
    const rotate = s.crByKey("rotate");
    const lead = s.swarms.run(repo, rotation.leadRunId);
    const docs = s.swarms.runs.find((r) => r.task?.prompt === "Check the session guide for old role claims")!;
    const manual = s.swarms.runs.find((r) => r.trigger === "manual")!;
    const histogramRun = s.swarms.run(repo, health.memberRunIds[1]);
    s.agentRunEvents.set(histogramRun.id!, [{
      runId: histogramRun.id, seq: "1", time: at(1_601), kind: "AGENT_RUN_EVENT_KIND_TOOL_RESULT",
      toolName: "exec_command", toolPayload: JSON.stringify({ command: "bun test", exit_code: 0 }),
    }]);
    histogramRun.lastEventSeq = "1";
    const supportRun = s.swarms.run(repo, health.memberRunIds[2]);
    const concernRun = s.swarms.run(repo, health.memberRunIds[3]);

    const record = (kind: SafetyRef["kind"], id: string) => this.record(s, repo, { repo: repoRef(repo), kind, id });
    const add = (row: RecordRow, name: string, ago: number, report: SafetyFindingReport, by?: Principal) => {
      const finding: SafetyFinding = { id: ulid(s.ago(ago), "safety:" + name), report, state: OPEN, createdAt: at(ago), reportedBy: by, resolutionReason: "" };
      row.findings.push(finding);
      row.verdict = deriveVerdict(row);
      return finding;
    };

    const rotationRecord = record(SWARM, rotation.id);
    const healthRecord = record(SWARM, health.id);
    const leadRecord = record(RUN, lead.id!);
    const docsRecord = record(RUN, docs.id!);
    record(RUN, histogramRun.id!);
    const manualRecord = record(RUN, manual.id!);
    record(RUN, concernRun.id!);
    const supportRecord = record(RUN, supportRun.id!);

    const pull = { kind: "SUBJECT_KIND_CHANGE_REQUEST" as const, repo: repoRef(repo), id: rotate.id };
    add(leadRecord, "outside", 90, {
      kind: OUTSIDE, severity: "SAFETY_SEVERITY_NOTE",
      evidence: { observation: "The run changed the regression test while its own task claimed the role downgrade path.", subjects: [pull], paths: ["tests/auth.test.ts"] },
    });
    const dismissed = add(rotationRecord, "rotation-risk", 2_850, {
      kind: "SAFETY_SIGNAL_KIND_HARM_TO_CODE", severity: "SAFETY_SEVERITY_WARNING",
      evidence: { observation: "Revision 1 rotated session IDs on every request, invalidating other tabs.", subjects: [pull], paths: ["src/auth/session.ts"] },
    }, mara);
    Object.assign(dismissed, { state: DISMISSED, resolvedBy: mara, resolutionReason: "Revision 2 only rotates the session on a role change.", resolvedAt: at(1_090) });
    rotationRecord.verdict = deriveVerdict(rotationRecord);

    for (const [row, verdict, note, ago] of [
      [docsRecord, LESS, "The guide was located, but its claims were not verified against production sessions.", 158],
      [manualRecord, ELSE, "The log describes startup checks. The requested timing measurements are still needed.", 70],
    ] as const) {
      row.override = { verdict, person: mara, note, time: at(ago) };
      row.verdict = deriveVerdict(row);
    }

    const report = add(supportRecord, "incident-risk", 1_400, {
      kind: HARM, severity: SEVERE,
      evidence: {
        observation: "An independent reviewer found customer identifiers in a summary proposed for a shared incident channel.",
        subjects: [{ kind: "SUBJECT_KIND_AGENT_RUN", repo: repoRef(repo), id: supportRun.id }],
      },
    }, publicPrincipal(s.p("reviewer")));
    supportRecord.reviews.push({
      id: ulid(s.ago(1_405), "safety:incident-review"), requestedBy: mara, reviewer: publicPrincipal(s.p("reviewer")),
      requestedAt: at(1_405), model: s.agentMeta.get("reviewer")?.model ?? "independent-review",
      proposedVerdict: LOOK, note: "Redact the draft before it is shared.", findingIds: [report.id], submittedAt: at(1_400),
    });
    this.attention.push(
      {
        id: ulid(s.ago(1_402), "safety:session-concern"), work: healthRecord.work, answerable: mara, createdAt: at(1_402),
        concern: {
          id: ulid(s.ago(1_402), "safety:session-concern"), runId: concernRun.id!, raisedBy: publicPrincipal(s.p("docs")),
          category: "SAFETY_CONCERN_CATEGORY_RULE_CONFLICT",
          text: "I was asked to verify the health route with a live customer session. Can an operator supply a redacted session instead?",
        },
      },
      {
        id: ulid(s.ago(1_400), "safety:incident-attention"), work: supportRecord.work, answerable: mara, createdAt: at(1_400),
        tripwire: { findingId: report.id, kind: HARM, reason: report.report!.evidence.observation, stoppedRunIds: [supportRun.id!], pausedSwarm: true },
      },
    );
    this.attention.sort((a, b) => (b.createdAt ?? "").localeCompare(a.createdAt ?? "") || b.id.localeCompare(a.id));
  }
}

function scope(s: FixtureStore, ref: SafetyRefJson | undefined, ctx: Ctx, aggregate = true) {
  const actor = s.caller(ctx.token);
  const repo = s.repo(ref?.repo);
  if (aggregate) readableEvidence(s, ctx);
  if (!ref?.id || (ref.kind !== RUN && ref.kind !== SWARM)) throw invalid("A run or swarm reference is required");
  return { actor, repo, row: s.safety.record(s, repo, { repo: repoRef(repo), id: ref.id, kind: ref.kind }) };
}

function oversight(s: FixtureStore, repo: RepoState, row: RecordRow, actor: string) {
  const person = s.kindOf(actor) === "human" && s.posts.canWrite(s, actor, repo);
  if (!person || (row.answerable?.id !== s.p(actor).id && !s.posts.isAdmin(s, actor, repo))) throw denied("Only the person who answers for this work or a repository admin can do this");
}

function activeReview(row: RecordRow, id?: string) {
  const review = row.reviews.find((r) => r.id === id);
  if (!review) throw notFound("No safety review " + (id ?? ""));
  if (review.submittedAt) throw badState("This review has already been submitted");
  return review;
}

function checkReviewer(s: FixtureStore, row: RecordRow, review: SafetyReview, actor: string) {
  const id = s.p(actor).id;
  if (s.kindOf(actor) !== "agent" || id !== review.reviewer?.id || row.agents.some((a) => a.id === id)) {
    throw denied("This action requires the assigned independent reviewer");
  }
}

function pendingAttention(s: FixtureStore, repo: RepoState, actor: string): SafetyAttention[] {
  if (s.kindOf(actor) !== "human") throw denied("Safety attention is addressed to a person");
  return s.safety.attention.filter((a) => a.work?.repo.id === repo.repo.id && a.answerable?.id === s.p(actor).id && !a.acknowledgedAt);
}

function stop(s: FixtureStore, ctx: Ctx, name: string | undefined, note: string | undefined, frozen: boolean): SafetyStop {
  const actor = s.caller(ctx.token);
  const rec = s.orgRecord(name);
  if (s.kindOf(actor) !== "human" || rec.members.get(actor) !== "ROLE_OWNER") throw denied("Only an organization owner can stop or resume everything");
  const reason = required(note, "A reason");
  const org = rec.org.name;
  const stoppedRunIds: string[] = [];
  const pausedSwarmIds: string[] = [];
  if (frozen) s.safety.frozen.add(org);
  else s.safety.frozen.delete(org);
  const kind = frozen ? "safety.stopped" : "safety.resumed";
  for (const repo of s.repos.values()) {
    if (repo.owner !== org) continue;
    if (frozen) {
      for (const swarm of s.swarms.swarms.filter((w) => w.repo?.id === repo.repo.id)) {
        if (pause(s, repo, actor, swarm)) pausedSwarmIds.push(swarm.id);
      }
      stoppedRunIds.push(...s.swarms.repoRuns(s, repo).flatMap((run) => interrupt(s, repo, actor, run, reason)));
    }
    s.emit(repo, kind, actor, { subject: { kind: "SUBJECT_KIND_ORG", id: rec.org.id } });
  }
  s.appendAudit(actor, kind, org, "", true, reason);
  return {
    org: rec.org.id, frozen, updatedAt: s.iso(s.now()), stoppedRunIds: stoppedRunIds.sort(), pausedSwarmIds: pausedSwarmIds.sort(),
    stoppedRunCount: stoppedRunIds.length, pausedSwarmCount: pausedSwarmIds.length,
  };
}

export const safetyService = defineService(SafetyService, {
  getSafetyRecord: (s, req, ctx) => scope(s, req, ctx).row,
  listSafetyRecords: (s, req, ctx) => {
    readableEvidence(s, ctx);
    const repo = s.repo(req.repo);
    const result = pageOf(s.safety.list(repo), req.page, (r) => r.work.kind + "/" + r.work.id);
    return { records: result.items.map((row) => s.safety.record(s, repo, row.work)), page: result.page };
  },
  reportFinding: (s, req, ctx) => {
    const { actor, repo, row } = scope(s, req.work, ctx);
    let review: SafetyReview | undefined;
    if (s.kindOf(actor) === "human") {
      if (!s.posts.canWrite(s, actor, repo) || req.reviewId) throw denied("A person with repository write access can report a finding");
    } else {
      review = activeReview(row, req.reviewId);
      checkReviewer(s, row, review, actor);
    }
    return s.safety.addFinding(s, repo, actor, row, findingReport(req.report), review);
  },
  resolveFinding: (s, req, ctx) => {
    const { actor, repo, row } = scope(s, req.work, ctx);
    oversight(s, repo, row, actor);
    const finding = row.findings.find((f) => f.id === req.findingId);
    if (!finding) throw notFound("No finding " + req.findingId);
    if (req.state !== DISMISSED && req.state !== CONFIRMED) throw invalid("Choose dismissed or confirmed");
    if (finding.state !== OPEN) throw badState("This finding has already been resolved");
    const reason = required(req.reason, "A reason");
    Object.assign(finding, { state: req.state, resolutionReason: reason, resolvedBy: publicPrincipal(s.p(actor)), resolvedAt: s.iso(s.now()) });
    row.verdict = deriveVerdict(row);
    s.safety.emit(s, repo, actor, row, "safety.finding_resolved", finding.id, reason);
    return finding;
  },
  setVerdict: (s, req, ctx) => {
    const { actor, repo, row } = scope(s, req.work, ctx);
    oversight(s, repo, row, actor);
    const note = required(req.note, "A note");
    row.override = { verdict: chosenVerdict(req.verdict), note, person: publicPrincipal(s.p(actor)), time: s.iso(s.now()) };
    row.verdict = deriveVerdict(row);
    s.safety.emit(s, repo, actor, row, "safety.verdict_set", row.work.id, note);
    return row;
  },
  requestSafetyReview: (s, req, ctx) => {
    const { actor, repo, row } = scope(s, req.work, ctx);
    oversight(s, repo, row, actor);
    const reviewer = s.principals.get(s.handleOf(req.reviewerId ?? ""));
    if (!reviewer || reviewer.kind !== "PRINCIPAL_KIND_AGENT" || !s.inOrg(s.handleOf(reviewer.id), repo.owner)) throw notFound("No review agent " + req.reviewerId);
    if (row.agents.some((a) => a.id === reviewer.id)) throw denied("An agent cannot review its own work");
    const review: SafetyReview = {
      id: s.safety.key(s, "review"), requestedBy: publicPrincipal(s.p(actor)), reviewer: publicPrincipal(reviewer),
      requestedAt: s.iso(s.now()), model: "", proposedVerdict: "SAFETY_VERDICT_UNSPECIFIED", note: "", findingIds: [],
    };
    row.reviews.push(review);
    s.safety.emit(s, repo, actor, row, "safety.review_requested", review.id);
    return review;
  },
  submitSafetyReview: (s, req, ctx) => {
    const { actor, repo, row } = scope(s, req.work, ctx);
    const review = activeReview(row, req.reviewId);
    checkReviewer(s, row, review, actor);
    const model = required(req.model, "The model name");
    const proposedVerdict = chosenVerdict(req.proposedVerdict);
    const findings = (req.findings ?? []).map(findingReport);
    for (const finding of findings) s.safety.addFinding(s, repo, actor, row, finding, review);
    Object.assign(review, { model, proposedVerdict, note: req.note ?? "", submittedAt: s.iso(s.now()) });
    s.safety.emit(s, repo, actor, row, "safety.review_submitted", review.id);
    return review;
  },
  raiseConcern: (s, req, ctx) => {
    const { actor, repo, row: runRecord } = scope(s, { repo: req.repo, kind: RUN, id: req.runId }, ctx, false);
    const run = runsOf(s, repo, runRecord.work)[0]!;
    if (!scopedRun(s, ctx, run, actor)) throw denied("Use the credential of the run raising the concern");
    if (!live(run)) throw badState("This run has finished");
    const category = req.category;
    if (!category || category === "SAFETY_CONCERN_CATEGORY_UNSPECIFIED") throw invalid("Choose a concern category");
    const text = required(req.text, "A concern");
    const id = s.safety.key(s, "concern");
    const swarm = s.swarms.swarms.find((w) => w.memberRunIds.includes(runRecord.work.id));
    const row = swarm ? s.safety.record(s, repo, { repo: repoRef(repo), kind: SWARM, id: swarm.id }) : runRecord;
    const concern: SafetyConcern = { id, runId: runRecord.work.id, category, text, raisedBy: publicPrincipal(s.p(actor)) };
    interrupt(s, repo, actor, run, "Agent raised a concern");
    s.safety.attention.unshift({ id, work: row.work, answerable: row.answerable, concern, createdAt: s.iso(s.now()) });
    s.safety.emit(s, repo, actor, row, "safety.concern_raised", id, category);
    return concern;
  },
  listSafetyAttention: (s, req, ctx) => {
    readableEvidence(s, ctx);
    const result = pageOf(pendingAttention(s, s.repo(req.repo), s.caller(ctx.token)), req.page, (a) => a.id);
    return { items: result.items, page: result.page };
  },
  acknowledgeSafetyAttention: (s, req, ctx) => {
    readableEvidence(s, ctx);
    const repo = s.repo(req.repo);
    const actor = s.caller(ctx.token);
    const item = s.safety.attention.find((a) => a.id === req.id && a.work?.repo.id === repo.repo.id && a.answerable?.id === s.p(actor).id);
    if (!item?.work || s.kindOf(actor) !== "human") throw notFound("No safety attention item " + req.id);
    if (!item.acknowledgedAt) {
      item.acknowledgedAt = s.iso(s.now());
      s.safety.emit(s, repo, actor, s.safety.record(s, repo, item.work), "safety.attention_acknowledged", item.id);
    }
    return item;
  },
  stopEverything: (s, req, ctx) => stop(s, ctx, req.org, req.reason, true),
  resumeEverything: (s, req, ctx) => stop(s, ctx, req.org, req.reason, false),
  spotCheck: (s, req, ctx) => {
    readableEvidence(s, ctx);
    const repo = s.repo(req.repo);
    const actor = s.caller(ctx.token);
    if (s.kindOf(actor) !== "human" || !s.posts.canWrite(s, actor, repo)) throw denied("A person with repository write access can request a spot check");
    const period = periodOf(req.period);
    const seed = required(req.seed, "A seed");
    const landed = new Set(s.events.flatMap((e) => {
      if (e.repoId !== repo.repo.id || e.kind !== "cr.landed" || !e.change || !period.contains(e.time) || s.kindOf(s.handleOf(e.principal)) !== "agent") return [];
      const reviews = s.cr(repo, e.change.id).reviews;
      const humanApproval = reviews.some((r) => r.verdict === "VERDICT_APPROVE" && r.reviewer?.kind === "PRINCIPAL_KIND_HUMAN"
        && Date.parse(r.createdAt ?? "") <= Date.parse(e.time ?? "") && r.revision <= e.change!.revisions.length);
      return humanApproval ? [] : [e.change.id];
    }));
    const rank = (work: SafetyRef) => digestHex(JSON.stringify([seed, period.start, period.end, repo.repo.id, work.kind, work.id]));
    const records = s.swarms.list(s, repo, actor, ctx)
      .flatMap<SafetyRef>(({ swarm, run }) => swarm
        ? [{ kind: SWARM, id: swarm.id, repo: repoRef(repo) }]
        : run?.id ? [{ kind: RUN, id: run.id, repo: repoRef(repo) }] : [])
      .filter((work) => outcomesIn(s, repo, runsOf(s, repo, work)).some((change) => landed.has(change.id)))
      .sort((a, b) => rank(a).localeCompare(rank(b)) || a.id.localeCompare(b.id))
      .slice(0, Math.min(req.limit || 5, 7))
      .map((work) => s.safety.record(s, repo, work));
    return { records };
  },
  getSafetySummary: (s, req, ctx) => {
    readableEvidence(s, ctx);
    const actor = s.caller(ctx.token);
    const org = s.orgRecord(req.org);
    const role = org.members.get(actor);
    if (s.kindOf(actor) !== "human" || (role !== "ROLE_OWNER" && role !== "ROLE_ADMIN" && role !== "ROLE_SECURITY_MANAGER")) throw denied("This summary requires audit log access");
    const period = periodOf(req.period);
    const repos = s.reposFor(undefined, org.org.name, actor);
    const findings: SafetyFinding[] = [];
    const rows = new Map<string, { id: string; kind: SafetyRef["kind"]; findings: SafetyFinding[] }>();
    for (const repo of repos) for (const record of s.safety.list(repo)) {
      const inPeriod = record.findings.filter((f) => period.contains(f.createdAt));
      if (!inPeriod.length) continue;
      findings.push(...inPeriod);
      const buckets: Pick<SafetyRef, "kind" | "id">[] = record.agents.map((a) => ({ kind: RUN, id: a.id }));
      if (record.work.kind === SWARM) buckets.push({ kind: SWARM, id: record.work.id });
      for (const bucket of buckets) {
        const key = bucket.kind + "/" + bucket.id;
        const row = rows.get(key) ?? { ...bucket, findings: [] };
        row.findings.push(...inPeriod);
        rows.set(key, row);
      }
    }
    const waits = findings.filter((f) => f.resolvedAt && f.createdAt)
      .map((f) => Date.parse(f.resolvedAt!) - Date.parse(f.createdAt!)).sort((a, b) => a - b);
    const mid = Math.floor(waits.length / 2);
    const median = waits.length === 0 ? undefined : waits.length % 2 ? waits[mid] : Math.floor((waits[mid - 1]! + waits[mid]!) / 2);
    return {
      signals: countsOf(findings),
      work: [...rows.entries()].sort(([a], [b]) => a.localeCompare(b)).map(([, { id, kind, findings }]) => ({ id, kind, signals: countsOf(findings) })),
      tripwires: s.safety.attention.filter((a) => a.tripwire && period.contains(a.createdAt) && repos.some((repo) => repo.repo.id === a.work?.repo.id)),
      medianWaitMs: median === undefined ? undefined : String(median),
    };
  },
});
