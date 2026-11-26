import { expect, test } from "@playwright/test";
import * as C from "./seed/content";
import { agentToken, newChange, openChange, pressUntil, repoPath, seed, signIn } from "./helpers";

test.describe("Pull requests", () => {
  test("list shows the kind of change, the state in words and filters by text", async ({ page }) => {
    await page.goto(repoPath("changes"));
    const rows = page.locator("[data-nav-item]");
    await expect(rows.filter({ hasText: seed.changes.rotate.title })).toContainText("Sensitive");
    await expect(rows.filter({ hasText: seed.changes.docs.title })).toContainText("Docs");
    await expect(rows.filter({ hasText: seed.changes.docs.title })).toContainText(/Needs you|Ready to merge/);
    await expect(rows.filter({ hasText: seed.changes.fix.title }).getByTestId("status-sentence")).toBeVisible();
    await page.getByLabel("Filter pull requests").fill("codex");
    await expect(rows.filter({ hasText: seed.changes.fix.title })).toHaveCount(0);
    await expect(rows.filter({ hasText: seed.changes.rotate.title })).toHaveCount(1);
  });

  test("state tabs and the server-side query language", async ({ page }) => {
    // `merged` is the word in the interface; `landed`, the state's first name, still opens it.
    await page.goto(repoPath("changes?state=landed"));
    await expect(page.getByRole("navigation", { name: "Pull request state" }).getByRole("link", { name: /^Merged/ })).toHaveAttribute("aria-current", "page");
    await expect(page.locator("[data-nav-item]").filter({ hasText: seed.changes.bump.title })).toContainText("Merged");
    await page.goto(repoPath("changes"));
    // The server-side query is one step away, behind a disclosure.
    await page.getByText("Search with a query").click();
    await page.getByLabel("Query", { exact: true }).fill("label:docs");
    await page.getByRole("button", { name: "Search", exact: true }).click();
    await expect(page).toHaveURL(/q=label%3Adocs/);
    await expect(page.locator("[data-nav-item]")).toHaveCount(1);
    await expect(page.locator("[data-nav-item]")).toContainText(seed.changes.docs.title);
    // A qualifier the server does not accept comes back as its typed error, beside the box.
    await page.getByLabel("Query", { exact: true }).fill("is:landed");
    await page.getByRole("button", { name: "Search", exact: true }).click();
    await expect(page.getByRole("alert").filter({ hasText: "INVALID_ARGUMENT" })).toBeVisible();
  });

  test("a pull request waits for a person; Approve and merge approves it and merges it through the queue", async ({ page, browser }) => {
    // A rate value unique to this run keeps the change a logic change even on a reused server.
    const rate = "0.06" + String(Date.now() % 100000).padStart(5, "0");
    const cr = await newChange("claude", "Add the Florida sales tax rate", { "src/billing/tax.ts": C.TAX_V2.replace('  "US-TX": 0.0625,\n', '  "US-TX": 0.0625,\n  "US-FL": ' + rate + ",\n") });
    await page.goto(repoPath("changes/" + cr.id));
    // Why you: the rule in words, and one click away as the server's own token.
    await expect(page.getByTestId("why-you")).toContainText(/approval/);
    await page.getByText("Show every rule").click();
    await expect(page.getByTestId("rule-list")).toContainText("approval:any");
    // The agent's view of the same page is facts and tools: merging is marked for a person, and
    // there is nothing to press.
    const ctx = await browser.newContext({ storageState: { cookies: [], origins: [] } });
    const agent = await ctx.newPage();
    await signIn(agent, await agentToken("claude"));
    await agent.goto(repoPath("changes/" + cr.id));
    await expect(agent.locator("[data-bee-page='pull']")).toBeVisible();
    await expect(agent.locator("[data-tool='change_enqueue_change']")).toHaveAttribute("data-kind", "needs a person");
    await expect(agent.getByTestId("pull-decisions")).toHaveCount(0);
    await ctx.close();

    // The person decides: one solid button, which approves and then merges.
    const decisions = page.getByTestId("pull-decisions");
    await expect(decisions.getByRole("button", { name: "Request changes" })).toBeVisible();
    await expect(decisions.getByRole("button", { name: "Close", exact: true })).toBeVisible();
    await decisions.getByRole("button", { name: "Approve and merge" }).click();
    await expect(page.getByRole("status").filter({ hasText: /Queued to merge|Approved revision 1/ }).first()).toBeVisible();
    await expect(page.getByTestId("decision-note")).toContainText("It merged", { timeout: 30_000 });
    await expect(page.getByTestId("merge-line").last()).toContainText("merged");
  });

  test("a sensitive change needs two approvals from people, and the review form is on Files changed", async ({ page }) => {
    await openChange(page, seed.changes.rotate.title);
    await expect(page.getByTestId("why-you")).toContainText(/approvals? from a person/);
    await page.getByText("Show every rule").click();
    await expect(page.getByTestId("rule-list")).toContainText(/approval:human/);
    // The overview's conversation carries the review; Files changed lists it with the form.
    await expect(page.getByRole("region", { name: /^Conversation/ })).toContainText("Rotation only on role change");
    await page.getByRole("navigation", { name: "Pull request", exact: true }).getByRole("link", { name: "Files changed" }).click();
    await expect(page.getByRole("region", { name: "Reviews" })).toContainText("Rotation only on role change");
    await expect(page.getByTestId("review-form")).toBeVisible();
    // Beside the diff, the rail lists the rules; the decisions stay under the title.
    await expect(page.getByRole("region", { name: "Rules" }).last()).toContainText(/approval/);
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
    // The revisions are beside the diff, on Files changed.
    await page.goto(repoPath("changes/" + seed.changes.fix.id + "/code"));
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
    // The overview says the checks in one line and lists them one click away; the Checks tab
    // has every run with its output.
    await page.getByText("Show the checks").click();
    await expect(page.locator("[data-human-checks]")).toBeVisible();
    await page.getByRole("navigation", { name: "Pull request", exact: true }).getByRole("link", { name: /^Checks/ }).click();
    await expect(page.getByRole("region", { name: /Checks on r2/ })).toContainText("lint");
    await expect(page.getByRole("region", { name: "Workflow runs" })).toBeVisible();
    await page.goto(repoPath("changes/" + seed.changes.fix.id + "/checks?rev=1"));
    await expect(page.getByRole("region", { name: /Checks on r1/ })).toBeVisible();
    await page.goto(repoPath("changes/" + seed.changes.fix.id + "/code"));
    await expect(page.getByRole("region", { name: "Provenance" })).toContainText("claude-opus-4-1");
    await expect(page.getByRole("region", { name: "Provenance" })).toContainText("sess-claude-0193");
    await expect(page.getByRole("region", { name: "Before you touch these paths" })).toBeVisible();
  });

  // The merge-in-stack, require_linear, merged-empty and metadata-outage states need a server that
  // holds a merge commit under a linear rule, an allow_empty merge and a downed store; the
  // seeded `--dev` server has none of them, so those branches are covered by the render tests in
  // lib/infra.test.ts. What the seed does produce is checked here against the real server.
  test("an open pull request lists the commits it would merge, none of them a merge commit", async ({ page }) => {
    await openChange(page, seed.changes.fix.title);
    // The overview lists the commits with their messages; the header counts them.
    await expect(page.getByTestId("commits").getByRole("listitem").first()).toBeVisible();
    await expect(page.getByTestId("merge-line").last()).toContainText(/wants to merge \w+ commits? into main/);
    await page.goto(repoPath("changes/" + seed.changes.fix.id + "/code"));
    const stack = page.getByRole("region", { name: "Stack" }).last();
    await expect(stack.getByTestId("stack-member").first()).toBeVisible();
    await expect(stack.getByText("merge", { exact: true })).toHaveCount(0);
    await expect(page.getByTestId("linear-refusal")).toHaveCount(0);
    await expect(page.getByTestId("degraded-banner")).toHaveCount(0);
  });

  test("a merged pull request lists the commits its merge recorded in the ref log", async ({ page }) => {
    await page.goto(repoPath("changes/" + seed.changes.bump.id + "/code"));
    const stack = page.getByRole("region", { name: "Stack" }).last();
    await expect(stack.getByTestId("stack-member")).toHaveCount(1);
    await expect(stack).not.toContainText("merged empty");
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
