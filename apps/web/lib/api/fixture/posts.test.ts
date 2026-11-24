import { createHash } from "node:crypto";
import { readFileSync, statSync } from "node:fs";
import { describe, expect, it } from "vitest";
import { createClient, type BeeClient } from "../client";
import type * as T from "../types";
import { FixtureStore, fixtureRpc } from "./index";
import { MEDIA_ALT, MEDIA_FILES, POST_SEEDS } from "./posts-world";
import { sniff } from "./posts";

const ANCHOR = Date.UTC(2026, 9, 18, 9, 0, 0);
const MIN = 60_000;

function world() {
  const store = new FixtureStore({ anchor: ANCHOR, clock: () => 0, live: false });
  const as = (token: string) => createClient(fixtureRpc(store, token));
  /** A client signed in as another seeded principal. */
  const asHandle = (handle: string): BeeClient => {
    const id = "tok_" + handle;
    store.tokens.push({ tokenId: id, parentTokenId: "", principalId: store.p(handle).id, revoked: false } as never);
    store.tokenSecrets.set("secret-" + handle + "-0001", id);
    return as("secret-" + handle + "-0001");
  };
  return { store, mara: as("mara-token-0001"), anon: createClient(fixtureRpc(store)), as, asHandle };
}

const api = { fullName: "acme/api" };
const engine = { fullName: "lattice/engine" };
const sdk = { fullName: "acme/sdk" };
const kit = { fullName: "lattice/agent-kit" };

async function fails(p: Promise<unknown>): Promise<{ code: string; reason: string }> {
  try {
    await p;
  } catch (e) {
    const err = e as { code: string; reason: string };
    return { code: err.code, reason: err.reason };
  }
  throw new Error("expected the call to fail");
}

/** An id that is not a principal. */
const NOT_A_PRINCIPAL = "01NOTAPRINCIPAL0000000000";
const infra = { fullName: "acme/infra" };
const web = { fullName: "acme/web" };
const bench = { fullName: "lattice/bench" };

const find = async (c: BeeClient, repo: T.RepoRef, title: string): Promise<T.ChangeRequest> => {
  const cr = (await c.change.listChanges({ repo })).changes.find((x) => x.title === title);
  if (!cr) throw new Error("no change " + title);
  return cr;
};

const published = async (c: BeeClient, repo: T.RepoRef) => (await c.post.listRepoPosts({ repo, page: { pageSize: 100 } })).posts;

describe("the demo world's posts", () => {
  it("has about 14 published posts on public repos, plus a pending, a withdrawn and a draft", async () => {
    const { store, mara } = world();
    const states = new Map<string, number>();
    for (const p of store.posts.posts) states.set(p.state, (states.get(p.state) ?? 0) + 1);
    expect(states.get("POST_STATE_PUBLISHED")).toBe(15);
    expect(states.get("POST_STATE_PENDING_APPROVAL")).toBe(1);
    expect(states.get("POST_STATE_WITHDRAWN")).toBe(1);
    expect(states.get("POST_STATE_DRAFT")).toBe(1);
    const publicRepos = [engine, kit, sdk, { fullName: "lattice/docs" }, { fullName: "lattice/bench" }];
    const counts = await Promise.all(publicRepos.map(async (r) => (await published(mara, r)).length));
    expect(counts.reduce((a, b) => a + b, 0)).toBe(14);
    expect(counts).toEqual([4, 4, 3, 1, 2]);
    // Both humans and agents write.
    const kinds = new Set(store.posts.posts.map((p) => store.kindOf(p.author)));
    expect(kinds).toEqual(new Set(["human", "agent"]));
    // The draft is on an open change in acme/api; the pending post waits on a landed change.
    const draft = store.posts.posts.find((p) => p.state === "POST_STATE_DRAFT");
    const cr = store.crs.find((c) => c.id === draft?.crId);
    expect([cr?.repo, cr?.state]).toEqual(["api", "CHANGE_STATE_OPEN"]);
  });

  it("ships real media: every file exists, matches its size and sha256, and stays small", () => {
    const dir = new URL("../../../public/fixture/posts/", import.meta.url);
    let total = 0;
    for (const [key, m] of Object.entries(MEDIA_FILES)) {
      const bytes = readFileSync(new URL(m.file, dir));
      expect(bytes.length, key).toBe(m.sizeBytes);
      expect(createHash("sha256").update(bytes).digest("hex"), key).toBe(m.sha256);
      expect(statSync(new URL(m.file, dir)).size, key).toBeLessThan(3 * 1024 * 1024);
      expect(sniff(bytes), key).toBe(m.contentType);
      if (m.kind === "image") expect(MEDIA_ALT[key]?.length, key).toBeGreaterThan(30);
      total += bytes.length;
    }
    expect(MEDIA_FILES.walkthrough).toMatchObject({ contentType: "video/mp4", durationMs: 8000 });
    expect(MEDIA_FILES.walkthrough!.sizeBytes).toBeLessThan(2 * 1024 * 1024);
    expect(total).toBeLessThan(6 * 1024 * 1024);
    for (const sp of POST_SEEDS) for (const k of sp.media) expect(MEDIA_FILES[k], sp.cr + " → " + k).toBeDefined();
  });

  it("gives every image alt text and every post a signed-looking media url", async () => {
    const { mara } = world();
    const posts = await published(mara, engine);
    const media = posts.flatMap((p) => p.media);
    expect(media.length).toBeGreaterThan(3);
    for (const m of media) {
      expect(m.url).toMatch(/^\/fixture\/posts\//);
      if (m.kind === "POST_MEDIA_KIND_IMAGE") expect(m.altText.length).toBeGreaterThan(10);
    }
    const video = (await published(mara, kit)).flatMap((p) => p.media).find((m) => m.kind === "POST_MEDIA_KIND_VIDEO");
    expect(video).toMatchObject({ contentType: "video/mp4", durationMs: 8000, width: 1280, height: 800 });
  });

  it("is the same on every build and never reads a random source", () => {
    const a = world().store.posts.posts.map((p) => [p.id, p.state, p.publishedAt]);
    const b = world().store.posts.posts.map((p) => [p.id, p.state, p.publishedAt]);
    expect(a).toEqual(b);
    for (const f of ["posts.ts", "posts-world.ts"]) expect(readFileSync(new URL("./" + f, import.meta.url), "utf8")).not.toMatch(/Math\.random|Date\.now/);
  });
});

describe("visibility", () => {
  it("shows published posts of public repos to a signed-out visitor and nothing of private ones", async () => {
    const { anon, mara, store } = world();
    const list = await published(anon, engine);
    expect(list.length).toBeGreaterThan(2);
    expect(list.every((p) => p.state === "POST_STATE_PUBLISHED" && p.viewer?.liked === false && p.viewer.canEdit === false)).toBe(true);
    expect((await anon.post.getPost({ postId: list[0]!.id })).id).toBe(list[0]!.id);
    // The private repo answers like a missing one; a post id alone does not open it either.
    expect(await fails(anon.post.listRepoPosts({ repo: api }))).toMatchObject({ code: "not_found", reason: "REPO_NOT_FOUND" });
    const secret = store.posts.posts.find((p) => p.repo === "api" && p.state === "POST_STATE_PUBLISHED")!;
    expect(await fails(anon.post.getPost({ postId: secret.id }))).toMatchObject({ code: "not_found", reason: "POST_NOT_FOUND" });
    expect((await mara.post.getPost({ postId: secret.id })).changeTitle).toBe("Add health route latency histogram");
    // Another org's member cannot read it either.
    const rhea = world().asHandle("rhea");
    expect(await fails(rhea.post.getPost({ postId: secret.id }))).toMatchObject({ code: "not_found" });
  });

  it("keeps signed-in actions behind an account", async () => {
    const { anon } = world();
    const id = (await published(anon, engine))[0]!.id;
    for (const call of [anon.post.likePost({ postId: id }), anon.post.repostPost({ postId: id }), anon.post.listFollowingFeed({}), anon.user.followRepo({ repo: engine }), anon.post.getChangePost({ repo: engine, changeId: "x" })]) {
      expect((await fails(call)).code).toBe("unauthenticated");
    }
  });

  it("shows a draft and a pending post only to people who may edit the change", async () => {
    const { mara, anon, store, asHandle } = world();
    const draft = store.posts.posts.find((p) => p.state === "POST_STATE_DRAFT")!;
    expect((await mara.post.getPost({ postId: draft.id })).viewer).toMatchObject({ canEdit: true });
    expect((await mara.post.getChangePost({ repo: api, changeId: draft.crId })).state).toBe("POST_STATE_DRAFT");
    const pending = store.posts.posts.find((p) => p.state === "POST_STATE_PENDING_APPROVAL")!;
    expect((await anon.change.getChange({ repo: { fullName: "lattice/docs" }, id: pending.crId })).post).toBeUndefined();
    expect((await asHandle("rhea").post.getPost({ postId: pending.id })).state).toBe("POST_STATE_PENDING_APPROVAL");
    expect(await fails(anon.post.getPost({ postId: pending.id }))).toMatchObject({ code: "not_found" });
    expect(await fails(asHandle("dev").post.getPost({ postId: pending.id }))).toMatchObject({ code: "not_found" });
  });

  it("serves a withdrawn post's permalink as withdrawn, without its text or media", async () => {
    const { anon, store } = world();
    const w = store.posts.posts.find((p) => p.state === "POST_STATE_WITHDRAWN")!;
    const post = await anon.post.getPost({ postId: w.id });
    expect(post).toMatchObject({ state: "POST_STATE_WITHDRAWN", text: "", media: [] });
    expect(post.withdrawnReason).toContain("renamed");
    expect((await published(anon, sdk)).map((p) => p.id)).not.toContain(w.id);
  });
});

describe("the Following feed", () => {
  it("lists published posts from followed repos, followed people and their reposts, once each, newest first", async () => {
    const { mara } = world();
    const feed = await mara.post.listFollowingFeed({ page: { pageSize: 100 } });
    const ids = feed.items.map((i) => i.post!.id);
    expect(new Set(ids).size).toBe(ids.length);
    expect(feed.items.every((i) => i.post!.state === "POST_STATE_PUBLISHED")).toBe(true);
    const ats = feed.items.map((i) => Date.parse(i.at!));
    expect(ats).toEqual([...ats].sort((a, b) => b - a));
    const repos = new Set(feed.items.map((i) => i.post!.repo!.fullName));
    // Followed: engine, agent-kit, sdk. Reached through people: docs (patcher) and bench (Rhea's repost).
    expect(repos).toEqual(new Set(["lattice/engine", "lattice/agent-kit", "acme/sdk", "lattice/docs", "lattice/bench"]));
    // A post that is both in a followed repo and by a followed author appears once, as itself.
    const rebase = feed.items.find((i) => i.post!.changeTitle === "Add three-way rebase")!;
    expect(rebase.repostedBy?.display).toBe("Dev Patel");
    const bench = feed.items.find((i) => i.post!.repo!.fullName === "lattice/bench")!;
    expect(bench.repostedBy?.display).toBe("Rhea Lindqvist");
    expect(Date.parse(bench.at!)).toBeGreaterThan(Date.parse(bench.post!.publishedAt!));
    // Neither a pending post nor a private repo's post is in it.
    expect(feed.items.some((i) => i.post!.repo!.fullName === "acme/api")).toBe(false);
    expect(feed.newestCursor).toBe(new Date(feed.items[0]!.at!).getTime() + "|" + feed.items[0]!.post!.id);
  });

  it("pages with a cursor of at_ms|post_id and refuses a bad one with bad_page_token", async () => {
    const { mara } = world();
    const all = (await mara.post.listFollowingFeed({ page: { pageSize: 100 } })).items.map((i) => i.post!.id);
    const seen: string[] = [];
    let token = "";
    for (let guard = 0; guard < 20; guard++) {
      const res = await mara.post.listFollowingFeed({ page: { pageSize: 4, pageToken: token } });
      seen.push(...res.items.map((i) => i.post!.id));
      token = res.page?.nextPageToken ?? "";
      if (!token) break;
      expect(token).toMatch(/^\d+\|[0-9A-Z]{26}$/);
    }
    expect(seen).toEqual(all);
    expect(await fails(mara.post.listFollowingFeed({ page: { pageToken: "nonsense" } }))).toMatchObject({ code: "invalid_argument", reason: "bad_page_token" });
    expect(await fails(mara.post.listFollowingFeed({ after: "12|x" }))).toMatchObject({ reason: "bad_page_token" });
  });

  it("with `after` returns only newer items, oldest first, and a cursor to save", async () => {
    const { mara } = world();
    const full = await mara.post.listFollowingFeed({ page: { pageSize: 100 } });
    const cursor = (i: number) => new Date(full.items[i]!.at!).getTime() + "|" + full.items[i]!.post!.id;
    const fresh = await mara.post.listFollowingFeed({ after: cursor(3) });
    expect(fresh.items.map((i) => i.post!.id)).toEqual(full.items.slice(0, 3).map((i) => i.post!.id).reverse());
    expect(fresh.newestCursor).toBe(cursor(0));
    const none = await mara.post.listFollowingFeed({ after: cursor(0) });
    expect(none.items).toEqual([]);
    expect(none.newestCursor).toBe(cursor(0));
  });

  it("is empty for someone who follows nothing, and fills when they follow a repo", async () => {
    const { store, asHandle } = world();
    const kit2 = asHandle("kit");
    // Kit follows Rhea, Omar and Mara; strip those to start from nothing.
    store.follows = store.follows.filter((f) => f.from !== "kit");
    store.posts.follows = store.posts.follows.filter((f) => f.who !== "kit");
    expect((await kit2.post.listFollowingFeed({})).items).toEqual([]);
    await kit2.user.followRepo({ repo: sdk });
    const after = await kit2.post.listFollowingFeed({});
    expect(after.items.length).toBe((await published(kit2, sdk)).length);
    await kit2.user.followUser({ login: "patcher" });
    expect((await kit2.post.listFollowingFeed({ page: { pageSize: 100 } })).items.length).toBeGreaterThan(after.items.length);
  });

  it("works for an agent token and lets it follow repos", async () => {
    const { asHandle } = world();
    const triage = asHandle("triage");
    expect((await triage.user.isFollowingRepo({ repo: engine })).following).toBe(true);
    const feed = await triage.post.listFollowingFeed({ page: { pageSize: 100 } });
    expect(new Set(feed.items.map((i) => i.post!.repo!.fullName))).toEqual(new Set(["lattice/engine", "lattice/agent-kit"]));
    await triage.user.followRepo({ repo: sdk });
    expect((await triage.user.listFollowedRepos({ user: "triage" })).repos.map((r) => r.fullName)).toContain("acme/sdk");
    await triage.user.unfollowRepo({ repo: sdk });
    expect((await triage.user.isFollowingRepo({ repo: sdk })).following).toBe(false);
  });
});

describe("follows", () => {
  it("lists followed repos and followers, hides private repos from others and follows a new repo's creator", async () => {
    const { mara, anon } = world();
    const followed = await mara.user.listFollowedRepos({ user: "mara" });
    expect(followed.repos.map((r) => r.fullName).sort()).toEqual(["acme/sdk", "lattice/agent-kit", "lattice/engine"]);
    const followers = await anon.user.listRepoFollowers({ repo: engine }).catch((e: { code: string }) => e.code);
    expect(followers).toBe("unauthenticated");
    expect((await mara.user.listRepoFollowers({ repo: engine })).followers.map((p) => p.display)).toContain("Mara Okafor");
    await mara.user.followRepo({ repo: engine });
    expect((await mara.user.listRepoFollowers({ repo: engine })).followers.filter((p) => p.display === "Mara Okafor")).toHaveLength(1);
    await mara.org.createOrg({ name: "newco" });
    await mara.repo.createRepo({ org: "newco", name: "first", visibility: "VISIBILITY_PRIVATE" });
    expect((await mara.user.isFollowingRepo({ repo: { fullName: "newco/first" } })).following).toBe(true);
    // Rhea cannot see Mara's private follow.
    const rhea = world().asHandle("rhea");
    expect((await rhea.user.listFollowedRepos({ user: "mara" })).repos.map((r) => r.fullName)).not.toContain("acme/api");
    expect(await fails(mara.user.followRepo({ repo: { fullName: "lattice/internal-tools" } }))).toMatchObject({ code: "not_found", reason: "REPO_NOT_FOUND" });
  });
});

describe("likes and reposts", () => {
  it("count once per person, are idempotent and show on the viewer", async () => {
    const { mara, asHandle } = world();
    const p = (await published(mara, sdk)).find((x) => x.changeTitle === "Bump typescript from 5.5.4 to 5.6.2")!;
    expect(p.viewer).toMatchObject({ liked: false, reposted: false });
    const l1 = await mara.post.likePost({ postId: p.id });
    const l2 = await mara.post.likePost({ postId: p.id });
    expect([l1.counts?.likes, l2.counts?.likes]).toEqual([String(Number(p.counts!.likes) + 1), String(Number(p.counts!.likes) + 1)]);
    expect(l2.viewer?.liked).toBe(true);
    await asHandle("dev").post.likePost({ postId: p.id });
    expect((await mara.post.getPost({ postId: p.id })).counts?.likes).toBe(String(Number(p.counts!.likes) + 2));
    expect((await mara.post.unlikePost({ postId: p.id })).viewer?.liked).toBe(false);
    expect((await mara.post.unlikePost({ postId: p.id })).counts?.likes).toBe(String(Number(p.counts!.likes) + 1));
    const r = await mara.post.repostPost({ postId: p.id });
    expect(r).toMatchObject({ viewer: { reposted: true }, counts: { reposts: String(Number(p.counts!.reposts) + 1) } });
    expect((await mara.post.undoRepost({ postId: p.id })).counts?.reposts).toBe(p.counts!.reposts);
  });

  it("works only on published posts", async () => {
    const { mara, store } = world();
    const w = store.posts.posts.find((x) => x.state === "POST_STATE_WITHDRAWN")!;
    expect(await fails(mara.post.likePost({ postId: w.id }))).toMatchObject({ code: "failed_precondition", reason: "INVALID_STATE" });
  });

  it("puts a followed person's repost in the follower's feed", async () => {
    const { asHandle } = world();
    const kit2 = asHandle("kit");
    const dev = asHandle("dev");
    await dev.user.followUser({ login: "kit" });
    const post = (await dev.post.listRepoPosts({ repo: { fullName: "lattice/bench" } })).posts[0]!;
    await kit2.post.repostPost({ postId: post.id });
    const feed = await dev.post.listFollowingFeed({ page: { pageSize: 100 } });
    const item = feed.items.find((i) => i.post!.id === post.id)!;
    expect(item.repostedBy?.display).toBe("Kit Nakamura");
    expect(feed.items.filter((i) => i.post!.id === post.id)).toHaveLength(1);
  });
});

describe("writing a post", () => {
  it("drafts a post on an open change, publishes it on landing when a person wrote it, and discards it on close", async () => {
    const { mara, store } = world();
    const cr = await find(mara, infra, "Add the us-east-2 cell");
    const none = await fails(mara.post.getChangePost({ repo: infra, changeId: cr.id }));
    expect(none).toMatchObject({ code: "not_found", reason: "POST_NOT_FOUND" });
    const draft = await mara.post.setChangePost({ repo: infra, changeId: cr.id, text: "A new cell in us-east-2.  #infra" });
    expect(draft).toMatchObject({ state: "POST_STATE_DRAFT", version: "1", changeNumber: cr.number, changeTitle: cr.title, viewer: { canEdit: true, canWithdraw: true, canApprove: false } });
    expect(draft.author?.display).toBe("Mara Okafor");
    expect((await mara.change.getChange({ repo: infra, id: cr.id })).post?.id).toBe(draft.id);
    const edited = await mara.post.setChangePost({ repo: infra, changeId: cr.id, text: "A new cell in us-east-2.", expectedVersion: "1" });
    expect([edited.id, edited.version]).toEqual([draft.id, "2"]);
    expect(await fails(mara.post.setChangePost({ repo: infra, changeId: cr.id, text: "again", expectedVersion: "1" }))).toMatchObject({ reason: "VERSION_MISMATCH" });
    // Landing publishes it, with published_at set to the landing time.
    store.land(store.crs.find((c) => c.id === cr.id)!, "mara");
    const live = await mara.post.getPost({ postId: draft.id });
    expect(live).toMatchObject({ state: "POST_STATE_PUBLISHED", publishedAt: new Date(store.now()).toISOString().replace(".000Z", "Z") });
    expect(await fails(mara.post.setChangePost({ repo: infra, changeId: cr.id, text: "late edit" }))).toMatchObject({ reason: "INVALID_STATE" });

    const other = await find(mara, api, "Document the webhook retry schedule");
    const d2 = await mara.post.setChangePost({ repo: api, changeId: other.id, text: "Webhook retries are documented." });
    await mara.change.closeChange({ repo: api, id: other.id, reason: "superseded" });
    expect((await mara.post.getPost({ postId: d2.id })).state).toBe("POST_STATE_DISCARDED");
    expect((await mara.change.getChange({ repo: api, id: other.id })).post).toBeUndefined();
  });

  it("holds an agent's post for a person's approval, and keeps that approval through landing", async () => {
    const { mara, store, asHandle } = world();
    const rhea = asHandle("rhea");
    const patcher = asHandle("patcher");
    const cr = await find(mara, { fullName: "lattice/agent-kit" }, "Stream partial tool output");
    const draft = await patcher.post.setChangePost({ repo: kit, changeId: cr.id, text: "Tools can now stream partial output while they run." });
    expect(draft.author?.display).toBe("patcher");
    expect(draft.viewer).toMatchObject({ canEdit: true, canApprove: false });
    // Its author cannot approve it; neither can another agent. A person with write access can.
    expect(await fails(patcher.post.approvePost({ repo: kit, postId: draft.id }))).toMatchObject({ code: "permission_denied" });
    expect(await fails(asHandle("bench-runner").post.approvePost({ repo: kit, postId: draft.id }))).toMatchObject({ code: "permission_denied" });
    expect((await rhea.post.getPost({ postId: draft.id })).viewer?.canApprove).toBe(true);
    const approved = await rhea.post.approvePost({ repo: kit, postId: draft.id });
    expect(approved).toMatchObject({ state: "POST_STATE_DRAFT", approvedBy: { display: "Rhea Lindqvist" }, viewer: { canApprove: false } });
    // A change to the text takes the sign-off back: a person approved what they read.
    const rewritten = await patcher.post.setChangePost({ repo: kit, changeId: cr.id, text: "Tools stream partial output while they run, one note at a time." });
    expect(rewritten.approvedBy).toBeUndefined();
    await rhea.post.approvePost({ repo: kit, postId: draft.id });
    store.land(store.crs.find((c) => c.id === cr.id)!, "rhea");
    expect((await mara.post.getPost({ postId: draft.id })).state).toBe("POST_STATE_PUBLISHED");
  });

  it("makes a landed agent post pending and publishes it at once on approval", async () => {
    const { mara, store, asHandle } = world();
    const runner = asHandle("bench-runner");
    const omar = asHandle("omar");
    const cr = await find(mara, bench, "Add p99 to the summary");
    const draft = await runner.post.setChangePost({ repo: bench, changeId: cr.id, text: "The summary reports p99 now." });
    const events: string[] = [];
    store.onEvent((e) => events.push(e.kind));
    store.land(store.crs.find((c) => c.id === cr.id)!, "omar");
    expect(events).toContain("post.approval_requested");
    expect((await omar.post.getPost({ postId: draft.id })).state).toBe("POST_STATE_PENDING_APPROVAL");
    expect((await asHandle("rhea").post.listFollowingFeed({ page: { pageSize: 100 } })).items.some((i) => i.post!.id === draft.id)).toBe(false);
    // Still editable while it waits; an agent cannot approve.
    expect((await runner.post.setChangePost({ repo: bench, changeId: cr.id, text: "The summary reports p99 as well." })).state).toBe("POST_STATE_PENDING_APPROVAL");
    expect(await fails(runner.post.approvePost({ repo: bench, postId: draft.id }))).toMatchObject({ code: "permission_denied" });
    const p = await omar.post.approvePost({ repo: bench, postId: draft.id });
    expect(p.state).toBe("POST_STATE_PUBLISHED");
    expect(p.approvedBy?.display).toBe("Omar Haddad");
    expect(events).toContain("post.published");
    expect(await fails(omar.post.approvePost({ repo: bench, postId: draft.id }))).toMatchObject({ reason: "INVALID_STATE" });
    expect((await asHandle("rhea").post.listFollowingFeed({ page: { pageSize: 100 } })).items.some((i) => i.post!.id === draft.id)).toBe(true);
  });

  it("refuses to approve a post a person wrote", async () => {
    const { mara, asHandle } = world();
    const cr = await find(mara, web, "Poll the robots endpoint every 5 seconds");
    const draft = await asHandle("lin").post.setChangePost({ repo: web, changeId: cr.id, text: "Robots are polled every 5 seconds." });
    expect(await fails(mara.post.approvePost({ repo: web, postId: draft.id }))).toMatchObject({ reason: "INVALID_STATE" });
  });

  it("applies the text and media rules", async () => {
    const { mara } = world();
    const cr = await find(mara, infra, "Add the us-east-2 cell");
    const set = (text: string, mediaIds: string[] = []) => mara.post.setChangePost({ repo: infra, changeId: cr.id, text, mediaIds });
    expect(await fails(set("x".repeat(501)))).toMatchObject({ code: "invalid_argument", reason: "text_too_long" });
    expect((await set("é".repeat(500))).text).toHaveLength(500);
    expect(await fails(set("bell\u0007"))).toMatchObject({ reason: "bad_text" });
    expect(await fails(set("tab\there"))).toMatchObject({ reason: "bad_text" });
    expect((await set("two\nlines")).text).toBe("two\nlines");
    expect(await fails(set("   "))).toMatchObject({ reason: "empty_post" });
    // NFC: a decomposed e-acute is stored composed, and counted as one character.
    expect((await set("é".repeat(300))).text).toBe("é".repeat(300));
    expect(await fails(set("hi", ["nope"]))).toMatchObject({ reason: "bad_media" });
  });

  it("uploads media: begin, put, finalize, then attaches it; alt text and limits are enforced", async () => {
    const { mara, store } = world();
    const cr = await find(mara, infra, "Add the us-east-2 cell");
    const png = Uint8Array.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 1, 2, 3, 4]);
    const begin = (over: Partial<T.BeginPostMediaUploadRequest> = {}) =>
      mara.post.beginPostMediaUpload({ repo: infra, changeId: cr.id, kind: "POST_MEDIA_KIND_IMAGE", contentType: "image/png", sizeBytes: String(png.length), ...over });
    expect(await fails(begin({ contentType: "image/svg+xml" }))).toMatchObject({ reason: "bad_media" });
    expect(await fails(begin({ contentType: "video/mp4" }))).toMatchObject({ reason: "bad_media" });
    expect(await fails(begin({ sizeBytes: String(9 * 1024 * 1024) }))).toMatchObject({ code: "resource_exhausted", reason: "TOO_LARGE" });
    const up = await begin();
    expect(up.uploadHeaders).toEqual({ "content-type": "image/png" });
    expect(Date.parse(up.expiresAt!) - store.now()).toBe(15 * MIN);
    const sha = createHash("sha256").update(png).digest("hex");
    // The bytes are checked against the hash and the type.
    store.posts.receive(up.mediaId, png);
    expect(await fails(mara.post.finalizePostMedia({ repo: infra, mediaId: up.mediaId, sha256: "0".repeat(64), altText: "x" }))).toMatchObject({ code: "data_loss", reason: "INTEGRITY" });
    expect(await fails(mara.post.finalizePostMedia({ repo: infra, mediaId: up.mediaId, sha256: sha, altText: "" }))).toMatchObject({ reason: "alt_text_required" });
    expect(() => store.posts.receive(up.mediaId, Uint8Array.from([0x3c, 0x73, 0x76, 0x67]))).toThrow(/not image\/png/);
    const media = await mara.post.finalizePostMedia({ repo: infra, mediaId: up.mediaId, sha256: sha, altText: "A tiny test image", width: 1, height: 1 });
    expect(media).toMatchObject({ id: up.mediaId, sha256: sha, width: 1, height: 1, altText: "A tiny test image", kind: "POST_MEDIA_KIND_IMAGE" });
    expect(media.url).toMatch(/^data:image\/png;base64,/);
    const post = await mara.post.setChangePost({ repo: infra, changeId: cr.id, text: "", mediaIds: [media.id] });
    expect(post.media.map((m) => m.id)).toEqual([media.id]);
    // Media belongs to one change.
    const other = await find(mara, web, "Poll the robots endpoint every 5 seconds");
    expect(await fails(mara.post.setChangePost({ repo: web, changeId: other.id, text: "x", mediaIds: [media.id] }))).toMatchObject({ reason: "bad_media" });
    // One video, no images beside it.
    const v = await mara.post.beginPostMediaUpload({ repo: infra, changeId: cr.id, kind: "POST_MEDIA_KIND_VIDEO", contentType: "video/mp4", sizeBytes: "1000" });
    const vm = await mara.post.finalizePostMedia({ repo: infra, mediaId: v.mediaId, sha256: "a".repeat(64), width: 640, height: 360, durationMs: 4000 });
    expect(vm.durationMs).toBe(4000);
    expect(await fails(mara.post.setChangePost({ repo: infra, changeId: cr.id, text: "x", mediaIds: [media.id, vm.id] }))).toMatchObject({ reason: "bad_media" });
    expect((await mara.post.setChangePost({ repo: infra, changeId: cr.id, text: "", mediaIds: [vm.id] })).media).toHaveLength(1);
    // Clearing removes the post and its media.
    await mara.post.clearChangePost({ repo: infra, changeId: cr.id });
    expect(await fails(mara.post.getChangePost({ repo: infra, changeId: cr.id }))).toMatchObject({ code: "not_found" });
    expect(store.posts.media.has(vm.id)).toBe(false);
  });

  it("allows at most four images", async () => {
    const { mara } = world();
    const cr = await find(mara, infra, "Add the us-east-2 cell");
    const ids: string[] = [];
    for (let i = 0; i < 5; i++) {
      const up = await mara.post.beginPostMediaUpload({ repo: infra, changeId: cr.id, kind: "POST_MEDIA_KIND_IMAGE", contentType: "image/png", sizeBytes: "10" });
      ids.push((await mara.post.finalizePostMedia({ repo: infra, mediaId: up.mediaId, sha256: "b".repeat(64), altText: "image " + i, width: 2, height: 2 })).id);
    }
    expect(await fails(mara.post.setChangePost({ repo: infra, changeId: cr.id, text: "", mediaIds: ids }))).toMatchObject({ reason: "bad_media" });
    expect((await mara.post.setChangePost({ repo: infra, changeId: cr.id, text: "", mediaIds: ids.slice(0, 4) })).media).toHaveLength(4);
  });

  it("limits who may write: the change's author or repo Write", async () => {
    const { mara, asHandle } = world();
    const cr = await find(mara, { fullName: "lattice/engine" }, "Cache ancestor lookups");
    expect(await fails(asHandle("dev").post.setChangePost({ repo: engine, changeId: cr.id, text: "x" }))).toMatchObject({ code: "permission_denied" });
    // Omar wrote the change; Rhea has write access to the repo.
    const omar = asHandle("omar");
    expect((await omar.post.setChangePost({ repo: engine, changeId: cr.id, text: "Ancestor lookups are cached." })).author?.display).toBe("Omar Haddad");
    expect((await asHandle("rhea").post.setChangePost({ repo: engine, changeId: cr.id, text: "Ancestor lookups are cached per id." })).author?.display).toBe("Rhea Lindqvist");
  });
});

describe("withdrawing", () => {
  it("lets the author or an admin withdraw, removes the post from feeds and keeps the permalink", async () => {
    const { mara, store, asHandle } = world();
    const post = (await published(mara, engine)).find((p) => p.author?.display === "Omar Haddad")!;
    expect(post.viewer?.canWithdraw).toBe(false);
    expect(await fails(mara.post.withdrawPost({ repo: engine, postId: post.id, reason: "no" }))).toMatchObject({ code: "permission_denied" });
    expect(await fails(asHandle("kit").post.withdrawPost({ repo: engine, postId: post.id }))).toMatchObject({ code: "permission_denied" });
    const w = await asHandle("omar").post.withdrawPost({ repo: engine, postId: post.id, reason: "Wrong benchmark numbers" });
    expect(w).toMatchObject({ state: "POST_STATE_WITHDRAWN", withdrawnReason: "Wrong benchmark numbers", text: "", media: [] });
    expect((await published(mara, engine)).map((p) => p.id)).not.toContain(post.id);
    expect((await mara.post.listFollowingFeed({ page: { pageSize: 100 } })).items.map((i) => i.post!.id)).not.toContain(post.id);
    expect(store.posts.byId(post.id)?.state).toBe("POST_STATE_WITHDRAWN");
    // An admin of the repo may withdraw someone else's post.
    const rhea = asHandle("rhea");
    const other = (await published(mara, engine)).find((p) => p.author?.display === "Kit Nakamura")!;
    expect((await rhea.post.withdrawPost({ repo: engine, postId: other.id })).state).toBe("POST_STATE_WITHDRAWN");
  });
});

describe("adoptions and inspired_by", () => {
  it("shows what a post inspired, only in repos the reader can read", async () => {
    const { mara, anon, asHandle } = world();
    const usage = (await published(mara, kit)).find((p) => p.changeTitle === "Record usage on every run")!;
    expect(usage.counts?.adoptions).toBe("1");
    const list = await mara.post.listPostAdoptions({ postId: usage.id });
    expect(list.adoptions).toHaveLength(1);
    expect(list.adoptions[0]).toMatchObject({ repo: api, title: "Record usage on every billing run", author: { display: "triage" }, subject: { kind: "SUBJECT_KIND_ISSUE", repo: api } });
    // A signed-out visitor, and a member of the source org, cannot read acme/api: no leak.
    expect((await anon.post.getPost({ postId: usage.id })).counts?.adoptions).toBe("0");
    expect((await asHandle("rhea").post.listPostAdoptions({ postId: usage.id })).adoptions).toEqual([]);
    // The public adoption (acme/sdk) is visible to everyone.
    const retry = (await published(mara, kit)).find((p) => p.changeTitle === "Retry tool calls with backoff")!;
    expect((await asHandle("dev").post.listPostAdoptions({ postId: retry.id })).adoptions).toHaveLength(1);
    // The list needs an account; the count on the post does not.
    expect((await fails(anon.post.listPostAdoptions({ postId: retry.id }))).code).toBe("unauthenticated");
    expect((await anon.post.getPost({ postId: retry.id })).counts?.adoptions).toBe("1");
  });

  it("links an issue or a change to a published post and refuses others with post_not_readable", async () => {
    const { mara, anon, asHandle, store } = world();
    const post = (await published(mara, engine))[0]!;
    const issue = await mara.issue.createIssue({ repo: api, title: "Walk the graph without recursion", body: "Same idea for the invoice tree.", labels: ["perf"], inspiredByPostId: post.id });
    expect(issue.inspiredBy).toMatchObject({ postId: post.id, repo: engine });
    expect((await mara.issue.getIssue({ repo: api, number: issue.number })).inspiredBy?.postId).toBe(post.id);
    expect((await mara.issue.listIssues({ repo: api })).issues.find((i) => i.number === issue.number)?.inspiredBy?.postId).toBe(post.id);
    expect((await mara.post.getPost({ postId: post.id })).counts?.adoptions).toBe("1");
    expect(store.events.at(-1)).toMatchObject({ kind: "post.adopted" });
    // acme/sdk is public: its issue shows the banner to everyone, even a signed-out visitor.
    const pub = await mara.issue.createIssue({ repo: sdk, title: "Walk pages iteratively", inspiredByPostId: post.id });
    expect((await anon.issue.getIssue({ repo: sdk, number: pub.number })).inspiredBy?.repo).toEqual(engine);

    const draft = store.posts.posts.find((p) => p.state === "POST_STATE_DRAFT")!;
    expect(await fails(mara.issue.createIssue({ repo: api, title: "x", inspiredByPostId: draft.id }))).toMatchObject({ code: "invalid_argument", reason: "post_not_readable" });
    expect(await fails(mara.issue.createIssue({ repo: api, title: "x", inspiredByPostId: "01NOSUCHPOST0000000000000A" }))).toMatchObject({ reason: "post_not_readable" });
    expect(await fails(mara.issue.createIssue({ repo: api, title: "  " }))).toMatchObject({ code: "invalid_argument" });

    // A change request carries the link the same way.
    const cr = await mara.change.openChange({ repo: api, targetRef: "main", title: "Walk the graph without recursion in api", body: "", inspiredByPostId: post.id, postText: "Ported the iterative walk from lattice/engine." });
    expect(cr.inspiredBy).toMatchObject({ postId: post.id, changeTitle: post.changeTitle });
    expect(cr.post).toMatchObject({ state: "POST_STATE_DRAFT", text: "Ported the iterative walk from lattice/engine." });
    expect(await fails(mara.change.openChange({ repo: api, targetRef: "main", title: "x", body: "", inspiredByPostId: draft.id }))).toMatchObject({ reason: "post_not_readable" });
    // A reader who cannot read the source repo sees the change, but not the link.
    const private1 = store.posts.posts.find((p) => p.repo === "api" && p.state === "POST_STATE_PUBLISHED")!;
    const sdkIssue = await mara.issue.createIssue({ repo: sdk, title: "From a private post?", inspiredByPostId: private1.id });
    expect(sdkIssue.inspiredBy).toMatchObject({ postId: private1.id });
    expect((await anon.issue.getIssue({ repo: sdk, number: sdkIssue.number })).inspiredBy).toBeUndefined();
    expect((await asHandle("rhea").issue.getIssue({ repo: sdk, number: sdkIssue.number })).inspiredBy).toBeUndefined();
  });
});

describe("agent feed triggers", () => {
  it("configures a trigger for one agent, makes the agent follow its sources and lists them", async () => {
    const { mara, asHandle } = world();
    const triageId = (await mara.agent.listAgents({ org: "acme" })).agents.find((a) => a.principal.display === "triage")!.principal.id;
    const t = await mara.agent.getAgentFeedTrigger({ repo: api, agent: triageId });
    expect(t).toMatchObject({ enabled: true, dailyUsdBudget: 5, maxRunsPerDay: 10, runsToday: 2, spentUsdToday: 0.2, checkIntervalSeconds: "300" });
    expect(t.sources).toEqual([engine, kit]);
    const docsId = (await mara.agent.listAgents({ org: "acme" })).agents.find((a) => a.principal.display === "docs-writer")!.principal.id;
    const set = await mara.agent.setAgentFeedTrigger({ repo: api, agent: docsId, enabled: true, dailyUsdBudget: 1.5, maxRunsPerDay: 4, sources: [{ fullName: "lattice/docs" }] });
    expect(set).toMatchObject({ enabled: true, dailyUsdBudget: 1.5, maxRunsPerDay: 4, sources: [{ fullName: "lattice/docs" }] });
    expect((await mara.agent.listAgentFeedTriggers({ repo: api })).triggers.map((x) => x.agent?.display)).toEqual(["billing-fixer", "docs-writer", "triage"]);
    const docs = asHandle("docs");
    expect((await docs.user.isFollowingRepo({ repo: { fullName: "lattice/docs" } })).following).toBe(true);
    // Dropping a source unfollows it.
    await mara.agent.setAgentFeedTrigger({ repo: api, agent: docsId, enabled: false, sources: [] });
    expect((await docs.user.isFollowingRepo({ repo: { fullName: "lattice/docs" } })).following).toBe(false);
    expect(await fails(mara.agent.setAgentFeedTrigger({ repo: api, agent: docsId, enabled: true, dailyUsdBudget: -1 }))).toMatchObject({ code: "invalid_argument" });
    expect(await fails(mara.agent.setAgentFeedTrigger({ repo: api, agent: triageId, enabled: true, sources: [{ fullName: "lattice/internal-tools" }] }))).toMatchObject({ code: "not_found" });
    expect(await fails(mara.agent.setAgentFeedTrigger({ repo: api, agent: NOT_A_PRINCIPAL, enabled: true }))).toMatchObject({ code: "not_found", reason: "AGENT_NOT_FOUND" });
    expect(await fails(mara.agent.getAgentFeedTrigger({ repo: api, agent: NOT_A_PRINCIPAL }))).toMatchObject({ code: "not_found" });
  });

  it("is for maintainers: members read it, others and signed-out visitors cannot", async () => {
    const { mara, anon, asHandle } = world();
    const id = (await mara.agent.listAgents({ org: "acme" })).agents[0]!.principal.id;
    expect((await fails(anon.agent.listAgentFeedTriggers({ repo: api }))).code).toBe("unauthenticated");
    expect((await fails(asHandle("rhea").agent.listAgentFeedTriggers({ repo: api }))).code).toBe("not_found");
    // Lin is a member, not a maintainer: she can read, not change.
    const lin = asHandle("lin");
    expect((await lin.agent.listAgentFeedTriggers({ repo: api })).triggers).toHaveLength(2);
    expect(await fails(lin.agent.setAgentFeedTrigger({ repo: api, agent: id, enabled: true }))).toMatchObject({ code: "permission_denied" });
  });

  it("records an adopt run that opened an issue, and one that found the post not relevant", async () => {
    const { mara } = world();
    const { runs } = await mara.agent.listAdoptRuns({ repo: api });
    expect(runs.map((r) => r.outcome)).toEqual(["opened_issue", "not_relevant"]);
    const [opened, skipped] = runs as [T.AdoptRun, T.AdoptRun];
    expect(opened).toMatchObject({ agent: { display: "triage" }, post: { repo: kit, changeTitle: "Record usage on every run" }, opened: { kind: "issue", repo: api, title: "Record usage on every billing run" } });
    const issue = await mara.issue.getIssue({ repo: api, number: Number(opened.opened!.number) });
    expect(issue.inspiredBy?.postId).toBe(opened.postId);
    expect(issue.author?.display).toBe("triage");
    expect(skipped).toMatchObject({ reason: expect.stringContaining("never rebases"), post: { changeTitle: "Order conflicts by path" } });
    expect(skipped.opened).toBeUndefined();
    expect(Date.parse(opened.finishedAt!)).toBeGreaterThan(Date.parse(opened.startedAt!));
    const triage = runs[0]!.agent!.id;
    expect((await mara.agent.listAdoptRuns({ repo: api, agentPrincipalId: triage })).runs).toHaveLength(2);
    expect((await mara.agent.listAdoptRuns({ repo: api, agentPrincipalId: "nobody" })).runs).toEqual([]);
    // runsToday counts runs in the last 24 hours.
    const id = triage;
    expect((await mara.agent.getAgentFeedTrigger({ repo: api, agent: id })).runsToday).toBe(2);
  });
});
