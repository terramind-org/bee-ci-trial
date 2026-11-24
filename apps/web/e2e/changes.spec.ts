import { expect, test } from "@playwright/test";
import * as C from "./seed/content";
import { agentToken, newChange, openChange, pressUntil, repoPath, seed, signIn } from "./helpers";

test.describe("Changes", () => {
  test("list shows risk classes, landing state and filters by text", async ({ page }) => {
    await page.goto(repoPath("changes"));
    const rows = page.locator("[data-nav-item]");
    await expect(rows.filter({ hasText: seed.changes.rotate.title })).toContainText("Sensitive");
    await expect(rows.filter({ hasText: seed.changes.docs.title })).toContainText("Docs");
    await expect(rows.filter({ hasText: seed.changes.docs.title })).toContainText("Ready to land");
    await expect(rows.filter({ hasText: seed.changes.fix.title })).toContainText("Blocked");
    await page.getByLabel("Filter changes").fill("codex");
    await expect(rows.filter({ hasText: seed.changes.fix.title })).toHaveCount(0);
    await expect(rows.filter({ hasText: seed.changes.rotate.title })).toHaveCount(1);
  });

  test("state tabs and the server-side query language", async ({ page }) => {
    await page.goto(repoPath("changes?state=landed"));
    await expect(page.locator("[data-nav-item]").filter({ hasText: seed.changes.bump.title })).toContainText("Merged");
    await page.goto(repoPath("changes"));
    await page.getByLabel("Query", { exact: true }).fill("label:docs");
    await page.getByRole("button", { name: "Query", exact: true }).click();
    await expect(page).toHaveURL(/q=label%3Adocs/);
    await expect(page.locator("[data-nav-item]")).toHaveCount(1);
    await expect(page.locator("[data-nav-item]")).toContainText(seed.changes.docs.title);
    // A qualifier the server does not accept comes back as its typed error, beside the box.
    await page.getByLabel("Query", { exact: true }).fill("is:landed");
    await page.getByRole("button", { name: "Query", exact: true }).click();
    await expect(page.getByRole("alert").filter({ hasText: "INVALID_ARGUMENT" })).toBeVisible();
  });

  test("land is blocked by policy until a human approves; the agent then lands through the queue", async ({ page, browser }) => {
    // A rate value unique to this run keeps the change a logic change even on a reused server.
    const rate = "0.06" + String(Date.now() % 100000).padStart(5, "0");
    const cr = await newChange("claude", "Add the Florida sales tax rate", { "src/billing/tax.ts": C.TAX_V2.replace('  "US-TX": 0.0625,\n', '  "US-TX": 0.0625,\n  "US-FL": ' + rate + ",\n") });
    await page.goto(repoPath("changes/" + cr.id + "/code"));
    const land = page.getByTestId("land-panel").last();
    await expect(land).toContainText("Blocked by policy");
    await expect(land).toContainText("approval:any");
    await expect(land.getByRole("button", { name: "Land via merge queue" })).toBeDisabled();

    // The human reviews in the UI, on the Code tab.
    const form = page.getByTestId("review-form");
    await form.getByText("Approve", { exact: true }).click();
    await form.getByLabel("Summary").fill("Rate matches the state's published table.");
    await form.getByRole("button", { name: "Submit review" }).click();
    await expect(page.getByRole("status").filter({ hasText: "Review submitted on r1" })).toBeVisible();
    await expect(page.getByRole("region", { name: "Reviews" })).toContainText("Rate matches");

    // The agent signs in with its own token: for its landing principal the approval counts.
    const ctx = await browser.newContext({ storageState: { cookies: [], origins: [] } });
    const agent = await ctx.newPage();
    await signIn(agent, await agentToken("claude"));
    await agent.goto(repoPath("changes/" + cr.id));
    const agentLand = agent.getByTestId("land-panel").last();
    await expect(agentLand).toContainText("Policy allows landing");
    await agentLand.getByRole("button", { name: "Land via merge queue" }).click();
    await expect(agent.getByText(/In queue|Landed on main/).filter({ visible: true }).first()).toBeVisible();
    await expect(agent.getByText("Landed on main").filter({ visible: true }).first()).toBeVisible({ timeout: 30_000 });
    await ctx.close();
    await page.reload();
    await expect(page.getByText("Landed on main").filter({ visible: true }).first()).toBeVisible();
  });

  test("a sensitive change needs two human approvals", async ({ page }) => {
    await openChange(page, seed.changes.rotate.title);
    const land = page.getByTestId("land-panel").last();
    await expect(land).toContainText("Blocked by policy");
    await expect(land).toContainText(/approval/);
    await expect(land.getByRole("button", { name: "Land via merge queue" })).toBeDisabled();
    // The Brief's conversation carries the review; the Code tab lists it with the form.
    await expect(page.getByRole("region", { name: "Conversation" })).toContainText("Rotation only on role change");
    await page.getByRole("navigation", { name: "Change request" }).getByRole("link", { name: "Code" }).click();
    await expect(page.getByRole("region", { name: "Reviews" })).toContainText("Rotation only on role change");
  });

  test("diff viewer toggles unified and split with the u key", async ({ page }) => {
    await page.goto(repoPath("changes/" + seed.changes.fix.id + "/code"));
    const layout = page.getByRole("radiogroup", { name: "Diff layout" });
    await expect(layout.getByRole("radio", { name: "unified" })).toHaveAttribute("aria-checked", "true");
    await expect(page.getByText("Math.round(net * taxFor(line.region))").first()).toBeVisible();
    // `u` attaches once the Code tab hydrates.
    await pressUntil(page, "u", () => expect(layout.getByRole("radio", { name: "split" })).toHaveAttribute("aria-checked", "true", { timeout: 1000 }));
    await expect(page.locator("th", { hasText: "New code" }).first()).toBeAttached();
    await page.locator("body").press("u");
    await expect(layout.getByRole("radio", { name: "unified" })).toHaveAttribute("aria-checked", "true");
  });

  test("revision switcher shows changes since an earlier revision and stale approvals", async ({ page }) => {
    await openChange(page, seed.changes.fix.title);
    await page.getByRole("region", { name: "Revisions" }).last().getByRole("link", { name: "r1", exact: true }).click();
    await expect(page).toHaveURL(/\/code\?since=1$/);
    await expect(page.getByRole("region", { name: "Reviews" })).toContainText(/stale|r1/);
    await expect(page.getByText("since r1").first()).toBeVisible();
    await expect(page.getByText("Tax is rounded per line").first()).toBeVisible();
    await page.goto(repoPath("changes/" + seed.changes.fix.id + "/code"));
    await pressUntil(page, "[", () => expect(page).toHaveURL(/\/code\?rev=1/, { timeout: 1000 }));
  });

  test("comments with a suggestion, resolves the thread, and applies the suggestion as a new revision", async ({ page }) => {
    const cr = await newChange("claude", "Cache invoice totals (e2e)", { "src/billing/cache.ts": C.CACHE });
    await page.goto(repoPath("changes/" + cr.id + "/code"));
    await page.getByRole("button", { name: "Comment on line 3" }).first().click();
    await page.getByRole("textbox", { name: "Comment on line 3" }).fill("Bound the cache.");
    await page.getByLabel("Suggest a replacement for this line").check();
    await page.getByLabel("Suggested replacement").fill("const totals = new Map<string, number>(); // capped at 10k entries");
    await page.getByRole("button", { name: "Comment", exact: true }).click();
    const suggestion = page.getByRole("article").filter({ hasText: "Bound the cache." });
    await expect(suggestion).toBeVisible();
    await suggestion.getByLabel("Include in batch").check();
    await page.getByRole("button", { name: /Apply 1 suggestion/ }).click();
    await expect(page.getByRole("status").filter({ hasText: "Applied 1 suggestion" })).toBeVisible();
    await expect(page.getByRole("region", { name: "Revisions" }).last().getByRole("link", { name: /^r2/ })).toBeVisible();
    await expect(page.getByText("capped at 10k entries").first()).toBeVisible();
  });

  test("replies to and resolves a review thread", async ({ page }) => {
    await page.goto(repoPath("changes/" + seed.changes.fix.id + "/code"));
    const thread = page.getByRole("article", { name: /Thread on src\/billing\/invoice.ts/ }).first();
    await expect(thread).toContainText("Can the doc comment say why we round per line?");
    await thread.getByRole("textbox").fill("Done in r2.");
    await thread.getByRole("button", { name: "Reply", exact: true }).click();
    await expect(thread).toContainText("Done in r2.");
    await thread.getByRole("button", { name: "Resolve", exact: true }).click();
    await expect(thread.getByText("resolved", { exact: true })).toBeVisible();
  });

  test("checks, provenance and path context for the viewed revision", async ({ page }) => {
    await openChange(page, seed.changes.fix.title);
    // The Brief says the checks in words; the Checks tab has every run with its output.
    await expect(page.locator("[data-human-checks]")).toBeVisible();
    await page.getByRole("navigation", { name: "Change request" }).getByRole("link", { name: /^Checks/ }).click();
    await expect(page.getByRole("region", { name: /Checks on r2/ })).toContainText("lint");
    await expect(page.getByRole("region", { name: "Workflow runs" })).toBeVisible();
    await page.goto(repoPath("changes/" + seed.changes.fix.id + "/checks?rev=1"));
    await expect(page.getByRole("region", { name: /Checks on r1/ })).toBeVisible();
    await page.goto(repoPath("changes/" + seed.changes.fix.id + "/code"));
    await expect(page.getByRole("region", { name: "Provenance" })).toContainText("claude-opus-4-1");
    await expect(page.getByRole("region", { name: "Provenance" })).toContainText("sess-claude-0193");
    await expect(page.getByRole("region", { name: "Before you touch these paths" })).toBeVisible();
  });

  // The merge-in-stack, require_linear, landed-empty and metadata-outage states need a server that
  // holds a merge changeset under a linear land rule, an allow_empty land and a downed store; the
  // seeded `--dev` server has none of them, so those branches are covered by the render tests in
  // lib/infra.test.ts. What the seed does produce is checked here against the real server.
  test("an open change lists the changesets it would land, none of them merges", async ({ page }) => {
    await openChange(page, seed.changes.fix.title);
    const stack = page.getByRole("region", { name: "Stack" }).last();
    await expect(stack.getByTestId("stack-member").first()).toBeVisible();
    await expect(stack.getByText("merge", { exact: true })).toHaveCount(0);
    await expect(page.getByTestId("linear-refusal")).toHaveCount(0);
    await expect(page.getByTestId("degraded-banner")).toHaveCount(0);
  });

  test("a landed change lists the members its land recorded in the ref log", async ({ page }) => {
    await page.goto(repoPath("changes/" + seed.changes.bump.id));
    const stack = page.getByRole("region", { name: "Stack" }).last();
    await expect(stack.getByTestId("stack-member")).toHaveCount(1);
    await expect(stack).not.toContainText("landed empty");
  });
});

test.describe("Release archives", () => {
  test("a source archive link downloads through the server action", async ({ page }) => {
    await page.goto(repoPath("releases"));
    const links = page.getByTestId("archive-links").first();
    const download = page.waitForEvent("download");
    await links.getByRole("button", { name: "tar.gz" }).click();
    expect((await download).suggestedFilename()).toMatch(/\.tar\.gz$/);
    await expect(links.getByTestId("archive-refusal")).toHaveCount(0);
  });
});
