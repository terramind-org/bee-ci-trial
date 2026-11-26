import { createElement as h } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it, vi } from "vitest";
import { createClient, type BeeClient } from "@/lib/api/client";
import type { ChangeRequest, Evidence, Post, Principal } from "@/lib/api/types";
import { FixtureStore, fixtureRpc } from "./api/fixture";
import {
  MAX_TEXT,
  canAdd,
  counterTone,
  draftProblem,
  evidenceChoices,
  mediaProblem,
  panelMode,
  panelViewer,
  parseUploadSegments,
  previewPost,
  withRepo,
  stateLine,
  textLength,
  uploadPath,
  type PanelViewer,
} from "./post-compose";
import { uploadPostMedia } from "./post-media";

// The panel calls router.refresh() after a write; rendering it to markup needs no router.
vi.mock("next/navigation", () => ({ useRouter: () => ({ refresh() {} }) }));
const { PostPanel } = await import("@/components/post/post-panel");

const human: Principal = { id: "p_mara", kind: "PRINCIPAL_KIND_HUMAN", display: "mara", ownerId: "", runtime: "", externalIds: [] };
const agent: Principal = { id: "p_kit", kind: "PRINCIPAL_KIND_AGENT", display: "kit", ownerId: "p_mara", runtime: "claude-code", externalIds: [] };
const IMG = "POST_MEDIA_KIND_IMAGE" as const;
const VID = "POST_MEDIA_KIND_VIDEO" as const;

function post(over: Partial<Post> = {}): Post {
  return {
    id: "post1",
    repo: { fullName: "lattice/engine" },
    changeId: "cr1",
    changeNumber: "31",
    changeTitle: "Add the tool registry",
    author: human,
    text: "Tools register once.",
    media: [],
    state: "POST_STATE_DRAFT",
    counts: { likes: "0", reposts: "0", comments: "0", adoptions: "0" },
    viewer: { liked: false, reposted: false, canEdit: true, canApprove: false, canWithdraw: false },
    version: "2",
    withdrawnReason: "",
    ...over,
  };
}

const reader: PanelViewer = { signedIn: true, canEdit: false, canApprove: false, canWithdraw: false, involved: false };
const writer: PanelViewer = { ...reader, canEdit: true, involved: true };
const change = (p?: Post): Pick<ChangeRequest, "id" | "number" | "title" | "state" | "post"> => ({ id: "cr1", number: "31", title: "Add the tool registry", state: "CHANGE_STATE_OPEN", post: p });
const render = (p: Post | undefined, viewer: PanelViewer) => renderToStaticMarkup(h(PostPanel, { repo: "lattice/engine", change: change(p), viewer, me: human }));

describe("the composer's rules", () => {
  it("counts scalar values of the NFC text and turns amber near the limit and red past it", () => {
    expect(textLength("é")).toBe(1);
    expect(textLength("🚀🚀")).toBe(2);
    expect(counterTone(0)).toBe("ok");
    expect(counterTone(MAX_TEXT - 50)).toBe("ok");
    expect(counterTone(MAX_TEXT - 49)).toBe("near");
    expect(counterTone(MAX_TEXT)).toBe("near");
    expect(counterTone(MAX_TEXT + 1)).toBe("over");
  });

  it("takes 1 to 4 described images or one video alone", () => {
    const img = (alt = "a shot") => ({ kind: IMG, alt });
    expect(mediaProblem([])).toBeNull();
    expect(mediaProblem([img(), img(), img(), img()])).toBeNull();
    expect(mediaProblem([img(), img(), img(), img(), img()])).toMatch(/at most 4/);
    expect(mediaProblem([{ kind: VID, alt: "" }])).toBeNull();
    expect(mediaProblem([{ kind: VID, alt: "" }, img()])).toMatch(/video goes alone/);
    expect(mediaProblem([{ kind: VID, alt: "" }, { kind: VID, alt: "" }])).toMatch(/one video/);
    expect(mediaProblem([img(" ")])).toMatch(/Describe the image/);
    expect(canAdd([], VID)).toBe(true);
    expect(canAdd([img()], VID)).toBe(false);
    expect(canAdd([{ kind: VID }], IMG)).toBe(false);
    expect(canAdd([img(), img(), img()], IMG)).toBe(true);
    expect(canAdd([img(), img(), img(), img()], IMG)).toBe(false);
  });

  it("refuses what the server refuses in text: too long, control characters, empty without media", () => {
    expect(draftProblem("x".repeat(MAX_TEXT), [])).toBeNull();
    expect(draftProblem("x".repeat(MAX_TEXT + 3), [])).toMatch(/3 characters over/);
    expect(draftProblem("tab\there", [])).toMatch(/control characters/);
    expect(draftProblem("two\nlines", [])).toBeNull();
    expect(draftProblem("  ", [])).toMatch(/Write something/);
    expect(draftProblem("", [{ kind: IMG, alt: "a shot" }])).toBeNull();
  });
});

describe("the state line", () => {
  const at = "/lattice/engine/posts/post1";
  it("says what happens next for each state", () => {
    expect(stateLine(null, at).text).toMatch(/No post yet/);
    expect(stateLine(post(), at)).toEqual({ tone: "neutral", text: "Publishes when this change merges" });
    expect(stateLine(post({ author: agent }), at)).toEqual({ tone: "warn", text: "Needs a person to approve before it publishes" });
    expect(stateLine(post({ author: agent, approvedBy: human }), at)).toEqual({ tone: "neutral", text: "Publishes when this change merges", approvedBy: "mara" });
    expect(stateLine(post({ state: "POST_STATE_PENDING_APPROVAL", author: agent }), at).text).toBe("Merged. Waiting for a person to approve this post");
    expect(stateLine(post({ state: "POST_STATE_PUBLISHED" }), at)).toEqual({ tone: "ok", text: "Published", href: at });
    expect(stateLine(post({ state: "POST_STATE_WITHDRAWN", withdrawnReason: "Wrong numbers" }), at).text).toBe("Withdrawn: Wrong numbers");
    expect(stateLine(post({ state: "POST_STATE_WITHDRAWN" }), at).text).toBe("Withdrawn");
    expect(stateLine(post({ state: "POST_STATE_DISCARDED" }), at).text).toMatch(/^Discarded/);
  });
});

describe("who sees what", () => {
  it("builds the viewer from the post's own permissions, or from the change when there is no post", () => {
    const cr = { state: "CHANGE_STATE_OPEN" as const, author: agent };
    expect(panelViewer(cr, human, true)).toEqual({ signedIn: true, canEdit: true, canApprove: false, canWithdraw: false, involved: true });
    expect(panelViewer(cr, human, false)).toMatchObject({ canEdit: false, involved: false });
    expect(panelViewer({ ...cr, author: human }, human, false)).toMatchObject({ canEdit: true, involved: true });
    expect(panelViewer({ ...cr, state: "CHANGE_STATE_LANDED" }, human, true)).toMatchObject({ canEdit: false, involved: true });
    expect(panelViewer(cr, null, false)).toMatchObject({ signedIn: false, canEdit: false });
    const p = post({ author: agent, viewer: { liked: false, reposted: false, canEdit: false, canApprove: true, canWithdraw: false } });
    expect(panelViewer({ ...cr, post: p }, human, true)).toEqual({ signedIn: true, canEdit: false, canApprove: true, canWithdraw: false, involved: true });
  });

  it("gives writers the composer, approvers the post, readers a published card and nothing else", () => {
    expect(panelMode(null, writer)).toBe("compose");
    expect(panelMode(post(), { ...reader, canApprove: true })).toBe("manage");
    expect(panelMode(post({ state: "POST_STATE_WITHDRAWN" }), { ...reader, involved: true })).toBe("manage");
    expect(panelMode(post({ state: "POST_STATE_PUBLISHED" }), reader)).toBe("card");
    expect(panelMode(post(), reader)).toBe("hidden");
    expect(panelMode(post({ state: "POST_STATE_PENDING_APPROVAL" }), reader)).toBe("hidden");
    expect(panelMode(null, reader)).toBe("hidden");
  });

  it("renders the composer with its counter, and Approve and Withdraw only when the viewer may", () => {
    const html = render(post({ author: agent }), { ...writer, canApprove: true });
    expect(html).toContain('data-testid="post-composer"');
    expect(html).toContain("Needs a person to approve before it publishes");
    expect(html).toMatch(/data-testid="post-counter" data-tone="ok"><span class="num">20<\/span>\/<span class="num">500<\/span>/);
    expect(html).toContain("Approve post");
    expect(html).not.toContain("Withdraw post");
    expect(html).toContain('aria-label="More actions for the post"');

    const published = render(post({ state: "POST_STATE_PUBLISHED" }), { ...reader, canWithdraw: true });
    expect(published).toContain("Withdraw post");
    expect(published).not.toContain("Approve post");
    expect(published).not.toContain('data-testid="post-composer"');
    expect(published).toContain('href="/lattice/engine/posts/post1"');
  });

  it("shows a reader a published post as a card, and nothing before it publishes", () => {
    const card = render(post({ state: "POST_STATE_PUBLISHED" }), reader);
    expect(card).toContain('data-mode="card"');
    expect(card).toContain('data-testid="post-card"');
    expect(card).not.toContain("post-state");
    expect(render(post(), reader)).toBe("");
    expect(render(undefined, { ...reader, signedIn: false })).toBe("");
  });

  it("counts amber near the limit and red past it in the rendered counter", () => {
    expect(render(post({ text: "x".repeat(470) }), writer)).toContain('data-tone="near"');
    expect(render(post({ text: "x".repeat(501) }), writer)).toContain('data-tone="over"');
  });
});

describe("Use evidence", () => {
  const asset = (over: Partial<Evidence["assets"][number]>) => ({ role: "EVIDENCE_ASSET_ROLE_MAIN" as const, contentType: "image/png", digest: "", sizeBytes: "10", width: 640, height: 400, durationMs: "0", alt: "The settings page", url: "/api/evidence/e1/0", ...over });
  const ev = (over: Partial<Evidence>): Evidence => ({
    id: "e1",
    repoId: "r",
    changeRequestId: "cr1",
    revision: 1,
    assets: [asset({})],
    source: "EVIDENCE_SOURCE_UPLOADED",
    checkRunId: "",
    workflowArtifactId: "",
    state: "EVIDENCE_STATE_READY",
    stale: false,
    spec: { kind: "EVIDENCE_KIND_SCREENSHOT", title: "Settings", caption: "", benchmark: [], url: "", paths: [], hero: true, order: 0 },
    ...over,
  });

  it("lists ready images and videos a post takes, leaving out posters, captions, pdfs and pending items", () => {
    const items = [
      ev({}),
      ev({ id: "e2", spec: { kind: "EVIDENCE_KIND_BEFORE_AFTER", title: "Toggle", caption: "", benchmark: [], url: "", paths: [], hero: false, order: 1 }, assets: [asset({ role: "EVIDENCE_ASSET_ROLE_BEFORE" }), asset({ role: "EVIDENCE_ASSET_ROLE_AFTER", alt: "" })] }),
      ev({ id: "e3", assets: [asset({ contentType: "video/mp4", alt: "" }), asset({ role: "EVIDENCE_ASSET_ROLE_POSTER" }), asset({ role: "EVIDENCE_ASSET_ROLE_CAPTIONS", contentType: "text/vtt" })], stale: true }),
      ev({ id: "e4", assets: [asset({ contentType: "application/pdf" })] }),
      ev({ id: "e5", state: "EVIDENCE_STATE_PENDING" }),
      ev({ id: "e6", assets: [asset({ contentType: "image/svg+xml" })] }),
      ev({ id: "e7", spec: { kind: "EVIDENCE_KIND_IMAGE", title: "Profiles", caption: "", benchmark: [], url: "", paths: [], hero: false, order: 2 }, assets: [asset({}), asset({})] }),
    ];
    const out = evidenceChoices(items);
    expect(out.map((c) => [c.key, c.title, c.kind])).toEqual([
      ["e1:0", "Settings", IMG],
      ["e2:0", "Toggle (before)", IMG],
      ["e2:1", "Toggle (after)", IMG],
      ["e3:0", "Settings", VID],
      ["e7:0", "Profiles (1 of 2)", IMG],
      ["e7:1", "Profiles (2 of 2)", IMG],
    ]);
    expect(out[0]?.alt).toBe("The settings page");
    expect(out[3]?.stale).toBe(true);
  });

  it("lists the seeded change's evidence through the fixture's ListEvidence", async () => {
    const { mara } = world();
    const cr = (await mara.change.listChanges({ repo: { fullName: "acme/api" } })).changes.find((c) => c.title === "Rotate session ids on role change")!;
    const { evidence } = await mara.brief.listEvidence({ repo: { fullName: "acme/api" }, changeIds: [cr.id] });
    const choices = evidenceChoices(evidence);
    expect(choices.some((c) => c.kind === IMG)).toBe(true);
    expect(choices.some((c) => c.kind === VID)).toBe(true);
    expect(choices.every((c) => c.url.startsWith("/api/evidence/"))).toBe(true);
  });
});

describe("the upload address", () => {
  it("brings the server's and the fixture's signed URLs to the same-origin route, keeping the query", () => {
    expect(uploadPath("https://bee.example/posts/r_01/m_02?expires=9&signature=ab")).toBe("/api/post-media/r_01/m_02?expires=9&signature=ab");
    expect(uploadPath("/posts/r_01/m_02?expires=9&signature=ab")).toBe("/api/post-media/r_01/m_02?expires=9&signature=ab");
    expect(uploadPath("/api/fixture/posts/r_01/m_02")).toBe("/api/post-media/r_01/m_02");
    expect(uploadPath("https://evil.example/_evidence/x")).toBeNull();
    expect(uploadPath("/posts/a/b/c")).toBeNull();
    expect(uploadPath("/posts/..%2F/b")).toBeNull();
    expect(parseUploadSegments(["r_01", "m_02"])).toEqual({ repoId: "r_01", mediaId: "m_02" });
    expect(parseUploadSegments(["r_01", "../x"])).toBeNull();
  });
});

describe("links", () => {
  it("names the post's repo when the server identified it by id, so links are /owner/repo/posts/…", () => {
    const byId = post({ state: "POST_STATE_PUBLISHED", repo: { id: "r_01" } as Post["repo"] });
    expect(withRepo(byId, "lattice/engine").repo).toEqual({ fullName: "lattice/engine" });
    const html = renderToStaticMarkup(h(PostPanel, { repo: "lattice/engine", change: change(byId), viewer: { ...reader, canWithdraw: true }, me: human }));
    expect(html).toContain('href="/lattice/engine/posts/post1"');
    expect(html).not.toContain('href="//posts/');
  });
});

describe("the live preview", () => {
  it("is the saved post with the composer's text and media, by the person about to save it", () => {
    const p = previewPost({ id: "cr1", number: "31", title: "Add the tool registry" }, "lattice/engine", post({ author: agent }), "café", [{ id: "new-1", kind: IMG, url: "blob:x", alt: "A shot", width: 10, height: 5, contentType: "image/png" }], human);
    expect(p).toMatchObject({ id: "post1", text: "café", state: "POST_STATE_PUBLISHED", author: human, changeNumber: "31", repo: { fullName: "lattice/engine" } });
    expect(p.media[0]).toMatchObject({ id: "new-1", url: "blob:x", altText: "A shot", width: 10 });
    expect(previewPost({ id: "cr1", number: "31", title: "t" }, "lattice/engine", null, "", [], undefined).id).toBe("preview");
  });
});

describe("the panel against the fixture", () => {
  it("lets a person approve an agent's draft, and saves a draft with media through the same-origin path", async () => {
    const { store, mara } = world();
    const api = { fullName: "acme/api" };
    const crs = (await mara.change.listChanges({ repo: api })).changes;
    const rotate = crs.find((c) => c.title === "Rotate session ids on role change")!;
    const me = (await mara.auth.whoAmI({})).principal ?? null;
    const viewer = panelViewer(rotate, me, true);
    expect(rotate.post?.author?.kind).not.toBe("PRINCIPAL_KIND_HUMAN");
    expect(viewer).toMatchObject({ canEdit: true, canApprove: true });
    expect(stateLine(rotate.post, "").text).toBe("Needs a person to approve before it publishes");

    // A PUT the composer would send: through uploadPath, handed to the store as the route does.
    const open = crs.find((c) => c.state === "CHANGE_STATE_OPEN" && !c.post)!;
    const put = (async (url: string, init: RequestInit) => {
      const target = uploadPath(url);
      expect(target).toMatch(/^\/api\/post-media\/[^/]+\/[^/?]+$/);
      store.posts.receive(target!.split("/").pop()!, new Uint8Array(await (init.body as Blob).arrayBuffer()));
      return new Response(null, { status: 204 });
    }) as unknown as typeof fetch;
    const bytes = Uint8Array.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0]);
    const file = new Blob([bytes], { type: "image/png" });
    const media = await uploadPostMedia(mara, { repo: api, changeId: open.id, file, altText: "A pixel" }, { fetch: put, decode: { image: async () => ({ width: 1, height: 1 }), video: async () => ({ width: 0, height: 0, durationSeconds: 0 }) } });
    const saved = await mara.post.setChangePost({ repo: api, changeId: open.id, text: "Now with a picture", mediaIds: [media.id], expectedVersion: "0" });
    expect(saved).toMatchObject({ state: "POST_STATE_DRAFT", text: "Now with a picture" });
    const stale = await fails(mara.post.setChangePost({ repo: api, changeId: open.id, text: "again", mediaIds: [media.id], expectedVersion: "999" }));
    expect(stale.reason).toBe("VERSION_MISMATCH");
  });
});

function world(): { store: FixtureStore; mara: BeeClient } {
  const store = new FixtureStore({ anchor: Date.UTC(2026, 9, 18, 9, 0, 0), clock: () => 0, live: false });
  return { store, mara: createClient(fixtureRpc(store, "mara-token-0001")) };
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
