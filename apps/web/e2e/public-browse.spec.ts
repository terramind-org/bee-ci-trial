import AxeBuilder from "@axe-core/playwright";
import { expect, test, type Page } from "@playwright/test";
import { Visibility } from "../lib/gen/bee/v1/forge_pb";
import { repoHref } from "../lib/routes";

/**
 * A signed-out visitor browses every tab of a public repo: no page asks them to sign in again or
 * shows UNAUTHENTICATED, and the member-only tabs (Agents, Security) offer to sign in instead.
 *
 * Runs against the real server (scripts/check-web-e2e.sh: the seeded repo is made public for the
 * duration of this file and private again after it) and against the fixture world
 * (`bunx playwright test -c playwright.fixture.config.ts`, which sets E2E_ADAPTER=fixture and
 * browses the public `lattice/engine`). Detail pages are found through the list pages' links, so
 * the same walk needs no seeded ids.
 */
const FIXTURE = process.env.E2E_ADAPTER === "fixture";

test.use({ storageState: { cookies: [], origins: [] } });
test.describe.configure({ mode: "serial" });

const BAD = /UNAUTHENTICATED|session has ended|Sign in to see this/;

let repo = "lattice/engine";
let restore: (() => Promise<void>) | null = null;

test.beforeAll(async () => {
  if (FIXTURE) return;
  // Imported here: the helpers read the seed manifest, which the fixture run does not have.
  const { api, seed } = await import("./helpers");
  const { connect, repoRef } = await import("./seed/lib");
  repo = seed.repo;
  const ref = repoRef(repo);
  const before = (await api().repo.getRepo({ repo: ref })).visibility;
  await api().repo.updateRepo({ repo: ref, visibility: Visibility.PUBLIC });
  restore = async () => {
    await api().repo.updateRepo({ repo: ref, visibility: before });
  };
  // Access decisions are cached briefly; wait until a caller without a token reads the repo.
  // A blank bearer is no credential: the server treats it as a signed-out caller.
  const anon = connect(api().endpoint, "");
  await expect.poll(() => anon.repo.getRepo({ repo: ref }).then(() => true, () => false), { timeout: 10_000 }).toBe(true);
});

test.afterAll(async () => {
  await restore?.();
});

async function visit(page: Page, path: string) {
  const res = await page.goto(path);
  expect(res?.status(), path).toBe(200);
  await page.waitForLoadState("load");
  await expect(page.locator("main")).toBeVisible();
  // Live streams and client fetches start after hydration; give them a moment to fail if they would.
  await page.waitForTimeout(1000);
  expect(await page.locator("body").innerText(), path).not.toMatch(BAD);
  const results = await new AxeBuilder({ page }).withTags(["wcag2a", "wcag2aa", "wcag21a", "wcag21aa", "wcag22aa"]).analyze();
  const bad = results.violations.filter((v) => v.impact === "serious" || v.impact === "critical");
  expect(bad.map((v) => v.id + ": " + v.help + " → " + v.nodes.slice(0, 3).map((n) => n.target.join(" ")).join(", ")), path + ": serious/critical axe violations").toEqual([]);
}

/** The first link in the page's main region whose path matches `shape`. */
async function firstLink(page: Page, shape: RegExp): Promise<string> {
  const hrefs = await page.locator("main a[href]").evaluateAll((as) => as.map((a) => a.getAttribute("href") ?? ""));
  const href = hrefs.find((h) => shape.test(h));
  expect(href, "a link matching " + shape).toBeTruthy();
  return href as string;
}

test("a signed-out visitor reads every tab of a public repo", async ({ page }) => {
  const at = (sub: string) => repoHref(repo, sub);
  const escaped = repo.replace(/[.*+?^${}()|[\]\\/]/g, "\\$&");

  await visit(page, at(""));
  await visit(page, at("code"));
  await visit(page, at("activity"));

  await visit(page, at("changes"));
  await visit(page, await firstLink(page, new RegExp("^/" + escaped + "/changes/[^/?]+$")));

  await visit(page, at("runs"));
  await visit(page, await firstLink(page, new RegExp("^/" + escaped + "/runs/[^/?]+$")));

  await visit(page, at("work"));
  await visit(page, await firstLink(page, new RegExp("^/" + escaped + "/work/\\d+$")));
  await visit(page, at("work/board"));

  await visit(page, at("releases"));
  await visit(page, at("packages"));
  await visit(page, at("pages"));
});

test("Agents and Security ask a signed-out visitor to sign in", async ({ page }) => {
  for (const [sub, title] of [
    ["agents", "Sign in to see agents"],
    ["security", "Sign in to see security alerts"],
  ] as const) {
    const path = repoHref(repo, sub);
    await visit(page, path);
    const main = page.locator("main");
    await expect(main.getByText(title)).toBeVisible();
    const signIn = main.getByRole("link", { name: "Sign in", exact: true });
    await expect(signIn).toHaveAttribute("href", "/signin?next=" + encodeURIComponent(path));
  }
});
