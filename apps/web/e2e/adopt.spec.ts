import AxeBuilder from "@axe-core/playwright";
import { expect, test, type Page } from "@playwright/test";
import type * as HelpersModule from "./helpers";

/**
 * Adopting another repo's upgrade: the "Inspired by" banner on an issue, the permalink's
 * adoption count and list, and an agent's page with what it follows, its feed trigger and its
 * adopt runs.
 *
 * Runs against the fixture world (`bunx playwright test -c playwright.fixture.config.ts adopt`,
 * signed in as Mara, a maintainer of acme/api, whose agent `triage` adopted lattice/agent-kit's
 * usage post) and against the real server (scripts/check-web-e2e.sh, signed in as the dev root).
 * On the real server the spec opens its own issue from the seeded post, so the seeded world is only
 * added to; a server without PostService (the seed then has no post) skips the file.
 */
const FIXTURE = process.env.E2E_ADAPTER === "fixture";

test.describe.configure({ mode: "serial" });

type Helpers = typeof HelpersModule;
let h: Helpers;
let hasPosts = FIXTURE;
/** The world the tests read: the repo with the adopted issue, its title and number, the agent, and where the post lives. */
let world = { repo: "acme/api", issue: "Record usage on every billing run", number: 0, source: "lattice/agent-kit", agent: "billing", sources: ["lattice/engine"] };

test.beforeAll(async () => {
  if (FIXTURE) return;
  // Imported here: the helpers read the seed manifest, which the fixture run does not have.
  h = await import("./helpers");
  const post = (h.seed as { post?: { id: string; repo: string } }).post;
  hasPosts = !!post;
  if (!post) return;
  const lib = await import("./seed/lib");
  const issue = await h.api().issue.createIssue({
    repo: lib.repoRef(h.seed.repo),
    title: "Adopt the post's approach " + Date.now().toString(36),
    body: "Port what the post describes.",
    labels: [],
    assignees: [],
    inspiredByPostId: post.id,
  });
  world = { repo: h.seed.repo, issue: issue.title, number: Number(issue.number), source: post.repo, agent: h.seed.agents.claude.id, sources: [] };
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
}

async function axe(page: Page, what: string) {
  await expect(page.locator('[aria-busy="true"][aria-label="Loading"]')).toHaveCount(0);
  const results = await new AxeBuilder({ page }).withTags(["wcag2a", "wcag2aa", "wcag21a", "wcag21aa", "wcag22aa"]).analyze();
  const bad = results.violations.filter((v) => v.impact === "serious" || v.impact === "critical");
  expect(bad.map((v) => v.id + ": " + v.help + " → " + v.nodes.slice(0, 3).map((n) => n.target.join(" ")).join(", ")), what + ": serious/critical axe violations").toEqual([]);
}

/** An agent's page. The fixture names agents by an id it makes up, so the fixture finds it in the agents list. */
async function openAgent(page: Page, agent: string) {
  if (!FIXTURE) return page.goto("/" + world.repo + "/agents/" + agent);
  await page.goto("/" + world.repo + "/agents");
  await page.locator('a[href*="/agents/"]').filter({ hasText: agent }).first().click();
  await expect(page.getByRole("heading", { level: 1, name: new RegExp(agent) })).toBeVisible();
}

/** The adopted issue's page. The fixture's number is not fixed, so it is found in the issue list. */
async function openAdoptedIssue(page: Page) {
  if (world.number) return page.goto("/" + world.repo + "/work/" + world.number);
  await page.goto("/" + world.repo + "/work");
  await page.getByRole("link", { name: new RegExp(world.issue) }).first().click();
  await expect(page.getByRole("heading", { level: 1, name: world.issue })).toBeVisible();
}

test("the banner shows on an adopted issue, quotes the post and links to it", async ({ page }) => {
  await openAdoptedIssue(page);
  const banner = page.locator("[data-inspired-by]");
  await expect(banner).toBeVisible();
  await expect(banner).toContainText("Inspired by " + world.source + "’s post");
  await expect(banner.locator("[data-inspired-excerpt]")).toContainText(/\S{3,}/);
  // The real server's seeded post carries an image; the fixture's usage post has none.
  if (!FIXTURE) await expect(banner.locator("[data-inspired-thumb]")).toHaveAttribute("src", /\/posts\/[^/]+\/media\/[^/]+$/);
  await axe(page, "adopted issue");
  await banner.getByRole("link", { name: /Inspired by/ }).click();
  await expect(page).toHaveURL(new RegExp("/" + world.source + "/posts/[^/]+$"));
});

test("an issue that was not adopted from a post has no banner", async ({ page }) => {
  await page.goto("/" + world.repo + "/work");
  const plain = page.getByRole("link").filter({ hasNotText: world.issue }).and(page.locator('a[href*="/work/"]'));
  await plain.first().click();
  await expect(page.getByRole("heading", { level: 1 })).toBeVisible();
  await expect(page.locator("[data-inspired-by]")).toHaveCount(0);
});

test("the permalink counts the adoptions and lists each one", async ({ page }) => {
  await openAdoptedIssue(page);
  await page.locator("[data-inspired-by]").getByRole("link", { name: /Inspired by/ }).click();
  await expect(page.getByTestId("post-card")).toBeVisible();
  const count = page.getByTestId("post-adoptions");
  await expect(count).toContainText(/Adopted by \d+ repos?/);
  await count.click();
  const section = page.getByTestId("post-adoptions-section");
  await expect(section).toBeInViewport();
  await expect(section.getByRole("heading", { name: "Adoptions" })).toBeVisible();
  const row = section.getByRole("listitem").filter({ hasText: world.issue });
  await expect(row).toContainText(world.repo);
  await expect(row).toContainText("opened an issue");
  await row.getByRole("link", { name: new RegExp(world.issue) }).click();
  await expect(page.getByRole("heading", { level: 1, name: world.issue })).toBeVisible();
  await page.goBack();
  await axe(page, "permalink with adoptions");
});

test("the agent page shows what the agent follows and its recent adopt runs", async ({ page }) => {
  test.skip(!FIXTURE, "the real server's seed has no feed trigger runs; the fixture seeds them");
  await openAgent(page, "triage");
  const follows = page.getByRole("region", { name: "Follows" });
  await expect(follows).toContainText("lattice/engine");
  await expect(follows).toContainText("lattice/agent-kit");
  const runs = page.getByRole("region", { name: "Recent adopt runs" }).getByTestId("adopt-run");
  const opened = runs.filter({ hasText: "Opened issue" });
  await expect(opened).toContainText("Record usage on every billing run");
  await expect(opened.getByRole("link", { name: /^#\d+$/ })).toHaveAttribute("href", /\/acme\/api\/work\/\d+$/);
  await expect(opened).toContainText("Evaluated lattice/agent-kit");
  await expect(runs.filter({ hasText: "Not relevant:" })).toContainText("never rebases changes");
  await axe(page, "agent page");
});

test("a maintainer enables the feed trigger, and it persists", async ({ page }) => {
  await openAgent(page, world.agent);
  const panel = page.getByRole("region", { name: "Feed trigger" });
  const form = panel.getByRole("form", { name: "Feed trigger settings" });
  await expect(form).toBeVisible();
  if (await form.getByRole("checkbox").isChecked()) await form.getByRole("checkbox").uncheck();
  await form.getByLabel("Sources", { exact: true }).fill(world.sources.join("\n"));
  await form.getByLabel("Daily budget (USD)").fill("3.5");
  await form.getByLabel("Runs per day").fill("4");
  await form.getByRole("checkbox").check();
  await form.getByRole("button", { name: "Save feed trigger" }).click();
  if (!FIXTURE) {
    // No API registers an agent definition, so the seeded agents are not enabled in the org and the
    // server refuses with its reason; the page shows it and keeps the settings. The fixture enables.
    await expect(form.getByRole("alert")).toContainText("is not enabled in this org");
    await axe(page, "agent feed trigger refusal");
    return;
  }
  await expect(page.getByRole("status").filter({ hasText: "Feed trigger on" })).toBeVisible();
  await expect(panel.getByTestId("feed-trigger-summary")).toContainText("evaluates new posts");
  await expect(panel).toContainText(/Up to 4 runs and \$3\.50 a day/);
  // It is stored, not just shown: a fresh load reads the same settings back.
  await page.reload();
  const again = page.getByRole("region", { name: "Feed trigger" }).getByRole("form", { name: "Feed trigger settings" });
  await expect(again.getByRole("checkbox")).toBeChecked();
  await expect(again.getByLabel("Runs per day")).toHaveValue("4");
  for (const s of world.sources) await expect(page.getByRole("region", { name: "Follows" })).toContainText(s);
  // Copy as gives the same call for a script.
  await again.getByRole("button", { name: /Copy as/ }).click();
  await expect(page.getByRole("dialog")).toContainText("set-agent-feed-trigger");
  await page.keyboard.press("Escape");
  await axe(page, "agent feed trigger");
});
