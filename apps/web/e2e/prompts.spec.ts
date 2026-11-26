import { expect, test } from "@playwright/test";
import { expectAccessible } from "./helpers";
import { PROMPTS } from "../lib/prompts";

test.describe("Agent prompts", () => {
  test("/prompts lists every prompt in its group, filled in for this server", async ({ page }) => {
    await page.goto("/prompts");
    await expect(page.getByRole("heading", { level: 1, name: "Agent prompts" })).toBeVisible();
    await expect(page.getByRole("heading", { level: 2 })).toHaveText(["Get started", "Daily work", "Operate"]);
    for (const p of PROMPTS) await expect(page.getByRole("heading", { level: 3, name: p.title })).toBeVisible();
    await expect(page.getByRole("link", { name: "llms.txt" })).toHaveAttribute("href", "/llms.txt");
    await expect(page.getByRole("link", { name: /Agents guide/ })).toHaveAttribute("href", /docs\/agents-guide\.md$/);
  });

  test("the body is collapsed, expands with the keyboard, and has no placeholder left", async ({ page }) => {
    await page.goto("/prompts");
    const card = page.locator("#setup-repo");
    const toggle = card.getByRole("button", { name: /Show the prompt/ });
    await expect(toggle).toHaveAttribute("aria-expanded", "false");
    await expect(card.locator("pre")).toBeHidden();
    await toggle.focus();
    await page.keyboard.press("Enter");
    await expect(card.getByRole("button", { name: /Hide the prompt/ })).toHaveAttribute("aria-expanded", "true");
    const body = card.locator("pre");
    await expect(body).toBeVisible();
    await expect(body).toContainText("bee git setup --repo");
    await expect(body).toContainText("/mcp");
    expect(await body.innerText()).not.toContain("{{");
  });

  test("Copy puts the filled prompt on the clipboard and says so", async ({ page, context }) => {
    await context.grantPermissions(["clipboard-read", "clipboard-write"]);
    await page.goto("/prompts");
    const card = page.locator("#open-and-land-change");
    // Clicking before hydration does nothing: retry until the toast shows.
    await expect(async () => {
      await card.getByRole("button", { name: "Copy prompt: Open a change request and land it" }).click();
      await expect(page.getByRole("status").filter({ hasText: "Prompt copied" })).toBeVisible({ timeout: 1000 });
    }).toPass({ timeout: 15_000 });
    const copied = await page.evaluate(() => navigator.clipboard.readText());
    expect(copied).toContain("bee change evaluate-landing");
    expect(copied).not.toContain("{{");
    expect(copied).toBe(await page.locator("#open-and-land-change pre").evaluate((el) => el.textContent ?? ""));
  });

  test("/llms.txt is plain text and names every prompt", async ({ request }) => {
    const res = await request.get("/llms.txt");
    expect(res.status()).toBe(200);
    expect(res.headers()["content-type"]).toContain("text/plain");
    const text = await res.text();
    expect(text.split("\n")[0]).toBe("# Beehive");
    for (const p of PROMPTS) expect(text).toContain(p.title);
    expect(text).toContain("/mcp");
    expect(text).not.toContain("{{");
  });

  test.describe("signed out", () => {
    test.use({ storageState: { cookies: [], origins: [] } });

    test("/prompts and /llms.txt need no sign-in", async ({ page, request }) => {
      await page.goto("/prompts");
      await expect(page).toHaveURL(/\/prompts$/);
      await expect(page.getByRole("heading", { level: 1, name: "Agent prompts" })).toBeVisible();
      await expect(page.getByRole("link", { name: "Sign in" })).toBeVisible();
      expect((await request.get("/llms.txt")).status()).toBe(200);
    });
  });

  for (const theme of ["light", "dark"] as const) {
    test.describe("accessibility · " + theme, () => {
      test.use({ colorScheme: theme });

      test("axe: /prompts, collapsed and expanded", async ({ page }) => {
        await page.goto("/prompts");
        await expect(page.getByRole("heading", { level: 3, name: "Set up Bee for a repository" })).toBeVisible();
        await expectAccessible(page);
        await page.locator("#setup-repo").getByRole("button", { name: /Show the prompt/ }).click();
        await expect(page.locator("#setup-repo pre")).toBeVisible();
        await expectAccessible(page);
      });
    });
  }
});
