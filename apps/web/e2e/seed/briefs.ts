/**
 * The briefs and evidence the seed attaches to its change requests, through BriefService like an
 * agent would (docs/spec/forge-briefs.md): five scenarios, one per way a change request can owe
 * or show its work.
 *
 *   ui      before/after, a captioned video, an ask, and a screenshot that went stale after r2
 *   cache   a benchmark: p95 latency 120 -> 85 ms
 *   fix     a test report CI verified (a check run the agent reported on the head)
 *   rotate  risks with mitigations and a rollback
 *   docs    nothing owed: the checklist has no items
 *
 * Media comes from e2e/seed/media (written by scripts/evidence-media.mjs, committed).
 */
import { readFileSync } from "node:fs";
import type { ObjectId } from "../../lib/gen/bee/v1/common_pb";
import { CheckStatus } from "../../lib/gen/bee/v1/forge_pb";
import { EvidenceAssetRole, EvidenceKind, EvidenceSource, type BriefChecklist, type Evidence } from "../../lib/gen/bee/v1/briefs_pb";
import { log, repoRef, type Api } from "./lib";

const media = (name: string) => new Uint8Array(readFileSync(new URL("./media/" + name, import.meta.url)));

/** A change request the scenarios touch, with the agent that opened it. */
interface Subject {
  id: string;
  number: bigint;
}

export interface BriefsInput {
  /** The repo's full name, "org/name". */
  repo: string;
  ui: { cr: Subject; agent: Api; r1Head: ObjectId; revise: () => Promise<void> };
  cache: { cr: Subject; agent: Api };
  fix: { cr: Subject; agent: Api; head: ObjectId };
  rotate: { cr: Subject; agent: Api };
  docs: { cr: Subject; agent: Api };
}

/** What the specs may rely on; written to the seed manifest. */
export interface SeededBriefs {
  ui: {
    outcome: string;
    ask: { question: string; options: string[] };
    risk: string;
    rollback: string;
    tryItUrl: string;
    heroTitle: string;
    videoTitle: string;
    staleTitle: string;
    beforeAlt: string;
    afterAlt: string;
    owes: string[];
  };
  cache: { outcome: string; benchmark: { name: string; unit: string; baseline: number; value: number }; title: string };
  fix: { outcome: string; reportTitle: string; passed: number; checkRunId: string; checkName: string };
  rotate: { outcome: string; risks: { text: string; mitigation: string }[]; rollback: string };
  docs: { owes: number };
}

const UI_ALT = {
  before: "The sessions list before the change: four rows that all look the same.",
  after: "The sessions list after the change: the first row is outlined in green with a filled button, the other three have an outlined Sign out button.",
  firstTry: "The first attempt at the sessions list: every row is marked and a banner sits below the list.",
  poster: "The first frame of the walkthrough video: a play button over the sessions page.",
};

const bytes = (e: Evidence) => e.assets.reduce((n, a) => n + Number(a.sizeBytes), 0);

async function checklist(api: Api, repo: string, cr: Subject): Promise<BriefChecklist> {
  return api.brief.getBriefChecklist({ repo: repoRef(repo), changeId: cr.id });
}

function expect(ok: boolean, what: string): void {
  if (!ok) throw new Error("seed briefs: " + what);
}

/** The ids of the items that still owe something. */
const owed = (c: BriefChecklist) => c.items.filter((i) => !i.satisfied).map((i) => i.id);

export async function seedBriefs(input: BriefsInput): Promise<SeededBriefs> {
  const repo = repoRef(input.repo);

  // ------------------------------------------------------------ ui
  const ui = input.ui;
  const uiOutcome = "The settings page marks which session is yours and lets you sign out of the others.";
  const uiAsk = { question: "Show the device name or only \"This device\" on the current row?", options: ["Device name", "Only \"This device\""] };
  const uiRisk = { text: "People who keep several browsers signed in can now sign them out by accident.", mitigation: "Signing out asks for no confirmation, but it only ends that one session." };
  const uiRollback = "Revert this change request; the list goes back to plain rows.";
  const tryIt = "https://preview.playground.example/settings/sessions";
  const firstTry = media("ui-first-try.png");
  // A screenshot of the first attempt, captured for r1.
  const stale = await ui.agent.brief.addEvidence({
    repo,
    changeId: ui.cr.id,
    revision: 1,
    spec: { kind: EvidenceKind.SCREENSHOT, title: "First attempt", caption: "Every row marked, with a banner the design later dropped.", paths: ["src/web/sessions.tsx"], hero: false, order: 2, benchmark: [] },
    assets: [{ role: EvidenceAssetRole.MAIN, contentType: "image/png", data: firstTry, alt: UI_ALT.firstTry, width: 960, height: 600 }],
  });
  await ui.revise();
  // r2 owes visual evidence again: the r1 screenshot no longer counts.
  const afterRevise = await checklist(ui.agent, input.repo, ui.cr);
  expect(afterRevise.items.some((i) => i.id === "evidence:ui" && !i.satisfied), "ui should owe a screenshot after r2");
  const owes = owed(afterRevise);
  await ui.agent.brief.setChangeBrief({
    repo,
    changeId: ui.cr.id,
    brief: {
      outcome: uiOutcome,
      highlights: [
        { text: "The row for this device is outlined and says \"This device\".", paths: ["src/web/sessions.tsx"] },
        { text: "Every other row has a Sign out button that ends only that session.", paths: ["src/web/sessions.tsx"] },
      ],
      asks: [uiAsk],
      risks: [uiRisk],
      rollback: uiRollback,
      tryItUrl: tryIt,
    },
  });
  const before = media("ui-before.png");
  const after = media("ui-after.png");
  const hero = await ui.agent.brief.addEvidence({
    repo,
    changeId: ui.cr.id,
    spec: { kind: EvidenceKind.BEFORE_AFTER, title: "Sessions list, before and after", caption: "Captured at 960 by 600 on the preview.", paths: ["src/web/sessions.tsx"], hero: true, order: 0, benchmark: [] },
    assets: [
      { role: EvidenceAssetRole.BEFORE, contentType: "image/png", data: before, alt: UI_ALT.before, width: 960, height: 600 },
      { role: EvidenceAssetRole.AFTER, contentType: "image/png", data: after, alt: UI_ALT.after, width: 960, height: 600 },
    ],
  });
  const video = await ui.agent.brief.addEvidence({
    repo,
    changeId: ui.cr.id,
    spec: { kind: EvidenceKind.VIDEO, title: "Walkthrough", caption: "Eight seconds: each row switches to its new look in turn.", paths: ["src/web/sessions.tsx"], hero: false, order: 1, benchmark: [] },
    assets: [
      { role: EvidenceAssetRole.MAIN, contentType: "video/webm", data: media("ui-walkthrough.webm"), alt: "", width: 480, height: 300, durationMs: 8000n },
      { role: EvidenceAssetRole.POSTER, contentType: "image/png", data: media("ui-poster.png"), alt: UI_ALT.poster, width: 960, height: 600 },
      { role: EvidenceAssetRole.CAPTIONS, contentType: "text/vtt", data: media("ui-walkthrough.vtt"), alt: "" },
    ],
  });
  const uiList = (await ui.agent.brief.listEvidence({ repo, changeIds: [ui.cr.id] })).evidence;
  expect(uiList.length === 3, "ui should have 3 evidence items, has " + uiList.length);
  expect(uiList.find((e) => e.id === stale.id)?.stale === true, "the r1 screenshot should be stale after r2");
  expect(uiList.find((e) => e.id === hero.id)?.stale === false && uiList.find((e) => e.id === video.id)?.stale === false, "the r2 evidence should be current");
  const uiDone = await checklist(ui.agent, input.repo, ui.cr);
  expect(uiDone.complete, "ui should owe nothing once r2 has evidence: " + owed(uiDone).join(", "));
  log("brief + 3 evidence items (1 stale, a " + bytes(video) + "-byte video with poster and captions) on ui #" + ui.cr.number);

  // ------------------------------------------------------------ cache
  const cache = input.cache;
  const cacheOutcome = "Invoice totals come back about 29% faster for customers with long histories.";
  const series = { name: "p95 latency", unit: "ms", baseline: 120, value: 85 };
  await cache.agent.brief.setChangeBrief({
    repo,
    changeId: cache.cr.id,
    brief: {
      outcome: cacheOutcome,
      highlights: [{ text: "Totals are remembered per customer instead of recomputed on each request.", paths: ["src/billing/cache.ts"] }],
      asks: [],
      risks: [{ text: "The cache never evicts, so memory grows with the number of customers.", mitigation: "A follow-up adds a size limit before it ships." }],
      rollback: "",
      tryItUrl: "",
    },
  });
  const cacheTitle = "Invoice totals, p95 latency";
  await cache.agent.brief.addEvidence({
    repo,
    changeId: cache.cr.id,
    spec: { kind: EvidenceKind.BENCHMARK, title: cacheTitle, caption: "1,000 requests for a customer with 2,400 invoices, run on the same machine.", paths: ["src/billing/cache.ts"], hero: true, order: 0, benchmark: [{ ...series, lowerIsBetter: true }] },
  });
  expect((await checklist(cache.agent, input.repo, cache.cr)).complete, "cache should owe nothing");

  // ------------------------------------------------------------ fix
  const fix = input.fix;
  const fixOutcome = "Invoice totals match the printed invoice to the cent: tax is rounded per line.";
  const checkName = "bun test";
  const reported = await fix.agent.check.reportCheck({
    repo,
    changeset: fix.head,
    name: checkName,
    status: CheckStatus.PASSED,
    summary: "2 passed",
    output: { title: "bun test", summary: "2 passed, 0 failed", text: "src/billing/invoice.test.ts\n✓ lineTotal multiplies quantity by unit price\n✓ invoiceTotal rounds tax per line\n", annotations: [], images: [] },
  });
  await fix.agent.brief.setChangeBrief({
    repo,
    changeId: fix.cr.id,
    brief: {
      outcome: fixOutcome,
      highlights: [{ text: "Tax is rounded once per line, the way the printed invoice does it.", paths: ["src/billing/invoice.ts"] }],
      asks: [],
      risks: [],
      rollback: "",
      tryItUrl: "",
    },
  });
  const reportTitle = "Unit tests";
  const report = await fix.agent.brief.addEvidence({
    repo,
    changeId: fix.cr.id,
    checkRunId: reported.id,
    spec: { kind: EvidenceKind.TEST_REPORT, title: reportTitle, caption: "Reported by the test run on the current head.", paths: ["src/billing/invoice.ts"], hero: true, order: 0, benchmark: [], testReport: { passed: 2, failed: 0, skipped: 0, durationMs: 41n, failures: [], summary: "All 2 billing tests pass" } },
  });
  expect(report.source === EvidenceSource.CHECK_RUN && report.checkRunId === reported.id, "the fix report should come from the check run");
  expect((await checklist(fix.agent, input.repo, fix.cr)).complete, "fix should owe nothing");

  // ------------------------------------------------------------ rotate
  const rotate = input.rotate;
  const rotateOutcome = "Changing someone's role now ends their old sessions, so a downgraded admin cannot keep acting with admin rights.";
  const rotateRisks = [
    { text: "People with several tabs open are signed out of the old ones when their role changes.", mitigation: "Only a role change rotates the id; ordinary requests keep the session." },
    { text: "A bug here could sign everyone out.", mitigation: "The rotation touches one session at a time and is covered by the auth tests." },
  ];
  const rotateRollback = "Revert this change request; sessions keep their ids as before and nothing needs migrating.";
  await rotate.agent.brief.setChangeBrief({
    repo,
    changeId: rotate.cr.id,
    brief: {
      outcome: rotateOutcome,
      highlights: [{ text: "A session gets a new id the moment its owner's role changes; the old id stops working.", paths: ["src/auth/session.ts"] }],
      asks: [],
      risks: rotateRisks,
      rollback: rotateRollback,
      tryItUrl: "",
    },
  });
  const rotateDone = await checklist(rotate.agent, input.repo, rotate.cr);
  expect(rotateDone.complete && rotateDone.items.some((i) => i.id === "brief:risk" && i.satisfied), "rotate should owe nothing and have a satisfied risk item: " + owed(rotateDone).join(", "));

  // ------------------------------------------------------------ docs
  const docsList = await checklist(input.docs.agent, input.repo, input.docs.cr);
  expect(docsList.complete && docsList.items.length === 0, "docs should owe nothing, but the checklist has " + docsList.items.length + " items");

  return {
    ui: {
      outcome: uiOutcome,
      ask: uiAsk,
      risk: uiRisk.text,
      rollback: uiRollback,
      tryItUrl: tryIt,
      heroTitle: "Sessions list, before and after",
      videoTitle: "Walkthrough",
      staleTitle: "First attempt",
      beforeAlt: UI_ALT.before,
      afterAlt: UI_ALT.after,
      owes,
    },
    cache: { outcome: cacheOutcome, benchmark: series, title: cacheTitle },
    fix: { outcome: fixOutcome, reportTitle, passed: 2, checkRunId: reported.id, checkName },
    rotate: { outcome: rotateOutcome, risks: rotateRisks, rollback: rotateRollback },
    docs: { owes: 0 },
  };
}
