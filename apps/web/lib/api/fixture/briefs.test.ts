import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";
import { frame } from "../../media/frames";
import { createClient, type BeeClient } from "../client";
import type * as T from "../types";
import { FixtureStore, fixtureRpc } from "./index";
import { sniffEvidence } from "./briefs";

const ANCHOR = Date.UTC(2026, 9, 18, 9, 0, 0);
const api = { fullName: "acme/api" };

function world(clock: () => number = () => 0) {
  const store = new FixtureStore({ anchor: ANCHOR, clock, live: false });
  const as = (token: string) => createClient(fixtureRpc(store, token));
  const asHandle = (handle: string): BeeClient => {
    const id = "tok_" + handle;
    store.tokens.push({ tokenId: id, parentTokenId: "", principalId: store.p(handle).id, revoked: false } as never);
    store.tokenSecrets.set("secret-" + handle + "-0001", id);
    return as("secret-" + handle + "-0001");
  };
  return { store, mara: as("mara-token-0001"), anon: createClient(fixtureRpc(store)), asHandle };
}

async function fails(p: Promise<unknown>): Promise<{ code: string; reason: string }> {
  try {
    await p;
  } catch (e) {
    const err = e as { code: string; reason: string };
    return { code: err.code, reason: err.reason };
  }
  throw new Error("expected the call to fail");
}

const find = async (c: BeeClient, title: string): Promise<T.ChangeRequest> => {
  const cr = (await c.change.listChanges({ repo: api })).changes.find((x) => x.title === title);
  if (!cr) throw new Error("no change " + title);
  return cr;
};

const spec = (kind: T.EvidenceKind, title: string, extra: Partial<T.EvidenceSpec> = {}): T.EvidenceSpec => ({ kind, title, caption: "", benchmark: [], url: "", paths: [], hero: false, order: 0, ...extra });
const b64 = (b: Uint8Array) => Buffer.from(b).toString("base64");
const png = (name: "before" | "after" | "panel" = "after") => frame(name);
const MAIN = "EVIDENCE_ASSET_ROLE_MAIN" as const;
const BEFORE = "EVIDENCE_ASSET_ROLE_BEFORE" as const;
const AFTER = "EVIDENCE_ASSET_ROLE_AFTER" as const;
const shot = (alt = "A settings page.") => ({ role: MAIN, contentType: "image/png", data: b64(png()), alt });

describe("the demo world's briefs and evidence", () => {
  it("gives rotate a brief with an ask, a risk and a rollback, and cache a benchmark", async () => {
    const { mara } = world();
    const rotate = await find(mara, "Rotate session ids on role change");
    expect(rotate.brief?.outcome).toMatch(/signs out/);
    expect(rotate.brief?.asks[0]?.options).toHaveLength(2);
    expect(rotate.brief?.risks[0]?.mitigation).toBeTruthy();
    expect(rotate.brief?.rollback).toBeTruthy();
    expect(rotate.brief?.author?.display).toBe("triage");
    const cache = await find(mara, "Cache invoice totals per customer");
    const items = (await mara.brief.listEvidence({ repo: api, changeIds: [cache.id] })).evidence;
    expect(items[0]?.spec).toMatchObject({ kind: "EVIDENCE_KIND_BENCHMARK", hero: true });
    expect(items[0]?.spec?.benchmark[0]).toMatchObject({ baseline: 120, value: 85, lowerIsBetter: true });
  });

  it("seeds the ui, fix and docs scenarios: an ask and a stale screenshot, a CI test report, nothing owed", async () => {
    const { mara } = world();
    const web = { fullName: "acme/web" };
    const ui = (await mara.change.listChanges({ repo: web })).changes.find((c) => c.title === "Mark the current session in the sessions list");
    if (!ui) throw new Error("no ui change");
    expect(ui.brief?.asks[0]?.options).toHaveLength(2);
    const items = (await mara.brief.listEvidence({ repo: web, changeIds: [ui.id] })).evidence;
    expect(items.map((e) => [e.spec?.kind, e.stale])).toEqual([
      ["EVIDENCE_KIND_BEFORE_AFTER", false],
      ["EVIDENCE_KIND_VIDEO", false],
      ["EVIDENCE_KIND_SCREENSHOT", true],
    ]);
    expect(items[0]?.spec?.hero).toBe(true);
    expect((await mara.brief.getBriefChecklist({ repo: web, changeId: ui.id })).complete).toBe(true);

    const tax = await find(mara, "Fix rounding drift in invoice tax lines");
    const report = (await mara.brief.listEvidence({ repo: api, changeIds: [tax.id] })).evidence[0];
    expect(report).toMatchObject({ source: "EVIDENCE_SOURCE_CHECK_RUN", stale: false, spec: { kind: "EVIDENCE_KIND_TEST_REPORT" } });
    expect(report?.checkRunId).toBeTruthy();

    const docs = await find(mara, "Document the webhook retry schedule");
    expect(docs.brief).toBeUndefined();
    expect((await mara.brief.listEvidence({ repo: api, changeIds: [docs.id] })).evidence).toEqual([]);
    expect(await mara.brief.getBriefChecklist({ repo: api, changeId: docs.id })).toMatchObject({ complete: true, items: [] });
  });

  it("orders hero first, flags older revisions stale and labels the sources", async () => {
    const { mara } = world();
    const rotate = await find(mara, "Rotate session ids on role change");
    const items = (await mara.brief.listEvidence({ repo: api, changeIds: [rotate.id] })).evidence;
    expect(items.map((e) => e.spec?.title)).toEqual(["The page after the change", "Walkthrough", "Auth tests", "Sessions list, before and after"]);
    expect(items.map((e) => e.stale)).toEqual([false, false, false, true]);
    expect(items.find((e) => e.spec?.title === "Auth tests")).toMatchObject({ source: "EVIDENCE_SOURCE_CHECK_RUN", revision: 3 });
    expect(items[0]?.source).toBe("EVIDENCE_SOURCE_UPLOADED");
    expect(items[3]?.revision).toBe(1);
    const hero = (await mara.brief.listEvidence({ repo: api, changeIds: [rotate.id], heroOnly: true })).evidence;
    expect(hero.map((e) => e.spec?.title)).toEqual(["The page after the change"]);
  });

  it("hands out same-origin URLs, never the server's", async () => {
    const { mara } = world();
    const rotate = await find(mara, "Rotate session ids on role change");
    const items = (await mara.brief.listEvidence({ repo: api, changeIds: [rotate.id] })).evidence;
    const assets = items.flatMap((e) => e.assets);
    expect(assets.length).toBe(6);
    for (const a of assets) {
      expect(a.url).toMatch(/^\/api\/evidence\/fixture\/[0-9A-Z]{26}\/(main|before|after|poster|captions)\?exp=\d+&sig=[0-9a-f]{24}&_d=[0-9a-f]{64}$/);
      expect(a.digest).toMatch(/^sha256:[0-9a-f]{64}$/);
    }
    const video = items.find((e) => e.spec?.title === "Walkthrough");
    expect(video?.assets.map((a) => [a.role, a.contentType])).toEqual([
      ["EVIDENCE_ASSET_ROLE_MAIN", "video/mp4"],
      ["EVIDENCE_ASSET_ROLE_POSTER", "image/png"],
      ["EVIDENCE_ASSET_ROLE_CAPTIONS", "text/vtt"],
    ]);
  });

  it("serves the committed post media and generated frames through signed URLs", () => {
    const { store } = world();
    const rotate = store.crByKey("rotate");
    for (const e of store.briefs.forChange(rotate.id)) {
      for (const a of e.assets) {
        const u = store.briefs.signedUrl(store, e, a.role).url;
        const [path, query] = u.replace("/_evidence/", "").split("?");
        const got = store.briefs.redeem(store, (path as string).split("/"), new URLSearchParams(query), "mara");
        expect(got.digest).toBe("sha256:" + a.sha256);
        expect(got.bytes.length).toBe(a.sizeBytes);
        expect(got.contentType).toBe(a.contentType);
      }
    }
  });

  it("uses no clock or randomness, and the generated frames are the same every time", () => {
    const src = readFileSync(new URL("./briefs.ts", import.meta.url), "utf8");
    expect(src).not.toMatch(/Math\.random|Date\.now|new Date\(\)/);
    expect(Buffer.from(frame("after")).equals(Buffer.from(frame("after")))).toBe(true);
    expect(Buffer.from(frame("after")).equals(Buffer.from(frame("before")))).toBe(false);
    expect(sniffEvidence(frame("panel"))?.contentType).toBe("image/png");
  });
});

describe("the brief", () => {
  it("is written by the author or a writer and not by a stranger", async () => {
    const { mara, asHandle, anon } = world();
    const cache = await find(mara, "Cache invoice totals per customer"); // lin's change
    const brief = { outcome: "Totals are faster.", highlights: [], asks: [], risks: [], rollback: "", tryItUrl: "" };
    const lin = asHandle("lin");
    const set = await lin.brief.setChangeBrief({ repo: api, changeId: cache.id, brief });
    expect(set.brief).toMatchObject({ outcome: "Totals are faster.", writtenForRevision: 1 });
    expect(set.brief?.author?.display).toBe("Lin Zhou");
    expect((await mara.change.getChange({ repo: api, id: cache.id })).brief?.outcome).toBe("Totals are faster.");
    // A different human outside the org cannot even see the private repo.
    const outsider = asHandle("rhea");
    expect(await fails(outsider.brief.setChangeBrief({ repo: api, changeId: cache.id, brief }))).toMatchObject({ reason: "REPO_NOT_FOUND" });
    expect(await fails(anon.brief.setChangeBrief({ repo: api, changeId: cache.id, brief }))).toMatchObject({ code: "unauthenticated" });
  });

  it("refuses text over 280 characters, too many items, bad asks and non-https links", async () => {
    const { mara } = world();
    const cr = await find(mara, "Cache invoice totals per customer");
    const base = { outcome: "ok", highlights: [], asks: [], risks: [], rollback: "", tryItUrl: "" };
    const set = (brief: Partial<T.ChangeBrief>) => mara.brief.setChangeBrief({ repo: api, changeId: cr.id, brief: { ...base, ...brief } });
    expect(await fails(set({ outcome: "x".repeat(281) }))).toMatchObject({ code: "invalid_argument", reason: "brief_too_long" });
    expect((await set({ outcome: "é".repeat(280) })).brief?.outcome).toHaveLength(280);
    expect(await fails(set({ highlights: Array.from({ length: 21 }, () => ({ text: "a", paths: [] })) }))).toMatchObject({ reason: "bad_brief" });
    expect(await fails(set({ asks: [{ question: "Ship?", options: ["Yes"] }] }))).toMatchObject({ reason: "bad_brief" });
    expect(await fails(set({ asks: [{ question: "Ship?", options: ["1", "2", "3", "4", "5", "6", "7"] }] }))).toMatchObject({ reason: "bad_brief" });
    expect(await fails(set({ tryItUrl: "http://example.com" }))).toMatchObject({ reason: "bad_url" });
    expect((await set({ tryItUrl: "https://example.com/x" })).brief?.tryItUrl).toBe("https://example.com/x");
  });

  it("honours expected_version and bumps it on every brief", async () => {
    const { mara } = world();
    const cr = await find(mara, "Cache invoice totals per customer");
    const brief = { outcome: "a", highlights: [], asks: [], risks: [], rollback: "", tryItUrl: "" };
    const first = await mara.brief.setChangeBrief({ repo: api, changeId: cr.id, brief });
    const second = await mara.brief.setChangeBrief({ repo: api, changeId: cr.id, brief: { ...brief, outcome: "b" }, expectedVersion: first.version });
    expect(Number(second.version)).toBe(Number(first.version) + 1);
    expect(await fails(mara.brief.setChangeBrief({ repo: api, changeId: cr.id, brief, expectedVersion: first.version }))).toMatchObject({ reason: "VERSION_MISMATCH" });
  });

  it("emits cr.brief_set and takes a brief with OpenChange", async () => {
    const { store, mara } = world();
    const brief = { outcome: "Opened with a brief.", highlights: [{ text: "One thing.", paths: ["a.ts"] }], asks: [], risks: [], rollback: "Revert it.", tryItUrl: "" };
    const opened = await mara.change.openChange({ repo: api, targetRef: "main", title: "Opened with a brief", body: "", brief });
    expect(opened.brief?.highlights[0]?.text).toBe("One thing.");
    expect(opened.brief?.writtenForRevision).toBe(1);
    const before = store.events.length;
    await mara.brief.setChangeBrief({ repo: api, changeId: opened.id, brief: { ...brief, outcome: "Changed." } });
    expect(store.events.slice(before).map((e) => e.kind)).toContain("cr.brief_set");
  });
});

describe("evidence", () => {
  it("adds a screenshot, reads it back and emits evidence.added", async () => {
    const { store, mara } = world();
    const cr = await find(mara, "Cache invoice totals per customer");
    const before = store.events.length;
    const e = await mara.brief.addEvidence({ repo: api, changeId: cr.id, spec: spec("EVIDENCE_KIND_SCREENSHOT", "The settings page"), assets: [shot()] });
    expect(e).toMatchObject({ state: "EVIDENCE_STATE_READY", revision: 1, stale: false, source: "EVIDENCE_SOURCE_UPLOADED" });
    expect(e.assets[0]).toMatchObject({ contentType: "image/png", width: 640, height: 400, alt: "A settings page." });
    expect((await mara.brief.getEvidence({ repo: api, id: e.id })).id).toBe(e.id);
    const ev = store.events.slice(before).find((x) => x.kind === "evidence.added");
    expect(ev?.note?.target).toBe("evidence:" + e.id);
  });

  it("refuses SVG, HTML and script, bytes that are not the declared type, and images without alt text", async () => {
    const { mara } = world();
    const cr = await find(mara, "Cache invoice totals per customer");
    const add = (data: Uint8Array, contentType = "image/png", alt = "alt") => mara.brief.addEvidence({ repo: api, changeId: cr.id, spec: spec("EVIDENCE_KIND_SCREENSHOT", "x"), assets: [{ role: MAIN, contentType, data: b64(data), alt }] });
    const text = (s: string) => new TextEncoder().encode(s);
    expect(await fails(add(text('<svg xmlns="http://www.w3.org/2000/svg"><script>1</script></svg>'), "image/svg+xml"))).toMatchObject({ reason: "bad_media" });
    expect(await fails(add(text("<html><script>alert(1)</script></html>"), "text/html"))).toMatchObject({ reason: "bad_media" });
    expect(await fails(add(text("alert(1)"), "image/png"))).toMatchObject({ reason: "bad_media" });
    expect(await fails(add(png(), "image/jpeg"))).toMatchObject({ reason: "bad_media" });
    expect(await fails(add(png(), "image/png", "  "))).toMatchObject({ reason: "missing_alt" });
  });

  it("checks the files against the kind", async () => {
    const { mara } = world();
    const cr = await find(mara, "Cache invoice totals per customer");
    const add = (kind: T.EvidenceKind, assets: T.InlineEvidenceAsset[], extra: Partial<T.EvidenceSpec> = {}) => mara.brief.addEvidence({ repo: api, changeId: cr.id, spec: spec(kind, "t", extra), assets });
    expect(await fails(add("EVIDENCE_KIND_SCREENSHOT", [shot(), shot()]))).toMatchObject({ reason: "bad_evidence" });
    expect(await fails(add("EVIDENCE_KIND_BEFORE_AFTER", [{ ...shot(), role: BEFORE }]))).toMatchObject({ reason: "missing_asset" });
    expect(await fails(add("EVIDENCE_KIND_BENCHMARK", []))).toMatchObject({ reason: "bad_evidence" });
    expect(await fails(add("EVIDENCE_KIND_BENCHMARK", [shot()], { benchmark: [{ name: "a", unit: "ms", baseline: 1, value: 2, lowerIsBetter: true }] }))).toMatchObject({ reason: "bad_evidence" });
    expect(await fails(add("EVIDENCE_KIND_PREVIEW_LINK", [], { url: "http://insecure.example" }))).toMatchObject({ reason: "bad_url" });
    expect(await fails(add("EVIDENCE_KIND_TEST_REPORT", []))).toMatchObject({ reason: "bad_evidence" });
    expect(await fails(add("EVIDENCE_KIND_IMAGE", Array.from({ length: 13 }, () => shot())))).toMatchObject({ code: "invalid_argument" });
    const ok = await add("EVIDENCE_KIND_BEFORE_AFTER", [
      { ...shot("Before"), role: BEFORE },
      { ...shot("After"), role: AFTER },
    ]);
    expect(ok.assets.map((a) => a.role)).toEqual([BEFORE, AFTER]);
    const link = await add("EVIDENCE_KIND_PREVIEW_LINK", [], { url: "https://preview.example/pr-1" });
    expect(link.spec?.url).toBe("https://preview.example/pr-1");
  });

  it("stops at 8 MiB inline and at 64 items", async () => {
    const { mara } = world();
    const cr = await find(mara, "Cache invoice totals per customer");
    const big = new Uint8Array(9 * 1024 * 1024);
    big.set(png().subarray(0, 8));
    expect(await fails(mara.brief.addEvidence({ repo: api, changeId: cr.id, spec: spec("EVIDENCE_KIND_SCREENSHOT", "x"), assets: [{ ...shot(), data: b64(big) }] }))).toMatchObject({ code: "resource_exhausted", reason: "TOO_LARGE" });
    for (let i = 0; i < 62; i++) await mara.brief.addEvidence({ repo: api, changeId: cr.id, spec: spec("EVIDENCE_KIND_PREVIEW_LINK", "link " + i, { url: "https://p.example/" + i }) });
    expect(await fails(mara.brief.addEvidence({ repo: api, changeId: cr.id, spec: spec("EVIDENCE_KIND_PREVIEW_LINK", "one more", { url: "https://p.example/x" }) }))).toMatchObject({ reason: "TOO_LARGE" });
  });

  it("keeps one hero per change request", async () => {
    const { mara } = world();
    const cr = await find(mara, "Cache invoice totals per customer");
    const second = await mara.brief.addEvidence({ repo: api, changeId: cr.id, spec: spec("EVIDENCE_KIND_PREVIEW_LINK", "Preview", { url: "https://p.example/1", hero: true }) });
    const all = (await mara.brief.listEvidence({ repo: api, changeIds: [cr.id] })).evidence;
    expect(all.filter((e) => e.spec?.hero).map((e) => e.id)).toEqual([second.id]);
    expect(all[0]?.id).toBe(second.id);
  });

  it("uploads in three steps and refuses a digest that does not match", async () => {
    const { store, mara } = world();
    const cr = await find(mara, "Cache invoice totals per customer");
    const bytes = png("before");
    const begin = await mara.brief.beginEvidenceUpload({
      repo: api,
      changeId: cr.id,
      spec: spec("EVIDENCE_KIND_SCREENSHOT", "Uploaded"),
      assets: [{ role: MAIN, contentType: "image/png", sizeBytes: String(bytes.length), alt: "The before frame." }],
    });
    expect(begin.evidence).toMatchObject({ state: "EVIDENCE_STATE_PENDING" });
    expect(begin.uploads[0]?.uploadUrl).toMatch(/^\/api\/evidence\//);
    // Nothing is listed until it is finalized.
    expect((await mara.brief.listEvidence({ repo: api, changeIds: [cr.id] })).evidence.some((e) => e.id === begin.evidence?.id)).toBe(false);
    const id = begin.evidence?.id as string;
    expect(await fails(mara.brief.finalizeEvidence({ repo: api, evidenceId: id, digests: [{ role: MAIN, sha256: "0".repeat(64) }] }))).toMatchObject({ reason: "missing_asset" });
    store.briefs.receive(id, MAIN, bytes, store);
    expect(await fails(mara.brief.finalizeEvidence({ repo: api, evidenceId: id, digests: [{ role: MAIN, sha256: "0".repeat(64) }] }))).toMatchObject({ reason: "INTEGRITY" });
    const { createHash } = await import("node:crypto");
    const done = await mara.brief.finalizeEvidence({ repo: api, evidenceId: id, digests: [{ role: MAIN, sha256: createHash("sha256").update(bytes).digest("hex") }] });
    expect(done.state).toBe("EVIDENCE_STATE_READY");
    expect(done.assets[0]).toMatchObject({ width: 640, height: 400 });
    expect(await fails(mara.brief.finalizeEvidence({ repo: api, evidenceId: id, digests: [] }))).toMatchObject({ reason: "INVALID_STATE" });
  });

  it("drops uploads left pending for an hour", async () => {
    let t = 0;
    const { store, mara } = world(() => t);
    const cr = await find(mara, "Cache invoice totals per customer");
    const begin = await mara.brief.beginEvidenceUpload({ repo: api, changeId: cr.id, spec: spec("EVIDENCE_KIND_SCREENSHOT", "Stalled"), assets: [{ role: MAIN, contentType: "image/png", sizeBytes: "10", alt: "x" }] });
    t = 61 * 60_000;
    await mara.brief.beginEvidenceUpload({ repo: api, changeId: cr.id, spec: spec("EVIDENCE_KIND_SCREENSHOT", "Another"), assets: [{ role: MAIN, contentType: "image/png", sizeBytes: "10", alt: "x" }] });
    expect(store.briefs.byId(begin.evidence?.id)).toBeUndefined();
  });

  it("updates the spec and alt text but never the kind, and deletes", async () => {
    const { store, mara } = world();
    const cr = await find(mara, "Cache invoice totals per customer");
    const e = await mara.brief.addEvidence({ repo: api, changeId: cr.id, spec: spec("EVIDENCE_KIND_SCREENSHOT", "Old title"), assets: [shot("Old alt")] });
    const up = await mara.brief.updateEvidence({ repo: api, id: e.id, spec: spec("EVIDENCE_KIND_SCREENSHOT", "New title", { caption: "Now with words" }), altTexts: [{ role: MAIN, alt: "New alt" }] });
    expect(up.spec).toMatchObject({ title: "New title", caption: "Now with words" });
    expect(up.assets[0]?.alt).toBe("New alt");
    expect(await fails(mara.brief.updateEvidence({ repo: api, id: e.id, spec: spec("EVIDENCE_KIND_VIDEO", "x") }))).toMatchObject({ reason: "kind_immutable" });
    expect(await fails(mara.brief.updateEvidence({ repo: api, id: e.id, spec: spec("EVIDENCE_KIND_SCREENSHOT", "x"), altTexts: [{ role: MAIN, alt: "" }] }))).toMatchObject({ reason: "missing_alt" });
    const before = store.events.length;
    await mara.brief.deleteEvidence({ repo: api, id: e.id });
    expect(await fails(mara.brief.getEvidence({ repo: api, id: e.id }))).toMatchObject({ code: "not_found" });
    expect(store.events.slice(before).map((x) => x.kind)).toContain("evidence.deleted");
  });

  it("lets only the reporter or a maintainer delete CI evidence", async () => {
    const { store, mara, asHandle } = world();
    const rotate = await find(mara, "Rotate session ids on role change");
    const ci = (await mara.brief.listEvidence({ repo: api, changeIds: [rotate.id] })).evidence.find((e) => e.source === "EVIDENCE_SOURCE_CHECK_RUN") as T.Evidence;
    // sam is a member with write access, but neither the reporter (triage) nor a maintainer.
    expect(store.p("sam").kind).toBe("PRINCIPAL_KIND_HUMAN");
    expect(await fails(asHandle("sam").brief.deleteEvidence({ repo: api, id: ci.id }))).toMatchObject({ code: "permission_denied" });
    await mara.brief.deleteEvidence({ repo: api, id: ci.id });
  });

  it("attaches CI evidence to the revision the check run tested", async () => {
    const { store, mara } = world();
    const rotate = store.crByKey("rotate");
    const r2 = rotate.revisions[1] as (typeof rotate.revisions)[number];
    const check = [...store.checks.values()].flat().find((c) => c.changeset?.raw === r2.head.id.raw);
    const id = (check as T.CheckRun).id;
    const e = await mara.brief.addEvidence({ repo: api, changeId: rotate.id, spec: spec("EVIDENCE_KIND_TEST_REPORT", "r2 tests", { testReport: { passed: 3, failed: 0, skipped: 0, durationMs: "10", failures: [], summary: "3 pass" } }), checkRunId: id });
    expect(e).toMatchObject({ revision: 2, source: "EVIDENCE_SOURCE_CHECK_RUN", checkRunId: id, stale: true });
    expect(await fails(mara.brief.addEvidence({ repo: api, changeId: rotate.id, spec: spec("EVIDENCE_KIND_PREVIEW_LINK", "x", { url: "https://p.example" }), checkRunId: "nope" }))).toMatchObject({ code: "not_found" });
  });
});

describe("the checklist", () => {
  /** A UI change: revises the cache change with a component file of logic. */
  async function uiChange() {
    const w = world();
    const cache = w.store.crByKey("cache");
    w.store.revise(cache, { "src/ui/Totals.tsx": "export function Totals() {\n  const total = 1 + 1;\n  return <b>{total}</b>;\n}\n" }, "lin");
    return { ...w, cache };
  }

  it("asks agent-written logic changes for a brief, and sensitive paths for a risk or rollback", async () => {
    const { mara } = world();
    const rotate = await find(mara, "Rotate session ids on role change");
    const list = await mara.brief.getBriefChecklist({ repo: api, changeId: rotate.id });
    expect(list.revision).toBe(3);
    const ids = list.items.map((i) => i.id);
    expect(ids).toContain("brief");
    expect(list.items.every((i) => i.satisfied)).toBe(true);
    expect(list.complete).toBe(true);
    // Without the brief the same revision owes both.
    const { store, mara: m2 } = world();
    const cr = store.crByKey("rotate");
    store.briefs.briefs.delete(cr.id);
    const owed = await m2.brief.getBriefChecklist({ repo: api, changeId: cr.id });
    expect(owed.complete).toBe(false);
    expect(owed.items.filter((i) => !i.satisfied).map((i) => i.id)).toEqual(expect.arrayContaining(["brief"]));
    expect(owed.items.find((i) => i.id === "brief")?.detail).toBe("an outcome and at least one highlight");
  });

  it("asks a UI change for a screenshot, before and after, or video, and is satisfied by one for that revision", async () => {
    const { mara, cache } = await uiChange();
    const owed = await mara.brief.getBriefChecklist({ repo: api, changeId: cache.id });
    const ui = owed.items.find((i) => i.id === "evidence:ui");
    expect(ui).toMatchObject({ satisfied: false, paths: ["src/ui/Totals.tsx"], detail: "a screenshot, before and after, or video of the UI change" });
    expect(ui?.kinds).toEqual(["EVIDENCE_KIND_SCREENSHOT", "EVIDENCE_KIND_BEFORE_AFTER", "EVIDENCE_KIND_VIDEO"]);
    // The benchmark and profiles on r1 do not count, and a screenshot of r1 does not satisfy r2.
    const old = await mara.brief.addEvidence({ repo: api, changeId: cache.id, spec: spec("EVIDENCE_KIND_SCREENSHOT", "r1"), assets: [shot()], revision: 1 });
    expect((await mara.brief.getBriefChecklist({ repo: api, changeId: cache.id })).items.find((i) => i.id === "evidence:ui")?.satisfied).toBe(false);
    const fresh = await mara.brief.addEvidence({ repo: api, changeId: cache.id, spec: spec("EVIDENCE_KIND_SCREENSHOT", "r2"), assets: [shot()] });
    const done = (await mara.brief.getBriefChecklist({ repo: api, changeId: cache.id })).items.find((i) => i.id === "evidence:ui");
    expect(done).toMatchObject({ satisfied: true, evidenceIds: [fresh.id] });
    expect(old.stale).toBe(true);
    expect(fresh.stale).toBe(false);
  });

  it("goes stale after a new revision", async () => {
    const { store, mara } = world();
    const cache = store.crByKey("cache");
    const e = await mara.brief.addEvidence({ repo: api, changeId: cache.id, spec: spec("EVIDENCE_KIND_SCREENSHOT", "now"), assets: [shot()] });
    expect(e.stale).toBe(false);
    store.revise(cache, { "src/ui/Totals.tsx": "export const Totals = () => <b>2</b>;\n" }, "lin");
    expect((await mara.brief.getEvidence({ repo: api, id: e.id })).stale).toBe(true);
  });

  it("names a revision that does not exist", async () => {
    const { mara } = world();
    const cr = await find(mara, "Cache invoice totals per customer");
    expect(await fails(mara.brief.getBriefChecklist({ repo: api, changeId: cr.id, revision: 9 }))).toMatchObject({ reason: "UNKNOWN_REVISION" });
  });
});

describe("the conversation and the sessions", () => {
  it("lists comments oldest first and takes a decision as an ordinary comment", async () => {
    const { mara } = world();
    const rotate = await find(mara, "Rotate session ids on role change");
    const before = (await mara.change.listComments({ repo: api, changeRequestId: rotate.id })).comments;
    expect(before.map((c) => c.author?.display)).toEqual(["Mara Okafor", "triage"]);
    expect(before[0]?.body).toBe('Decision on "Sign out other devices when a role changes?": Only this session');
    const body = 'Decision on "Roll out gradually?": Yes';
    const made = await mara.change.commentChange({ repo: api, changeRequestId: rotate.id, body });
    expect(made).toMatchObject({ body, author: { display: "Mara Okafor" } });
    const after = (await mara.change.listComments({ repo: api, changeRequestId: rotate.id })).comments;
    expect(after.map((c) => c.id)).toEqual([...before.map((c) => c.id), made.id]);
    expect(await fails(mara.change.commentChange({ repo: api, changeRequestId: rotate.id, body: "  " }))).toMatchObject({ code: "invalid_argument" });
  });

  it("tells which agent sessions produced a change request", async () => {
    const { mara } = world();
    const rotate = await find(mara, "Rotate session ids on role change");
    const res = await mara.agent.getSessionsForChange({ repo: api, changeRequest: rotate.id });
    expect(res.sessions).toHaveLength(1);
    expect(res.sessions[0]).toMatchObject({ session: "ses_01JACtriage4d0", model: "claude-opus-5", changeRequestIds: [rotate.id] });
    expect(res.sessions[0]?.changesets).toHaveLength(3);
    expect(Number(res.sessions[0]?.cost?.usdMicros)).toBe((52 + 33 + 2) * 10_000);
    const byHead = await mara.agent.getSessionsForChange({ repo: api, changeset: rotate.revisions[1]?.head });
    expect(byHead.sessions[0]?.changesets).toHaveLength(1);
    // A change by a person has no sessions; naming neither is an error.
    const cache = await find(mara, "Cache invoice totals per customer");
    expect((await mara.agent.getSessionsForChange({ repo: api, changeRequest: cache.id })).sessions).toEqual([]);
    expect(await fails(mara.agent.getSessionsForChange({ repo: api }))).toMatchObject({ code: "invalid_argument" });
  });
});

describe("who can read what", () => {
  it("shows a signed-out visitor a public repo's evidence and checklist, and hides private repos", async () => {
    const { anon, mara } = world();
    const engine = { fullName: "lattice/engine" };
    const cr = (await anon.change.listChanges({ repo: engine })).changes[0] as T.ChangeRequest;
    expect((await anon.brief.listEvidence({ repo: engine, changeIds: [cr.id] })).evidence).toEqual([]);
    expect((await anon.brief.getBriefChecklist({ repo: engine, changeId: cr.id })).complete).toBe(true);
    const rotate = await find(mara, "Rotate session ids on role change");
    expect(await fails(anon.brief.listEvidence({ repo: api, changeIds: [rotate.id] }))).toMatchObject({ reason: "REPO_NOT_FOUND" });
  });

  it("decides again when a signed URL is redeemed", () => {
    let t = 0;
    const { store } = world(() => t);
    const e = store.briefs.forChange(store.crByKey("rotate").id)[0] as NonNullable<ReturnType<typeof store.briefs.byId>>;
    const u = store.briefs.signedUrl(store, e, e.assets[0]?.role as never).url;
    const [path, query] = u.replace("/_evidence/", "").split("?");
    const segs = (path as string).split("/");
    const q = () => new URLSearchParams(query);
    expect(store.briefs.redeem(store, segs, q(), "mara").bytes.length).toBeGreaterThan(0);
    // Signed out: the repo is private, so it does not exist.
    expect(() => store.briefs.redeem(store, segs, q(), undefined)).toThrowError(/no evidence/);
    // Tampered signature.
    const bad = q();
    bad.set("sig", "0".repeat(24));
    expect(() => store.briefs.redeem(store, segs, bad, "mara")).toThrowError(/not valid/);
    // Another asset's path with this URL's signature.
    expect(() => store.briefs.redeem(store, [segs[0] as string, segs[1] as string, "poster"], q(), "mara")).toThrowError();
    // After the quarter hour.
    t = 16 * 60_000;
    expect(() => store.briefs.redeem(store, segs, q(), "mara")).toThrowError(/expired/);
  });
});
