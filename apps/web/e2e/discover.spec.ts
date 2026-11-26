import { expect, test, type Page } from "@playwright/test";
import { expectAccessible, seed } from "./helpers";
import { BUSIEST_TOPIC, DISCOVERY_ORG, PRIVATE_REPO, PUBLIC_REPOS } from "./seed/discovery";

/** The real page, not its loading skeleton (which carries the same heading). */
const settled = (page: Page) => expect(page.locator('[aria-busy="true"]')).toHaveCount(0);

/** Discovery pages are public: every test here runs signed out unless it says otherwise. */
test.use({ storageState: { cookies: [], origins: [] } });

test.describe("explore", () => {
  test("renders every section for a signed-out visitor", async ({ page }) => {
    await page.goto("/explore");
    await expect(page.getByRole("heading", { level: 1, name: "Explore" })).toBeVisible();
    for (const name of ["Featured topics", "Trending repositories this week", "Most active agents this week", "Top reviewers", "Rising repositories"]) {
      await expect(page.getByRole("heading", { name }), name).toBeVisible();
    }
    await expect(page.getByRole("link", { name: BUSIEST_TOPIC, exact: true }).first()).toBeVisible();
    await expect(page.getByRole("link", { name: PUBLIC_REPOS[0] }).first()).toBeVisible();
    await expect(page.getByText(PRIVATE_REPO)).toHaveCount(0);
    await expect(page.getByRole("link", { name: "See all" })).toHaveAttribute("href", "/trending");
    await expect(page.getByRole("heading", { name: "Starred by people you follow" })).toHaveCount(0);
  });

  test("Trending and Leaderboards are sub-views of Explore, and the old routes still open", async ({ page }) => {
    await page.goto("/explore");
    const tabs = page.getByRole("navigation", { name: "Explore sections" });
    await expect(tabs.getByRole("link")).toHaveText(["Overview", "Trending", "Leaderboards"]);
    await expect(tabs.getByRole("link", { name: "Overview" })).toHaveAttribute("aria-current", "page");
    await tabs.getByRole("link", { name: "Trending" }).click();
    await expect(page).toHaveURL(/\/trending$/);
    await expect(page.getByRole("heading", { level: 1, name: "Trending" })).toBeVisible();
    await expect(page.getByRole("navigation", { name: "Explore sections" }).getByRole("link", { name: "Trending" })).toHaveAttribute("aria-current", "page");
    await page.getByRole("navigation", { name: "Explore sections" }).getByRole("link", { name: "Leaderboards" }).click();
    await expect(page).toHaveURL(/\/leaderboards$/);
    await expect(page.getByRole("heading", { level: 1, name: "Leaderboards" })).toBeVisible();
    await page.getByRole("navigation", { name: "Explore sections" }).getByRole("link", { name: "Overview" }).click();
    await expect(page).toHaveURL(/\/explore$/);
  });

  test("entries are large and unboxed: a mark of 36px or more, and no bordered cards", async ({ page }) => {
    await page.goto("/explore");
    const entry = page.getByRole("article", { name: PUBLIC_REPOS[0] }).first();
    await expect(entry).toBeVisible();
    const mark = await entry.getByRole("img").first().boundingBox();
    expect(mark?.width ?? 0).toBeGreaterThanOrEqual(36);
    expect(await entry.evaluate((el) => getComputedStyle(el).borderTopWidth)).toBe("0px");
  });

  test("axe", async ({ page }) => {
    await page.goto("/explore");
    await expect(page.getByRole("heading", { level: 1, name: "Explore" })).toBeVisible();
    await settled(page);
    await expectAccessible(page);
  });
});

test.describe("trending", () => {
  test("switches kind, window and language through the URL", async ({ page }) => {
    await page.goto("/trending");
    await expect(page.getByRole("link", { name: PUBLIC_REPOS[0] }).first()).toBeVisible();

    await page.getByRole("navigation", { name: "Kind" }).getByRole("link", { name: "Agents" }).click();
    await expect(page).toHaveURL(/kind=agents/);
    await expect(page.getByRole("navigation", { name: "Kind" }).getByRole("link", { name: "Agents" })).toHaveAttribute("aria-current", "page");
    await expect(page.getByRole("img", { name: /\(agent\)/ }).first()).toBeVisible();

    await page.getByRole("link", { name: "This month" }).click();
    await expect(page).toHaveURL(/kind=agents/);
    await expect(page).toHaveURL(/since=month/);

    await page.getByRole("navigation", { name: "Kind" }).getByRole("link", { name: "Repositories" }).click();
    await expect(page).not.toHaveURL(/kind=/);
    await page.getByLabel("Language").fill("Rust");
    await page.getByRole("button", { name: "Apply" }).click();
    await expect(page).toHaveURL(/language=Rust/);
    await expect(page).toHaveURL(/since=month/);
    await expect(page.getByLabel("Language")).toHaveValue("Rust");
    // Either the repos in that language or the honest empty state, never another language's repos.
    await expect(page.getByRole("list", { name: /Trending repositories/ }).or(page.getByText("Nothing is trending this month yet"))).toBeVisible();
    await expect(page.getByText("TypeScript", { exact: true })).toHaveCount(0);
  });

  test("an unknown kind or window falls back to the defaults", async ({ page }) => {
    await page.goto("/trending?kind=bogus&since=decade");
    await expect(page.getByRole("navigation", { name: "Kind" }).getByRole("link", { name: "Repositories" })).toHaveAttribute("aria-current", "page");
    await expect(page.getByRole("navigation", { name: "Time window" }).getByRole("link", { name: "This week" })).toHaveAttribute("aria-current", "page");
  });

  test("starring while signed out asks to sign in and comes back", async ({ page }) => {
    await page.goto("/trending");
    await page.getByRole("link", { name: /^Star lattice\/agentkit/ }).click();
    await expect(page).toHaveURL(/\/signin\?next=%2Ftrending/);
  });

  test("axe", async ({ page }) => {
    await page.goto("/trending");
    await expect(page.getByRole("heading", { level: 1, name: "Trending" })).toBeVisible();
    await settled(page);
    await expectAccessible(page);
  });
});

test.describe("leaderboards", () => {
  test("show an agent at rank 1 for landed changes, with its runtime", async ({ page }) => {
    await page.goto("/leaderboards");
    await expect(page.getByRole("heading", { level: 1, name: "Leaderboards" })).toBeVisible();
    await expect(page.getByText(/Computed/)).toBeVisible();
    const first = page.locator("li[data-rank='1']");
    await expect(first.getByRole("link", { name: /^(claude-code|codex)$/ })).toBeVisible();
    await expect(first.getByRole("img", { name: /\(agent\)/ })).toBeVisible();
    await expect(first.getByText(/^(claude-code|codex)$/).last()).toBeVisible();
    await expect(first.locator(".num").nth(1)).not.toHaveText("0");
    // A hairline share bar sits under the row, and the share is also printed.
    await expect(first.getByText(/^\d+%$/)).toBeVisible();
    await expect(page.getByRole("table")).toHaveCount(0);
  });

  test("switches board and window through the URL", async ({ page }) => {
    await page.goto("/leaderboards");
    await page.getByRole("link", { name: "Repositories by stars gained" }).click();
    await expect(page).toHaveURL(/board=repos-stars/);
    await expect(page.locator("li[data-rank='1']").getByRole("link", { name: PUBLIC_REPOS[0] })).toBeVisible();
    await page.getByRole("link", { name: "This month" }).click();
    await expect(page).toHaveURL(/board=repos-stars/);
    await expect(page).toHaveURL(/since=month/);
    await expect(page.locator("li[data-rank]").first()).toBeVisible();
  });

  test("a board offers Copy as for its ListLeaderboard request", async ({ page }) => {
    await page.goto("/leaderboards?board=humans-reviews");
    // The redesigned board exposes Copy as through its page menu.
    await page.getByRole("button", { name: "More actions" }).click();
    await page.getByRole("menuitem", { name: "Copy as CLI, HTTP or MCP" }).click();
    const dialog = page.getByRole("dialog");
    await expect(dialog.getByLabel("CLI command")).toContainText("leaderboard");
    await dialog.getByRole("tab", { name: "HTTP" }).click();
    await expect(dialog.getByLabel("curl request")).toContainText("SearchService/ListLeaderboard");
    await expect(dialog.getByLabel("curl request")).toContainText("LEADERBOARD_METRIC_REVIEWS");
  });

  test("axe", async ({ page }) => {
    await page.goto("/leaderboards");
    await expect(page.locator("li[data-rank]").first()).toBeVisible();
    await settled(page);
    await expectAccessible(page);
  });
});

test.describe("topics", () => {
  test("a topic page lists its repositories and can sort them", async ({ page }) => {
    await page.goto("/topics/" + BUSIEST_TOPIC);
    await expect(page.getByRole("heading", { level: 1 })).toBeVisible();
    await expect(page.getByRole("link", { name: PUBLIC_REPOS[0] }).first()).toBeVisible();
    await page.getByRole("link", { name: "Recently updated" }).click();
    await expect(page).toHaveURL(/sort=updated/);
    await expect(page.getByRole("link", { name: PUBLIC_REPOS[0] }).first()).toBeVisible();
    await settled(page);
    await expectAccessible(page);
  });

  test("an unknown topic shows the not-found page", async ({ page }) => {
    await page.goto("/topics/no-such-topic-here");
    await expect(page.getByRole("heading", { name: "Nothing here" })).toBeVisible();
  });
});

test.describe("atom feeds", () => {
  const entries = (xml: string) => [...xml.matchAll(/<entry>/g)].length;

  test("the repo feed is valid Atom with lands", async ({ request }) => {
    const res = await request.get("/" + PUBLIC_REPOS[0] + "/feed.atom");
    expect(res.status()).toBe(200);
    expect(res.headers()["content-type"]).toContain("application/atom+xml");
    const xml = await res.text();
    expect(xml).toMatch(/^<\?xml version="1.0" encoding="utf-8"\?>\n<feed xmlns="http:\/\/www.w3.org\/2005\/Atom">/);
    expect(xml.trimEnd().endsWith("</feed>")).toBe(true);
    expect(xml).toContain("<title>" + PUBLIC_REPOS[0]);
    expect(entries(xml)).toBeGreaterThan(0);
    expect(xml).toMatch(/<id>tag:[^,]+,2026:land\//);
    expect(xml.match(/<entry>/g)?.length).toBe(xml.match(/<\/entry>/g)?.length);
  });

  test("the owner feed covers the org's public repositories", async ({ request }) => {
    const res = await request.get("/" + DISCOVERY_ORG + "/feed.atom");
    expect(res.status()).toBe(200);
    expect(res.headers()["content-type"]).toContain("application/atom+xml");
    const xml = await res.text();
    expect(entries(xml)).toBeGreaterThan(0);
    expect(xml).toContain(PUBLIC_REPOS[0]);
    expect(xml).not.toContain(PRIVATE_REPO);
  });

  test("private, missing and reserved names are 404", async ({ request }) => {
    expect((await request.get("/" + PRIVATE_REPO + "/feed.atom")).status()).toBe(404);
    expect((await request.get("/" + DISCOVERY_ORG + "/no-such-repo/feed.atom")).status()).toBe(404);
    expect((await request.get("/no-such-owner-here/feed.atom")).status()).toBe(404);
    expect((await request.get("/explore/feed.atom")).status()).toBe(404);
  });

  test("a signed-in session does not widen the feed", async ({ browser }) => {
    const ctx = await browser.newContext({ storageState: "test-results/.auth/session.json" });
    const res = await ctx.request.get("/" + seed.org + "/feed.atom");
    // The seeded playground repos are private: a feed fetched with a session still lists none of them.
    expect([200, 404]).toContain(res.status());
    expect(await res.text()).not.toContain("<entry>");
    await ctx.close();
  });
});
