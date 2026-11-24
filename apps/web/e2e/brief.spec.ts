import { expect, test } from "@playwright/test";
import { agentToken, expectAccessible, repoPath, seed } from "./helpers";

/** The change request page's tabs against the seeded briefs (e2e/seed/briefs.ts). */
const ui = () => repoPath("changes/" + seed.changes.ui.id);
const fix = () => repoPath("changes/" + seed.changes.fix.id);

test.describe("Change request brief", () => {
  test("brief tab answers outcome, status and needs you first", async ({ page }) => {
    await page.goto(ui());
    await expect(page.getByRole("heading", { level: 1, name: seed.changes.ui.title })).toBeVisible();
    await expect(page.getByRole("navigation", { name: "Change request" }).getByRole("link", { name: "Brief" })).toHaveAttribute("aria-current", "page");
    const brief = page.locator("[data-brief]");
    await expect(brief.locator("[data-outcome]")).toHaveText(seed.briefs.ui.outcome);
    await expect(brief.locator("[data-status]")).toBeVisible();
    // The five questions, in order: what, needs you, does it work, is it safe, who did it; then what changed and the conversation.
    const order = await brief.locator("[data-brief-section]").evaluateAll((els) => els.map((e) => e.getAttribute("data-brief-section")));
    expect(order).toEqual(["what", "needs", "works", "safe", "who", "changed", "conversation"]);
    const needs = page.getByRole("region", { name: "Needs you" });
    await expect(needs).toContainText(seed.briefs.ui.ask.question);
    for (const option of seed.briefs.ui.ask.options) await expect(needs.getByRole("button", { name: option })).toBeVisible();
    const safe = page.getByRole("region", { name: "Is it safe?" });
    await expect(safe).toContainText(seed.briefs.ui.risk);
    await expect(safe).toContainText(seed.briefs.ui.rollback);
    await expect(page.getByRole("region", { name: "Who did it" })).toContainText(seed.agents.claude.display);
    // The Post panel sits under "Who did it", once.
    await expect(page.getByRole("region", { name: "Who did it" }).getByText("Post", { exact: true }).first()).toBeVisible();
  });

  test("before and after slider works with the keyboard", async ({ page }) => {
    await page.goto(ui());
    const slider = page.getByRole("slider", { name: /Compare before and after/ });
    await expect(slider).toHaveAttribute("aria-valuenow", "50");
    await slider.focus();
    await slider.press("ArrowRight");
    await expect(slider).toHaveAttribute("aria-valuenow", "55");
    await slider.press("PageDown");
    await expect(slider).toHaveAttribute("aria-valuenow", "30");
    await slider.press("End");
    await expect(slider).toHaveAttribute("aria-valuenow", "100");
    await slider.press("Home");
    await expect(slider).toHaveAttribute("aria-valuenow", "0");
    // Both pictures are described in the caption, so neither depends on the divider's position.
    const figure = page.locator("figure[data-evidence=before_after]");
    await expect(figure).toContainText(seed.briefs.ui.beforeAlt);
    await expect(figure).toContainText(seed.briefs.ui.afterAlt);
  });

  test("video loads on demand with captions", async ({ page }) => {
    await page.goto(ui());
    const gallery = page.locator("[data-evidence-gallery]");
    await expect(gallery.locator("video")).toHaveCount(0);
    await gallery.getByRole("button", { name: new RegExp("Play video: " + seed.briefs.ui.videoTitle + ".*with captions") }).click();
    const video = gallery.locator("video[data-evidence-media=video]");
    await expect(video).toBeVisible();
    await expect(video.locator("track[kind=captions]")).toHaveAttribute("src", /^\/api\/evidence\//);
    const captions = await page.request.get((await video.locator("track").getAttribute("src")) ?? "");
    expect(captions.ok()).toBe(true);
    expect(await captions.text()).toMatch(/^WEBVTT/);
  });

  test("sources are labelled and stale evidence is flagged", async ({ page }) => {
    await page.goto(ui());
    const stale = page.locator("figure[data-nav-item]").filter({ hasText: seed.briefs.ui.staleTitle });
    await expect(stale.locator("[data-stale]")).toContainText("Stale: from r1");
    await expect(stale).toContainText("From the agent");
    const hero = page.locator("figure[data-nav-item]").filter({ hasText: seed.briefs.ui.heroTitle });
    await expect(hero.locator("[data-stale]")).toHaveCount(0);

    // CI evidence links to its check run on the Checks tab, which carries the anchor.
    await page.goto(fix());
    const ci = page.locator("figure[data-nav-item]").filter({ hasText: seed.briefs.fix.reportTitle }).locator("[data-source=ci]");
    await expect(ci).toContainText("Verified by CI");
    await ci.click();
    await expect(page).toHaveURL(new RegExp("/checks#check-" + seed.briefs.fix.checkRunId + "$"));
    const check = page.locator("#check-" + seed.briefs.fix.checkRunId);
    await expect(check).toContainText(seed.briefs.fix.checkName);
    await expect(check).toContainText("2 passed, 0 failed");
    await expect(check.locator("[data-check-evidence]")).toContainText(seed.briefs.fix.reportTitle);
  });

  test("change.md returns the brief, evidence and full diff", async ({ playwright, baseURL }) => {
    const url = repoPath("changes/" + seed.changes.ui.id + "/change.md");
    // An agent fetches it with its own token and no session cookie.
    const anon = await playwright.request.newContext({ baseURL, storageState: { cookies: [], origins: [] } });
    const get = async (token: string) => anon.get(url, { headers: { authorization: "Bearer " + token } });
    const res = await get(await agentToken("claude"));
    expect(res.status()).toBe(200);
    expect(res.headers()["content-type"]).toContain("text/markdown");
    const md = await res.text();
    expect(md).toContain("# " + seed.changes.ui.title);
    expect(md).toContain("**Outcome:** " + seed.briefs.ui.outcome);
    expect(md).toContain(seed.briefs.ui.heroTitle);
    expect(md).toContain(seed.briefs.ui.staleTitle);
    expect(md).toContain("diff --bee a/src/web/sessions.tsx");
    // A token the server refuses answers with its reason, not a document.
    const bad = await get("not-a-real-token-at-all");
    expect(bad.status()).toBe(401);
    expect((await bad.json()).code).toBe("unauthenticated");
    await anon.dispose();
  });

  test("?rev and ?since open the Code tab, and the tabs keep their own views", async ({ page, request }) => {
    const res = await request.get(fix() + "?rev=1&since=", { maxRedirects: 0 });
    expect(res.status()).toBe(307);
    expect(res.headers()["location"]).toContain("/changes/" + seed.changes.fix.id + "/code?rev=1");
    await page.goto(fix() + "?rev=2&since=1");
    await expect(page).toHaveURL(/\/code\?rev=2&since=1$/);
    await expect(page.getByRole("navigation", { name: "Change request" }).getByRole("link", { name: "Code" })).toHaveAttribute("aria-current", "page");
    await expect(page.getByRole("heading", { name: "r2 since r1" })).toBeVisible();

    await page.getByRole("navigation", { name: "Change request" }).getByRole("link", { name: "For agents" }).click();
    await expect(page).toHaveURL(/\/agents$/);
    const actions = page.locator("[data-actions]");
    await expect(actions.getByRole("button", { name: /Copy as CLI, HTTP or MCP: Read the change request/ })).toBeVisible();
    await expect(page.locator("[data-fetch-line]")).toContainText("Authorization: Bearer");
    await expect(page.getByRole("region", { name: "change.md, rendered" })).toContainText(seed.briefs.fix.outcome);
  });

  for (const tab of ["", "/code", "/checks", "/agents"]) {
    test("axe: the " + (tab.slice(1) || "brief") + " tab of the ui change", async ({ page }) => {
      await page.goto(ui() + tab);
      await expect(page.locator("main")).toBeVisible();
      await page.waitForTimeout(400);
      await expectAccessible(page);
    });
  }
});
