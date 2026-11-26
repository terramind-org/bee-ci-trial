import { expect, test } from "@playwright/test";
import { AgentRunState, AgentTaskKind } from "../lib/gen/bee/v1/agents_pb";
import { api, repoPath, seed } from "./helpers";
import { repoRef } from "./seed/lib";

test.describe("Agents", () => {
  test("groups work by state and keeps the principal directory reachable", async ({ page }) => {
    await page.goto(repoPath("agents"));
    await expect(page.getByRole("heading", { level: 1, name: "Agents" })).toBeVisible();
    for (const name of ["Working", "Paused", /^Finished this week · \d+$/]) {
      await expect(page.getByRole("heading", { level: 2, name })).toBeVisible();
    }
    const finished = page.locator("details").filter({ hasText: /^Finished this week ·/ });
    await expect(finished).not.toHaveAttribute("open", "");
    await page.getByText("Agent directory", { exact: true }).click();
    const directory = page.locator("details").filter({ hasText: /^Agent directory/ });
    await expect(directory.getByRole("link", { name: "claude-code", exact: true })).toHaveAttribute("href", repoPath("agents/" + seed.agents.claude.id));
    await expect(directory.getByRole("link", { name: "codex", exact: true })).toHaveAttribute("href", repoPath("agents/" + seed.agents.codex.id));
  });

  test("opens a completed standalone run from its task cell in both modes", async ({ page }) => {
    const goal = "Read the repository and identify its entry point";
    const started = await api().agent.startAgentRun({
      repo: repoRef(seed.repo), task: { kind: AgentTaskKind.FREEFORM, prompt: goal },
      baseRef: "main",
    });
    const finished = [AgentRunState.FAILED, AgentRunState.SUCCEEDED, AgentRunState.CANCELLED];
    await expect.poll(async () => finished.includes((await api().agent.getAgentRun({ id: started.id })).state)).toBe(true);
    const run = await api().agent.getAgentRun({ id: started.id });
    const taskState = run.state === AgentRunState.SUCCEEDED ? "Finished" : "Came back empty";
    await page.goto(repoPath("agents"));
    await page.getByText(/^Finished this week · /).click();
    await expect(page.getByRole("img", { name: goal + " · " + taskState })).toHaveAttribute("title", goal + " · " + taskState);
    await page.getByRole("link", { name: goal, exact: true }).click();
    await expect(page.getByRole("heading", { level: 1, name: goal })).toBeVisible();
    await expect(page.getByRole("heading", { name: "Logs and events" })).toBeVisible();
    await expect(page.getByRole("button", { name: "Retry" })).toBeVisible();
    await expect(page.getByRole("button", { name: "Pause" })).toHaveCount(0);
    await page.goto(repoPath("agents/runs/" + run.id + "?mode=bee"));
    await expect(page.getByRole("heading", { level: 1, name: "# " + goal })).toBeVisible();
    await expect(page.locator('[data-tool="agent_list_agent_run_events"]')).toBeVisible();
    await expect(page.locator('[data-tool="agent_retry_agent_run"]')).toBeVisible();
  });

  test("agent detail reads as a work log: recent work, claims, what it adopts, then access", async ({ page }) => {
    await page.goto(repoPath("agents/" + seed.agents.claude.id));
    await expect(page.getByRole("heading", { level: 1 })).toContainText("claude-code");
    await expect(page.getByRole("heading", { level: 2 }).allInnerTexts()).resolves.toEqual(["Recent work", "Working now", "Follows", "Feed trigger", "Recent adopt runs", "Access"]);
    const tokens = page.getByRole("region", { name: "Access" });
    await expect(tokens.getByText("active", { exact: true }).first()).toBeVisible();
    // TokenInfo.used: the seed's commits consumed part of the changeset budget.
    await expect(tokens).toContainText(/commits\s*\d+\/\d+/);
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
    await revoke.last().click();
    await page.getByRole("alertdialog").getByRole("button", { name: "Revoke", exact: true }).click();
    await expect(page.getByRole("status").filter({ hasText: /Revoked/ })).toBeVisible();
    // Revoking a token revokes everything derived from it, so at least one fewer is active.
    await expect.poll(() => revoke.count()).toBeLessThan(before);
  });
});
