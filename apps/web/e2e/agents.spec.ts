import { expect, test } from "@playwright/test";
import { repoPath, seed } from "./helpers";

test.describe("Agents", () => {
  test("lists agents as large rows that say what each is doing", async ({ page }) => {
    await page.goto(repoPath("agents"));
    const claude = page.locator("[data-nav-item]").filter({ hasText: "claude-code" }).first();
    await expect(claude).toContainText("owned by dev");
    await expect(claude).toContainText("claude-opus-4-1");
    await expect(claude).toContainText(/open change request|Idle right now|[Ww]orking/);
    await expect(claude).toContainText(/merged \d+ changes? this week|nothing merged this week/);
    await expect(claude).toContainText("spent $");
    await expect(claude.getByRole("img", { name: "claude-code (agent)" })).toBeVisible();
    const codex = page.locator("[data-nav-item]").filter({ hasText: "gpt-5-codex" });
    await expect(codex).toContainText("codex");
    // No agent definitions can be registered over the API yet; the screen says where numbers come from.
    await expect(claude).toContainText("unregistered");
  });

  test("agent detail reads as a work log: recent work, claims, what it adopts, then access", async ({ page }) => {
    await page.goto(repoPath("agents/" + seed.agents.claude.id));
    await expect(page.getByRole("heading", { level: 1 })).toContainText("claude-code");
    await expect(page.getByRole("heading", { level: 2 }).allInnerTexts()).resolves.toEqual(["Recent work", "Working now", "Follows", "Feed trigger", "Recent adopt runs", "Access"]);
    const tokens = page.getByRole("region", { name: "Access" });
    await expect(tokens.getByText("active", { exact: true }).first()).toBeVisible();
    // TokenInfo.used: the seed's commits consumed part of the changeset budget.
    await expect(tokens).toContainText(/changesets\s*\d+\/\d+/);
    await expect(page.getByRole("region", { name: "Working now" })).toContainText("src/billing");
    const prov = page.getByRole("region", { name: "Recent work" });
    await expect(prov).toContainText("claude-opus-4-1");
    await expect(prov).toContainText("sess-claude-0193");
    await expect(prov).toContainText(seed.changes.fix.title + " (r2)");
    await expect(prov).toContainText("Add the Texas sales tax rate");
    await expect(prov).toContainText("$");
  });

  test("mints a narrowed token with a budget, shows the secret once, and can revoke it", async ({ page }) => {
    await page.goto(repoPath("agents/" + seed.agents.codex.id));
    await expect(page.getByRole("heading", { level: 1 })).toContainText("codex");
    const tokens = page.getByRole("region", { name: "Access" });
    const revoke = tokens.getByRole("button", { name: "Revoke", exact: true });
    await expect(tokens).toBeVisible();
    // The seed already gives codex active tokens: count them before minting, so the count below
    // is taken after the list shows the new token rather than racing the post-mint refresh.
    const seeded = await revoke.count();
    await page.getByText("Mint a narrowed token").click();
    const form = page.getByRole("form", { name: "Mint a narrowed token" });
    await form.getByLabel("Session label").fill("e2e-mint");
    await form.getByLabel("Write paths").fill("docs/");
    await form.getByLabel("Budget: changesets").fill("5");
    await form.getByRole("button", { name: "Copy as" }).click();
    const cli = page.getByRole("dialog").getByLabel("CLI command");
    await expect(cli).toContainText("bee token mint --for");
    await expect(cli).toContainText("--max-changesets 5");
    await page.keyboard.press("Escape");
    await form.getByRole("button", { name: "Mint token" }).click();
    await expect(page.getByTestId("minted-token")).not.toBeEmpty();
    await page.getByRole("button", { name: "Done" }).click();
    await expect(page.getByTestId("minted-token")).toHaveCount(0);

    // The new token is listed as active; a revoked token stays listed, marked revoked, so it
    // leaves the set of rows with a Revoke button.
    await expect(revoke).toHaveCount(seeded + 1);
    const before = seeded + 1;
    page.once("dialog", (d) => void d.accept());
    await revoke.last().click();
    await expect(page.getByRole("status").filter({ hasText: /Revoked/ })).toBeVisible();
    // Revoking a token revokes everything derived from it, so at least one fewer is active.
    await expect.poll(() => revoke.count()).toBeLessThan(before);
  });
});
