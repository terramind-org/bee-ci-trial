import AxeBuilder from "@axe-core/playwright";
import { expect, test, type Locator, type Page } from "@playwright/test";
import { PostState } from "../lib/gen/bee/v1/posts_pb";
import { flatPng } from "./seed/png";
import type * as HelpersModule from "./helpers";
import type * as SeedLibModule from "./seed/lib";

/**
 * Writing a change request's post from its page, against the real server
 * (scripts/check-web-e2e.sh, signed in as the dev root):
 *
 *   1. a person drafts text and an image with its description, and saves it;
 *   2. an agent's draft asks for a person; once its change lands it waits, a person approves it
 *      on the page, it publishes and shows in the follower's /feed;
 *   3. the person withdraws it, with a reason, and it leaves the feed;
 *   4. the panel passes axe.
 *
 * Every change is opened here, so the seeded world is left as it was. A server without
 * PostService (the seed then has no post) skips the file. The fixture world has its own
 * composer tests (lib/post-compose.test.ts), so this file does not run there.
 */
const FIXTURE = process.env.E2E_ADAPTER === "fixture";

test.describe.configure({ mode: "serial" });

type Helpers = typeof HelpersModule;
type SeedLib = typeof SeedLibModule;
let h: Helpers;
let lib: SeedLib;
let hasPosts = false;
const stamp = Date.now().toString(36);

test.beforeAll(async () => {
  if (FIXTURE) return;
  // Imported here: the helpers read the seed manifest, which the fixture run does not have.
  h = await import("./helpers");
  lib = await import("./seed/lib");
  hasPosts = !!(h.seed as { post?: unknown }).post;
});

test.beforeEach(() => {
  test.skip(FIXTURE, "the composer writes through the real server; the fixture has lib/post-compose.test.ts");
  test.skip(!hasPosts, "this server has no PostService, so the seed made no posts");
});

/** Opens a docs change on the seeded repo as `api` (docs, so policy lets an agent land it alone). */
async function openDocsChange(api: Awaited<ReturnType<Helpers["agentApi"]>>, who: string, title: string) {
  const repo = h.seed.repo;
  const head = await lib.commit(api, repo, await lib.tip(api, repo), { author: who, message: title, files: { ["docs/posts/" + stamp + "-" + title.length + ".md"]: "# " + title + "\n" } }, repo + ":post-compose:" + stamp + ":" + title);
  return api.change.openChange({ repo: lib.repoRef(repo), targetRef: "main", head, title, body: "", linkedIssues: [], labels: [] });
}

function panelOf(page: Page): Locator {
  return page.getByTestId("post-panel");
}

async function axe(page: Page, what: string) {
  // The app keeps an EventService/Subscribe stream open, so global network idle never arrives.
  // Wait for the composer state to render, then ensure its route loading skeleton is gone.
  await expect(panelOf(page).getByTestId("post-state")).toBeVisible();
  await expect(page.locator('[aria-busy="true"][aria-label="Loading"]')).toHaveCount(0);
  const results = await new AxeBuilder({ page }).withTags(["wcag2a", "wcag2aa", "wcag21a", "wcag21aa", "wcag22aa"]).analyze();
  const bad = results.violations.filter((v) => v.impact === "serious" || v.impact === "critical");
  expect(bad.map((v) => v.id + ": " + v.help + " → " + v.nodes.slice(0, 3).map((n) => n.target.join(" ")).join(", ")), what + ": serious/critical axe violations").toEqual([]);
}

test("a person drafts text and an image with its description, and saves it", async ({ page }) => {
  const root = h.api();
  const cr = await openDocsChange(root, "Dev <dev@bee.local>", "Note the release steps " + stamp);
  await page.goto(h.repoPath("changes/" + cr.id));
  const panel = panelOf(page);
  await expect(panel).toHaveAttribute("data-mode", "compose");
  await expect(panel.getByTestId("post-state")).toHaveText(/No post yet/);

  // The counter follows the text, and Save stays off until there is something to save.
  const text = "Release steps are written down now, so anyone can cut one. #docs " + stamp;
  await panel.getByRole("textbox", { name: "Post text" }).fill(text);
  await expect(panel.getByTestId("post-counter")).toContainText(String(text.length) + "/500");
  await expect(panel.getByTestId("post-counter")).toHaveAttribute("data-tone", "ok");

  // An image without a description cannot be saved; with one, it uploads and the draft saves.
  await panel.locator('input[type="file"]').setInputFiles({ name: "steps.png", mimeType: "image/png", buffer: Buffer.from(flatPng(64, 40, [40, 90, 200])) });
  await expect(panel.getByTestId("post-attachment")).toHaveCount(1);
  await panel.getByRole("button", { name: "Save draft" }).click();
  await expect(panel.getByTestId("post-error")).toContainText("Describe the image");
  await panel.getByRole("textbox", { name: /Image 1 description/ }).fill("A blue square standing in for the release checklist");
  await expect(panel.getByTestId("post-preview").getByRole("img", { name: "A blue square standing in for the release checklist" })).toBeVisible();
  await panel.getByRole("button", { name: "Save draft" }).click();
  await expect(panel.getByTestId("post-state")).toHaveText("Publishes when this change merges");
  await expect(panel).toHaveAttribute("data-state", "POST_STATE_DRAFT");

  const saved = await root.post.getChangePost({ repo: lib.repoRef(h.seed.repo), changeId: cr.id });
  expect(saved.state).toBe(PostState.DRAFT);
  expect(saved.text).toBe(text);
  expect(saved.media.map((m) => [m.contentType, m.altText, m.width, m.height])).toEqual([["image/png", "A blue square standing in for the release checklist", 64, 40]]);

  // It is there after a reload, uploaded; a stale version is refused with the server's reason.
  await page.reload();
  await expect(panelOf(page).getByTestId("post-attachment")).toHaveCount(1);
  await expect(panelOf(page).getByTestId("post-attachment").locator("img")).toHaveJSProperty("complete", true);
  await root.post.setChangePost({ repo: lib.repoRef(h.seed.repo), changeId: cr.id, text: text + " (edited elsewhere)", mediaIds: saved.media.map((m) => m.id) });
  await panelOf(page).getByRole("textbox", { name: "Post text" }).fill(text + " again");
  await panelOf(page).getByRole("button", { name: "Save post" }).click();
  await expect(panelOf(page).getByTestId("post-error")).toContainText("VERSION_MISMATCH");

  await axe(page, "the composer");
  await root.change.closeChange({ repo: lib.repoRef(h.seed.repo), id: cr.id });
});

test("an agent's draft waits for a person, who approves it; landing publishes it to the follower's feed, and it can be withdrawn", async ({ page }) => {
  const root = h.api();
  const agent = await h.agentApi("claude");
  const repo = lib.repoRef(h.seed.repo);
  const cr = await openDocsChange(agent, "claude-code <claude-code@agents.bee.local>", "Document the post flow " + stamp);
  const text = "The post flow is documented: draft, merge, approve, publish. " + stamp;
  await agent.post.setChangePost({ repo, changeId: cr.id, text, mediaIds: [] });
  // The dev root created the repo, so it follows it already; following again keeps it so.
  await root.user.followRepo({ repo }).catch(() => undefined);

  await page.goto(h.repoPath("changes/" + cr.id));
  const panel = panelOf(page);
  await expect(panel.getByTestId("post-state")).toHaveText("Needs a person to approve before it publishes");
  await expect(panel.getByRole("button", { name: "Approve post" })).toBeVisible();
  await expect(panel.getByRole("button", { name: "Withdraw post" })).toHaveCount(0);

  // The agent lands its own docs change; its post waits, because no person approved it yet.
  await agent.change.landChange({ repo, id: cr.id });
  expect((await root.post.getChangePost({ repo, changeId: cr.id })).state).toBe(PostState.PENDING_APPROVAL);
  await page.reload();
  await expect(panelOf(page).getByTestId("post-state")).toHaveText("Merged. Waiting for a person to approve this post");
  await axe(page, "a post waiting for approval");

  await panelOf(page).getByRole("button", { name: "Approve post" }).click();
  const published = panelOf(page).getByTestId("post-state").getByRole("link", { name: "Published" });
  await expect(published).toBeVisible();
  await expect(published).toHaveAttribute("href", new RegExp("^" + h.repoPath("posts/") + "[^/]+$"));
  await expect(panelOf(page).getByRole("button", { name: "Approve post" })).toHaveCount(0);

  await page.goto("/feed");
  const card = page.getByTestId("post-card").filter({ hasText: stamp }).filter({ hasText: "The post flow is documented" });
  await expect(card).toHaveCount(1);

  // Withdrawn by the repo's admin, with a reason: it leaves the feed and the panel says why.
  await page.goto(h.repoPath("changes/" + cr.id));
  await panelOf(page).getByRole("button", { name: "Withdraw post" }).click();
  const dialog = page.getByRole("dialog", { name: "Withdraw this post?" });
  await expect(dialog).toBeVisible();
  await axe(page, "the withdraw dialog");
  await dialog.getByRole("textbox", { name: /Reason/ }).fill("Superseded by the full guide");
  await dialog.getByRole("button", { name: "Withdraw", exact: true }).click();
  await expect(dialog).toHaveCount(0);
  await expect(panelOf(page).getByTestId("post-state")).toHaveText("Withdrawn: Superseded by the full guide");
  expect((await root.post.getChangePost({ repo, changeId: cr.id })).state).toBe(PostState.WITHDRAWN);

  await page.goto("/feed");
  await expect(page.getByRole("heading", { level: 1, name: "Feed" })).toBeVisible();
  await expect(page.getByTestId("post-card").filter({ hasText: stamp })).toHaveCount(0);
});
