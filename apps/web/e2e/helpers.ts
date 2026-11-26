import AxeBuilder from "@axe-core/playwright";
import { expect, type Page } from "@playwright/test";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import type { ChangeRequest } from "../lib/gen/bee/v1/forge_pb";
import { orgSettingsHref, repoHref } from "../lib/routes";
import type { SeededBriefs } from "./seed/briefs";
import { commit, connect, repoRef, tip, type Api, type Commit } from "./seed/lib";

/** What e2e/seed.ts created (ids, numbers, titles). */
export interface Seed {
  org: string;
  repo: string;
  human: { id: string; display: string };
  agents: Record<"claude" | "codex", { id: string; display: string; tokenId: string; childTokenId?: string }>;
  issues: Record<"drift" | "rotate" | "retries" | "states" | "texas" | "florida", number>;
  changes: Record<"ui" | "fix" | "rotate" | "docs" | "bump" | "cache" | "closed", { id: string; number: number; title: string }>;
  /** What the seed wrote into each change request's brief and evidence (e2e/seed/briefs.ts). */
  briefs: SeededBriefs;
  runs: { failed: string; failedNumber: number; total: number };
  history: { base: string; tax: string; health: string; ci: string; blockedPush: string };
  project: { id: string; title: string };
  releases: string[];
  package: string;
  repos: string[];
}

export const seed: Seed = JSON.parse(readFileSync(new URL("./.seed.json", import.meta.url), "utf8")) as Seed;

function credentials(): { endpoint: string; token: string } {
  const dir = process.env.E2E_DATA_DIR;
  if (!dir) throw new Error("E2E_DATA_DIR is not set: run scripts/check-web-e2e.sh");
  const text = readFileSync(join(dir, "credentials"), "utf8");
  return { endpoint: /endpoint = "(.*)"/.exec(text)?.[1] ?? "", token: /token = "(.*)"/.exec(text)?.[1] ?? "" };
}

/** The dev root token, for signing in through the UI. */
export const rootToken = () => credentials().token;

let rootApi: Api | null = null;
/** The public API as the dev user, for arranging state a test needs. */
export function api(): Api {
  if (!rootApi) {
    const c = credentials();
    rootApi = connect(c.endpoint, c.token);
  }
  return rootApi;
}

/** A short-lived token for one of the seeded agents, minted by its owner for the test. */
export async function agentToken(who: "claude" | "codex"): Promise<string> {
  const repoId = (await api().repo.getRepo({ repo: repoRef(seed.repo) })).id;
  const minted = await api().auth.mintToken({
    principalId: seed.agents[who].id,
    scope: { repos: [repoId], actions: ["read", "commit", "review", "lease", "land", "issues_write"], expiresAt: { seconds: BigInt(Math.floor(Date.now() / 1000) + 3600), nanos: 0 }, session: "e2e-" + who },
  });
  return minted.token;
}

/** The public API as one of the seeded agents. */
export async function agentApi(who: "claude" | "codex"): Promise<Api> {
  return connect(credentials().endpoint, await agentToken(who));
}

/** A page of the seeded repo: `repoPath("changes/" + id)` → `/dev/playground/changes/…`. */
export const repoPath = (sub = "") => repoHref(seed.repo, sub);

/** A section of the seeded org's settings: `orgPath("audit")` → `/orgs/dev/settings/audit`. */
export const orgPath = (sub = "") => orgSettingsHref(seed.org, sub);

/** Signs a page in with a token through the sign-in screen (the UI then acts as that principal). */
export async function signIn(page: Page, token: string) {
  await page.goto("/signin?next=" + encodeURIComponent(repoPath("activity")));
  await page.getByRole("textbox", { name: "Token" }).fill(token);
  await page.getByRole("button", { name: "Sign in with token" }).click();
  await expect(page).toHaveURL(new RegExp(repoPath("activity") + "$"));
}

/** Presses a key until `check` passes; single-key shortcuts attach once the page hydrates. */
export async function pressUntil(page: Page, key: string, check: () => Promise<void>) {
  await expect(async () => {
    await page.locator("body").press(key);
    await check();
  }).toPass({ timeout: 15_000 });
}

const AUTHOR = { claude: "claude-code <claude-code@agents.bee.local>", codex: "codex <codex@agents.bee.local>" };

/** Opens a fresh change request on main as an agent, so a test owns what it changes. */
export async function newChange(who: "claude" | "codex", title: string, files: Record<string, string>, body = ""): Promise<ChangeRequest> {
  const a = await agentApi(who);
  const label = title + ":" + Date.now() + ":" + Math.random();
  const c: Commit = { author: AUTHOR[who], message: title, files, provenance: { runtime: who === "claude" ? "claude-code" : "codex", model: who === "claude" ? "claude-opus-4-1" : "gpt-5-codex", session: "e2e-" + who } };
  const head = await commit(a, seed.repo, await tip(api(), seed.repo), c, label);
  return a.change.openChange({ repo: repoRef(seed.repo), targetRef: "main", head, title, body });
}

/** Opens a change request from the list by its title. */
export async function openChange(page: Page, title: string) {
  await page.goto(repoPath("changes?state=all"));
  await page.locator("[data-nav-item]").filter({ hasText: title }).first().click();
  await expect(page.getByRole("heading", { level: 1, name: title })).toBeVisible();
}

/** Zero serious or critical axe violations (WCAG 2.2 AA rules). */
export async function expectAccessible(page: Page) {
  const results = await new AxeBuilder({ page }).withTags(["wcag2a", "wcag2aa", "wcag21a", "wcag21aa", "wcag22aa"]).analyze();
  const bad = results.violations.filter((v) => v.impact === "serious" || v.impact === "critical");
  expect(bad.map((v) => v.id + ": " + v.help + " → " + v.nodes.slice(0, 3).map((n) => n.target.join(" ")).join(", ")), "serious/critical axe violations").toEqual([]);
  return results;
}

/** Opens the Copy-as panel from a trigger and returns the CLI text. */
export async function copyAsCli(page: Page, trigger: ReturnType<Page["locator"]>) {
  await trigger.click();
  const dialog = page.getByRole("dialog");
  await expect(dialog).toBeVisible();
  const cli = await dialog.getByLabel("CLI command").innerText();
  return { dialog, cli };
}
