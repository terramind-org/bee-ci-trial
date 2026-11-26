import { createElement as h } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it, vi } from "vitest";
import { renderMarkdown } from "../markdown";
import { evidenceIndex } from "@/lib/evidence-view";
import { AGENT, brief, change, check, checklist, comment, diffItem, evidence, image, LIN, MARA, review } from "@/lib/brief-samples";
import { BenchmarkChart } from "./benchmark-chart";
import { clampPosition, CompareSlider, keyStep } from "./compare-slider";
import { EvidenceGallery } from "./evidence-gallery";
import { EvidenceVideo } from "./evidence-video";
import { HumanChecks, checkHref } from "./human-checks";
import { PullDecisionButtons } from "../pull/decisions";
import { ToastProvider } from "../toast";
import { NeedsYou, owedSentence } from "./needs-you";
import { PreviewLink, safePreviewUrl } from "./preview-link";
import { SourceChip, StaleBadge } from "./source-chip";
import { Team } from "./team";
import { TestReport } from "./test-report";
import { WhatChanged } from "./what-changed";
import { WhoDidIt } from "./who-did-it";
import { WorthALook } from "./worth-a-look";

vi.mock("next/navigation", () => ({ useRouter: () => ({ refresh: () => {}, push: () => {} }), usePathname: () => "/acme/api/changes/cr-1" }));

const render = (el: Parameters<typeof renderToStaticMarkup>[0]) => renderToStaticMarkup(el);

describe("markdown evidence images", () => {
  const idx = evidenceIndex([evidence("ev1", { assets: [image("MAIN", "/api/evidence/ev1/main?sig=x&_d=" + "a".repeat(64), "The settings page")] })]);

  it("loads only evidence of the change, through the same-origin route", () => {
    const html = renderMarkdown("Before: ![New settings](evidence:ev1) and `![x](evidence:ev1)`", "acme/api", idx);
    expect(html).toContain('<img src="/api/evidence/ev1/main?sig=x&amp;_d=' + "a".repeat(64) + '" alt="New settings" loading="lazy"');
    expect(html).toContain('data-evidence-image="ev1"');
    // Inside a code span it stays code.
    expect(html).toContain("<code>![x](evidence:ev1)</code>");
  });

  it("uses the evidence's alt text when the Markdown gives none, and the alt alone for unknown ids", () => {
    expect(renderMarkdown("![](evidence:ev1)", undefined, idx)).toContain('alt="The settings page"');
    const html = renderMarkdown("See ![a missing shot](evidence:nope).", undefined, idx);
    expect(html).toBe("<p>See a missing shot.</p>");
    expect(renderMarkdown("![shot](evidence:ev1)")).toBe("<p>shot</p>");
  });

  it("turns every other image into a link and never loads it", () => {
    const html = renderMarkdown("![remote](https://example.com/x.png) ![bad](javascript:alert(1)) ![under_score](/a_b_.png)");
    expect(html).not.toContain("<img");
    expect(html).toContain('<a href="https://example.com/x.png" rel="noreferrer noopener" target="_blank">remote</a>');
    expect(html).toContain('<a href="#">bad</a>');
    expect(html).toContain('<a href="/a_b_.png">under_score</a>');
  });

  it("ignores a NUL in the source", () => {
    expect(renderMarkdown("a\u00000\u0000b")).toBe("<p>a0b</p>");
  });
});

describe("compare slider", () => {
  it("moves by 5 with arrows, 25 with page keys, to the ends with Home and End, within 0-100", () => {
    expect(keyStep("ArrowRight", 50)).toBe(55);
    expect(keyStep("ArrowLeft", 50)).toBe(45);
    expect(keyStep("ArrowUp", 98)).toBe(100);
    expect(keyStep("ArrowDown", 3)).toBe(0);
    expect(keyStep("PageUp", 50)).toBe(75);
    expect(keyStep("PageDown", 10)).toBe(0);
    expect(keyStep("Home", 50)).toBe(0);
    expect(keyStep("End", 50)).toBe(100);
    expect(keyStep("Enter", 50)).toBeNull();
    expect(clampPosition(120.4)).toBe(100);
  });

  it("is a labelled slider with both alt texts in the caption", () => {
    const html = render(h(CompareSlider, { before: image("BEFORE", "/api/evidence/b", "Old menu"), after: image("AFTER", "/api/evidence/a", "New menu"), title: "Menu" }));
    expect(html).toContain('role="slider"');
    expect(html).toContain('aria-valuenow="50"');
    expect(html).toContain('aria-valuemin="0"');
    expect(html).toContain('aria-valuemax="100"');
    expect(html).toContain('aria-label="Compare before and after: Menu"');
    expect(html).toContain("Old menu");
    expect(html).toContain("New menu");
    expect(html).toContain("clip-path:inset(0 0 0 50%)");
  });

  it("moves a label beside the divider when its side is too narrow", () => {
    const html = render(h(CompareSlider, { before: image("BEFORE", "/b", "Old"), after: image("AFTER", "/a", "New"), title: "M", initial: 5 }));
    expect(html).toContain("‹ Before");
    expect(html).toContain(">After<");
  });
});

describe("video", () => {
  it("downloads nothing until play: a button with the poster, the length and captions noted", () => {
    const media = { ...image("MAIN", "/api/evidence/v/main", ""), contentType: "video/mp4", durationMs: "21000" };
    const html = render(h(EvidenceVideo, { media, poster: image("POSTER", "/api/evidence/v/poster", ""), captions: { ...image("MAIN", "/api/evidence/v/captions", ""), contentType: "text/vtt" }, title: "Walkthrough" }));
    expect(html).not.toContain("<video");
    expect(html).not.toContain("/api/evidence/v/main");
    expect(html).toContain('aria-label="Play video: Walkthrough, 21s, with captions"');
    expect(html).toContain('src="/api/evidence/v/poster"');
    expect(html).toContain(">CC<");
  });
});

describe("benchmark chart, test report and preview link", () => {
  it("states better or worse in words, with values in a hidden table", () => {
    const html = render(h(BenchmarkChart, { title: "Cache", series: [{ name: "p95", unit: "ms", baseline: 120, value: 85, lowerIsBetter: true }, { name: "memory", unit: "MB", baseline: 210, value: 224, lowerIsBetter: true }] }));
    expect(html).toContain("35 ms faster (29% better) than the baseline.");
    expect(html).toContain("14 MB larger (7% worse) than the baseline.");
    expect(html).toContain("Better on 1 of 2 measures, worse on 1");
    expect(html).toContain('<table class="sr-only">');
    expect(html).toContain("<title>This change: 85 ms</title>");
    expect(html).toContain("text-ok");
    expect(html).toContain("text-bad");
  });

  it("says the test counts and lists failures", () => {
    const html = render(h(TestReport, { report: { passed: 13, failed: 1, skipped: 0, durationMs: "4000", failures: [{ name: "auth::rotates", message: "expected 2 tokens" }], summary: "" } }));
    expect(html).toContain("Failed: </span>13 passed and 1 failed in 4s");
    expect(html).toContain("auth::rotates");
  });

  it("links only https previews", () => {
    expect(safePreviewUrl("http://x.example")).toBeNull();
    expect(safePreviewUrl("https://u:p@x.example")).toBeNull();
    expect(render(h(PreviewLink, { url: "javascript:alert(1)" }))).not.toContain("<a");
    expect(render(h(PreviewLink, { url: "https://preview.example/pr-1", title: "Try the preview" }))).toContain('href="https://preview.example/pr-1"');
  });
});

describe("evidence gallery", () => {
  it("puts the hero first, labels sources and revisions, and flags stale items", () => {
    const items = [
      evidence("ci", { source: "EVIDENCE_SOURCE_CHECK_RUN", checkRunId: "ck-1", spec: { kind: "EVIDENCE_KIND_TEST_REPORT", title: "Unit tests", testReport: { passed: 14, failed: 0, skipped: 0, durationMs: "0", failures: [], summary: "" } } }),
      evidence("hero", { spec: { hero: true, title: "New settings", caption: "The toggle" }, assets: [image("MAIN", "/api/evidence/hero/main", "Settings")] }),
      evidence("old", { stale: true, revision: 1, uploader: LIN, spec: { title: "Old shot" }, assets: [image("MAIN", "/api/evidence/old/main", "Old")] }),
    ];
    const html = render(h(EvidenceGallery, { items, repo: "acme/api", changeId: "cr-1", latest: 2 }));
    expect(html.indexOf("evidence-hero")).toBeLessThan(html.indexOf("evidence-ci"));
    expect(html).toContain("lg:col-span-2");
    expect(html).toContain('loading="eager"');
    expect(html).toContain("From the agent");
    expect(html).toContain("From Lin Chen");
    expect(html).toContain("Verified by CI");
    expect(html).toContain('href="/acme/api/changes/cr-1/checks#check-ck-1"');
    expect(html).toContain("Stale: from r1");
    expect(html).toContain("opacity-60");
    expect(html).toContain("14 passed");
    expect(html.match(/data-nav-item/g)?.length).toBe(3);
  });

  it("says what fills it when empty", () => {
    expect(render(h(EvidenceGallery, { items: [], latest: 1 }))).toContain("No evidence yet");
  });

  it("chips and badges stand alone", () => {
    expect(render(h(SourceChip, { evidence: { source: "EVIDENCE_SOURCE_WORKFLOW_ARTIFACT", checkRunId: "" } }))).toContain("Verified by CI");
    expect(render(h(StaleBadge, { evidence: { stale: false, revision: 2 }, latest: 2 }))).toBe("");
  });
});

describe("what the author asks of a person", () => {
  const ask = { question: "Sign out other devices?", options: ["Only this session", "Every device"] };
  const cr = change({ brief: brief({ asks: [ask] }) });
  const owes = checklist(false, [{ id: "evidence:ui", detail: "a screenshot of the UI change", kinds: [], paths: [], satisfied: false, evidenceIds: [] }]);

  it("offers the asks as decisions to a reviewer, and says what the agent owes", () => {
    const html = render(h(NeedsYou, { repo: "acme/api", change: cr, comments: [], checklist: owes, viewer: MARA }));
    expect(html).toContain('role="group" aria-label="Sign out other devices?"');
    expect(html).toContain(">Every device<");
    expect(html).toContain("The agent still owes: a screenshot of the UI change.");
    // Approving, requesting changes and merging are the buttons under the title, not here.
    expect(html).not.toContain("Approve");
    expect(html).not.toContain("data-agent-confirm");
  });

  it("shows the decision taken, and no decision buttons to the author", () => {
    const html = render(h(NeedsYou, { repo: "acme/api", change: cr, comments: [comment(MARA, 'Decision on "Sign out other devices?": Every device', "2026-10-27T12:00:00Z")], viewer: AGENT }));
    expect(html).toContain("Decided: “Every device” by Mara Okafor");
    expect(html).not.toContain('role="group"');
  });

  it("asks a signed-out visitor to sign in, and renders nothing when nothing is asked or owed", () => {
    expect(render(h(NeedsYou, { repo: "acme/api", change: cr, comments: [], viewer: null, signInNext: "/acme/api/changes/cr-1" }))).toContain('href="/signin?next=%2Facme%2Fapi%2Fchanges%2Fcr-1"');
    expect(render(h(NeedsYou, { repo: "acme/api", change: change({ state: "CHANGE_STATE_LANDED", brief: brief({ asks: [ask] }) }), comments: [], viewer: MARA }))).toBe("");
    expect(render(h(NeedsYou, { repo: "acme/api", change: change(), comments: [], viewer: MARA }))).toBe("");
    expect(owedSentence(checklist(true), AGENT)).toBeNull();
  });
});

describe("the decisions under the title", () => {
  const html = (decisions: Parameters<typeof PullDecisionButtons>[0]["decisions"], signInHref?: string) => render(h(ToastProvider, null, h(PullDecisionButtons, { repo: "acme/api", crId: "cr-1", number: "41", revision: 2, target: "main", decisions, signInHref })));

  it("offers Approve and merge, Request changes and Close, with one solid button, each marked for agents", () => {
    const out = html({ primary: "approve-and-merge", canRequestChanges: true, canClose: true, note: "Your approval is recorded first; it merges if the rules then allow it." });
    expect(out).toContain(">Approve and merge<");
    expect(out).toContain(">Request changes<");
    expect(out).toContain(">Close<");
    expect(out.match(/bg-accent /g)).toHaveLength(1);
    expect(out).toContain('data-agent-confirm="Approves revision 2 of pull request #41 in acme/api and merges it into main if the rules then allow it"');
    expect(out).toContain('data-agent-confirm="Closes pull request #41 in acme/api without merging it"');
    expect(out).toContain("Your approval is recorded first");
  });

  it("offers Merge alone to someone who already approved, and nothing to press on a merged one", () => {
    const merge = html({ primary: "merge", canRequestChanges: true, canClose: true, note: "You approved revision 2. The rules allow merging it." });
    expect(merge).toContain(">Merge<");
    expect(merge).not.toContain("Approve and merge");
    const done = html({ primary: null, canRequestChanges: false, canClose: false, note: "It merged. Nothing needs you here." });
    expect(done).not.toContain("<button");
    expect(done).toContain("It merged. Nothing needs you here.");
  });

  it("asks a signed-out reader to sign in", () => {
    const out = html({ primary: null, canRequestChanges: false, canClose: false, note: "Sign in to approve it, request changes or close it." }, "/signin?next=%2Facme%2Fapi%2Fchanges%2Fcr-1");
    expect(out).toContain('href="/signin?next=%2Facme%2Fapi%2Fchanges%2Fcr-1"');
    expect(out).not.toContain("<button");
  });
});

describe("who did it", () => {
  it("shows the agent, the person with the mark, model, cost and the prompt digest, and omits what is absent", () => {
    const full = render(h(WhoDidIt, { who: { author: "triage", agent: true, agentId: "p-agent", person: "Mara Okafor", models: ["claude-opus-5"], runtimes: ["Claude Code"], costMicros: 870_000, sessions: 1, revisions: 2, promptDigests: ["b8152e3ad0ed5346a175d0a9506e39bb"], ownChecks: ["bun test passed"], elapsedMs: 0 } }));
    expect(full).toContain(">Agent<");
    expect(full).toMatch(/class="dot"[^>]*><\/span><span>Mara Okafor/);
    expect(full.match(/class="dot"/g)).toHaveLength(1);
    expect(full).toContain("claude-opus-5");
    expect(full).toContain("$0.87");
    expect(full).toContain('data-testid="prompt-digest"');
    expect(full).toContain("b8152e3ad0ed5346a175d0a9506e39bb");
    expect(full).toContain("bun test passed");
    const bare = render(h(WhoDidIt, { who: { author: "Lin Chen", agent: false, person: "Lin Chen", models: [], runtimes: [], costMicros: 0, sessions: 0, revisions: 1, promptDigests: [], ownChecks: [], elapsedMs: 0 } }));
    expect(bare).toContain(">Person<");
    for (const absent of ["Model", "Runtime", "Cost", "Sessions", "Prompt", "Ran itself", "Time"]) expect(bare).not.toContain(">" + absent + "<");
  });
});

describe("what changed, worth a look, checks and team", () => {
  const diff = [diffItem("src/auth/session.ts", "sensitive", ["-old", "+new"]), diffItem("src/billing/a.ts", "logic", ["+x"]), diffItem("docs/a.md", "docs", ["+d"])];

  it("says areas in sentences and links files to the Code tab", () => {
    const html = render(h(WhatChanged, { diff, repo: "acme/api", changeId: "cr-1" }));
    expect(html).toContain("Sensitive changes in src/auth (1 file), without tests.");
    expect(html).toContain('href="/acme/api/changes/cr-1/code#file-src%2Fauth%2Fsession.ts"');
    expect(render(h(WhatChanged, { diff: [] }))).toContain("changes no files");
  });

  it("shows picked hunks read-only, sensitive first", () => {
    const html = render(h(WorthALook, { diff }));
    expect(html.indexOf("src/auth/session.ts")).toBeLessThan(html.indexOf("src/billing/a.ts"));
    expect(html).not.toContain("docs/a.md");
    expect(html).not.toContain("<button");
    expect(render(h(WorthALook, { diff: [diffItem("docs/a.md", "docs", ["+d"])] }))).toContain("Nothing in this revision changes behaviour");
  });

  it("groups checks in words, folding the passed ones", () => {
    const html = render(h(HumanChecks, { repo: "acme/api", checks: [check("test", "CHECK_STATUS_FAILED", { url: "/runs/7" }), check("lint", "CHECK_STATUS_PASSED")] }));
    expect(html).toContain("Test check failed.");
    expect(html).toContain('href="/acme/api/runs/7"');
    expect(html).toContain("<details");
    expect(checkHref("acme/api", "http://ci.example/1")).toBe("/acme/api/runs");
    expect(checkHref("acme/api", "https://ci.example/1")).toBe("https://ci.example/1");
  });

  it("lists who did it with roles, cost and time", () => {
    const html = render(
      h(Team, {
        change: change(),
        reviews: [review(MARA, "VERDICT_APPROVE", 2)],
        sessions: [{ session: "s", runId: "", agentPrincipalId: AGENT.id, model: "claude-opus-5-5", runtime: "claude-code", promptDigest: "", chain: [], changesets: [], changeRequestIds: [], cost: { inputTokens: "0", outputTokens: "0", usdMicros: "1500000" }, createdAt: "2026-10-27T11:00:00Z" }],
      }),
    );
    expect(html).toContain("1 agent and 1 person");
    expect(html).toContain("$1.50");
    expect(html).toContain("Agent on Claude Code");
    expect(html).toContain("Wrote the code and ran a session");
    expect(html).toContain("claude-opus-5-5");
    expect(html).toContain('href="/acme/api/agents/p-agent"');
  });
});
