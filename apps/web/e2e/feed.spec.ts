import AxeBuilder from "@axe-core/playwright";
import { expect, test, type Browser, type Locator, type Page } from "@playwright/test";
import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { PostMediaKind, PostState } from "../lib/gen/bee/v1/posts_pb";
import { Verdict } from "../lib/gen/bee/v1/forge_pb";
import { repoHref } from "../lib/routes";

/**
 * Posts as a reader meets them: the Following feed with images and a video, Like and Repost,
 * following a repo, a repo's Updates tab, and the permalink, signed in and signed out.
 *
 * Runs against the real server (scripts/check-web-e2e.sh, signed in as the dev root, who follows
 * the lattice repos it created) and against the fixture world (`bunx playwright test -c
 * playwright.fixture.config.ts`, signed in as the demo user Mara, who follows lattice/engine,
 * lattice/agent-kit and acme/sdk). On the real server, the seed's post (an image on
 * lattice/agentkit) is joined here by a video post on the same repo and a text post on the
 * private lattice/internal-roadmap, both landed before the tests run. A server without
 * PostService (the seed then has no post) skips the file.
 */
const FIXTURE = process.env.E2E_ADAPTER === "fixture";

const world = FIXTURE ? { publicRepo: "lattice/agent-kit", privateRepo: "acme/api" } : { publicRepo: "lattice/agentkit", privateRepo: "lattice/internal-roadmap" };
let hasPosts = FIXTURE;

test.describe.configure({ mode: "serial" });

/** A minimal MP4: an `ftyp` box and an empty `mdat`, enough for the server's magic-byte check. */
function tinyMp4(): Uint8Array {
  const ftyp = [0, 0, 0, 0x18, ...Buffer.from("ftypisom"), 0, 0, 2, 0, ...Buffer.from("isomiso2")];
  const mdat = [0, 0, 0, 8, ...Buffer.from("mdat")];
  return Uint8Array.from([...ftyp, ...mdat]);
}

test.beforeAll(async () => {
  if (FIXTURE) return;
  // Imported here: the helpers read the seed manifest, which the fixture run does not have.
  const { api, seed } = await import("./helpers");
  const { commit, connect, repoRef, tip } = await import("./seed/lib");
  hasPosts = !!(seed as { post?: unknown }).post;
  if (!hasPosts) return;
  const cred = readFileSync(join(process.env.E2E_DATA_DIR ?? "", "credentials"), "utf8");
  const endpoint = /endpoint = "(.*)"/.exec(cred)?.[1] ?? "";
  const reviewer = connect(endpoint, /^reviewer_token = "(.*)"/m.exec(cred)?.[1] ?? "");
  const root = api();
  const stamp = Date.now().toString(36);

  /** Opens a change as root, attaches the post, has the reviewer approve it, and lands it. */
  async function landPost(fullName: string, text: string, video: boolean) {
    const repo = repoRef(fullName);
    const head = await commit(root, fullName, await tip(root, fullName), { author: "Dev <dev@bee.local>", message: "Feed e2e " + stamp, files: { ["notes/feed-" + stamp + ".md"]: "# " + text + "\n" } }, fullName + ":feed-e2e:" + stamp);
    const cr = await root.change.openChange({ repo, targetRef: "main", head, title: (video ? "Record a walkthrough " : "Plan the next quarter ") + stamp, body: "", linkedIssues: [], labels: [] });
    const mediaIds: string[] = [];
    if (video) {
      const bytes = tinyMp4();
      const begin = await root.post.beginPostMediaUpload({ repo, changeId: cr.id, kind: PostMediaKind.VIDEO, contentType: "video/mp4", sizeBytes: BigInt(bytes.length) });
      const put = await fetch(new URL(begin.uploadUrl, endpoint), { method: "PUT", headers: begin.uploadHeaders, body: Buffer.from(bytes) });
      if (!put.ok) throw new Error("video upload: " + put.status + " " + (await put.text()));
      const sha256 = createHash("sha256").update(bytes).digest("hex");
      const media = await root.post.finalizePostMedia({ repo, mediaId: begin.mediaId, sha256, altText: "A short walkthrough of the change", width: 1280, height: 800, durationMs: 1000 });
      mediaIds.push(media.id);
    }
    await root.post.setChangePost({ repo, changeId: cr.id, text, mediaIds });
    await reviewer.change.submitReview({ repo, changeRequestId: cr.id, revision: 1, verdict: Verdict.APPROVE, body: "Fine." });
    await root.change.landChange({ repo, id: cr.id });
    const post = await root.post.getChangePost({ repo, changeId: cr.id });
    if (post.state !== PostState.PUBLISHED) throw new Error("the e2e post on " + fullName + " did not publish: " + PostState[post.state]);
  }
  await landPost(world.publicRepo, "A walkthrough of the new flow, recorded end to end. #demo", true);
  await landPost(world.privateRepo, "Members only: the plan for next quarter.", false);
});

test.beforeEach(async ({ page }) => {
  test.skip(!hasPosts, "this server has no PostService, so the seed made no posts");
  if (FIXTURE) await signInFixture(page);
});

/** The fixture signs in as Mara with any token of eight or more characters. */
async function signInFixture(page: Page) {
  await page.goto("/signin?next=" + encodeURIComponent("/feed"));
  await page.getByRole("textbox", { name: "Token" }).fill("fixture-token-0001");
  await page.getByRole("button", { name: "Sign in with token" }).click();
  await expect(page).toHaveURL(/\/feed$/);
  await expect(page.getByRole("heading", { level: 1, name: "Feed" })).toBeVisible();
}

async function axe(page: Page, what: string) {
  const results = await new AxeBuilder({ page }).withTags(["wcag2a", "wcag2aa", "wcag21a", "wcag21aa", "wcag22aa"]).analyze();
  const bad = results.violations.filter((v) => v.impact === "serious" || v.impact === "critical");
  expect(bad.map((v) => v.id + ": " + v.help + " → " + v.nodes.slice(0, 3).map((n) => n.target.join(" ")).join(", ")), what + ": serious/critical axe violations").toEqual([]);
}

/** An image has loaded through the media route's redirect once it has a natural size. */
async function expectLoaded(img: Locator) {
  await img.scrollIntoViewIfNeeded();
  await expect.poll(() => img.evaluate((el: HTMLImageElement) => el.complete && el.naturalWidth > 0), { timeout: 15_000 }).toBe(true);
}

/** The permalink of the first post card in a list page, read from its time link. */
async function firstPermalink(page: Page, path: string): Promise<string> {
  await page.goto(path);
  const href = await page.getByTestId("post-card").first().locator('a[href*="/posts/"]').first().getAttribute("href");
  expect(href).toMatch(/\/posts\/[^/]+$/);
  return href as string;
}

async function signedOut(browser: Browser) {
  const ctx = await browser.newContext({ storageState: { cookies: [], origins: [] } });
  return { page: await ctx.newPage(), close: () => ctx.close() };
}

test("the feed shows posts with images and video, and passes axe", async ({ page }) => {
  await page.goto("/feed");
  await expect(page.getByRole("heading", { level: 1, name: "Feed" })).toBeVisible();
  const cards = page.getByTestId("post-card");
  await expect(cards.first()).toBeVisible();
  // Images come through the permalink's media route and load.
  const img = page.getByTestId("post-media").first().locator("img").first();
  await expect(img).toHaveAttribute("src", /\/posts\/[^/]+\/media\/[^/]+$/);
  await expect(img).toHaveAttribute("alt", /\S/);
  await expectLoaded(img);
  // The video is a native player that only fetches metadata until it is played.
  const video = page.getByTestId("post-video").first();
  await video.scrollIntoViewIfNeeded();
  await expect(video).toBeVisible();
  await expect(video).toHaveAttribute("preload", "metadata");
  await expect(video).toHaveAttribute("controls", "");
  expect(await video.evaluate((v: HTMLVideoElement) => v.playsInline)).toBe(true);
  // The byline names the repo and the footer names the merged change.
  await expect(cards.first()).toContainText("Merged");
  await page.evaluate(() => window.scrollTo(0, 0));
  await axe(page, "/feed");
});

test("a signed-out visitor to /feed is sent to sign in", async ({ browser }) => {
  const anon = await signedOut(browser);
  await anon.page.goto("/feed");
  await expect(anon.page).toHaveURL(/\/signin\?next=%2Ffeed$|\/signin\?next=\/feed$/);
  await anon.close();
});

/** Toggles one of a card's actions, checks the count moved and the state survives a reload, then puts it back. */
async function toggleAction(page: Page, path: string, label: "Like" | "Repost") {
  await page.goto(path);
  const button = () => page.getByTestId("post-card").first().getByRole("button", { name: new RegExp("^" + label + " \\d+$") });
  const count = async () => Number((await button().innerText()).replace(/\D/g, ""));
  const before = (await button().getAttribute("aria-pressed")) === "true";
  const n = await count();
  await button().click();
  await expect(button()).toHaveAttribute("aria-pressed", String(!before));
  await expect.poll(count).toBe(before ? n - 1 : n + 1);
  // The server has it: a fresh render shows the same state.
  await expect(async () => {
    await page.reload();
    await expect(button()).toHaveAttribute("aria-pressed", String(!before), { timeout: 2_000 });
  }).toPass({ timeout: 15_000 });
  expect(await count()).toBe(before ? n - 1 : n + 1);
  await button().click();
  await expect(button()).toHaveAttribute("aria-pressed", String(before));
  await expect.poll(count).toBe(n);
}

test("like and repost a post", async ({ page }) => {
  const permalink = await firstPermalink(page, repoHref(world.publicRepo, "updates"));
  await toggleAction(page, permalink, "Like");
  await toggleAction(page, permalink, "Repost");
});

test("follow and unfollow a repo from its header", async ({ page }) => {
  await page.goto(repoHref(world.publicRepo, "updates"));
  const button = page.getByRole("button", { name: new RegExp("^(Follow|Following) " + world.publicRepo.replace("/", "\\/")) });
  await expect(button).toBeVisible();
  const before = (await button.getAttribute("aria-pressed")) === "true";
  const count = async () => Number((await button.innerText()).replace(/\D/g, "") || "0");
  const n = await count();
  await button.click();
  await expect(button).toHaveAttribute("aria-pressed", String(!before));
  await expect(button).toContainText(before ? "Follow" : "Following");
  await expect.poll(count).toBe(before ? n - 1 : n + 1);
  await expect(async () => {
    await page.reload();
    await expect(button).toHaveAttribute("aria-pressed", String(!before), { timeout: 2_000 });
  }).toPass({ timeout: 15_000 });
  await button.click();
  await expect(button).toHaveAttribute("aria-pressed", String(before));
  await expect.poll(count).toBe(n);
});

test("the Updates tab lists the repo's posts and passes axe", async ({ page }) => {
  await page.goto(repoHref(world.publicRepo));
  await page.getByRole("link", { name: "Updates", exact: true }).first().click();
  await expect(page).toHaveURL(new RegExp(repoHref(world.publicRepo, "updates") + "$"));
  await expect(page.getByRole("heading", { level: 1, name: "Updates" })).toBeVisible();
  await expect(page.getByTestId("post-card").first()).toBeVisible();
  await expect(page.getByTestId("post-card").first()).toContainText(world.publicRepo);
  await axe(page, "updates");
});

test("a public post's permalink loads signed out, with link previews, and passes axe", async ({ page, browser }) => {
  // The image post: the seed's on the real server, a post with screenshots in the fixture.
  await page.goto(repoHref(world.publicRepo, "updates"));
  const withImage = page.getByTestId("post-card").filter({ has: page.getByTestId("post-media") }).first();
  const permalink = (await withImage.locator('a[href*="/posts/"]').first().getAttribute("href")) as string;
  const text = (await withImage.locator("p.whitespace-pre-wrap").innerText()).slice(0, 30);

  const anon = await signedOut(browser);
  const res = await anon.page.goto(permalink);
  expect(res?.status()).toBe(200);
  const card = anon.page.getByTestId("post-card");
  await expect(card).toBeVisible();
  await expect(card).toContainText(text);
  await expect(anon.page.getByRole("button", { name: /^Like/ })).toHaveCount(0);
  await expect(card.getByRole("link", { name: /Like.*sign in first/ })).toBeVisible();
  await expectLoaded(card.locator("img").first());
  // OpenGraph and Twitter: "<repo>: <change title>", the text, and the first image through the media route.
  await expect(anon.page.locator('meta[property="og:title"]')).toHaveAttribute("content", new RegExp("^" + world.publicRepo + ": "));
  await expect(anon.page.locator('meta[property="og:image"]').first()).toHaveAttribute("content", /^https?:\/\/[^/]+\/.+\/posts\/[^/]+\/media\/[^/]+$/);
  await expect(anon.page.locator('meta[name="twitter:card"]')).toHaveAttribute("content", "summary_large_image");
  await axe(anon.page, "permalink signed out");
  await anon.close();
});

test("a private repo's post is a 404 signed out, media included", async ({ page, browser }) => {
  const permalink = await firstPermalink(page, repoHref(world.privateRepo, "updates"));
  const ok = await page.goto(permalink);
  expect(ok?.status()).toBe(200);
  const anon = await signedOut(browser);
  const res = await anon.page.goto(permalink);
  expect(res?.status()).toBe(404);
  await expect(anon.page.locator("body")).not.toContainText("Members only");
  const media = await anon.page.request.get(permalink + "/media/anything", { maxRedirects: 0 });
  expect(media.status()).toBe(404);
  await anon.close();
});
