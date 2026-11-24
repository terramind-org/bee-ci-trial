import { expect, test } from "@playwright/test";
import { expectAccessible, pressUntil } from "./helpers";
import { BUSIEST_TOPIC, EXPECTED, LOGINS, PRIVATE_REPO, PUBLIC_REPOS, STARS } from "./seed/discovery";

const AGENTKIT = "lattice/agentkit";
const BENCHKIT = "lattice/benchkit";

test.describe("Repositories", () => {
  test("filters write the URL and narrow the results", async ({ page }) => {
    await page.goto("/repositories?tab=public");
    await expect(page.getByRole("heading", { level: 1, name: "Repositories" })).toBeVisible();
    for (const r of PUBLIC_REPOS) await expect(page.getByRole("link", { name: r, exact: true })).toBeVisible();
    await expect(page.getByRole("link", { name: PRIVATE_REPO })).toHaveCount(0);

    // Text: waits for a pause, then the URL and the list follow.
    await page.getByRole("searchbox", { name: "Find a repository" }).fill("agentkit");
    await expect(page).toHaveURL(/q=agentkit/);
    await expect(page.getByRole("link", { name: AGENTKIT, exact: true })).toBeVisible();
    await expect(page.getByRole("link", { name: "lattice/tl", exact: true })).toHaveCount(0);

    await page.getByRole("searchbox", { name: "Find a repository" }).fill("");
    await expect(page).not.toHaveURL(/q=/);
    // A language nobody uses: the URL follows and the list says so.
    await page.getByLabel("Language").selectOption("Kotlin");
    await expect(page).toHaveURL(/language=Kotlin/);
    await expect(page.getByText("No repositories match these filters")).toBeVisible();
    await expect(page.getByRole("link", { name: AGENTKIT, exact: true })).toHaveCount(0);
    await page.getByRole("link", { name: "Clear filters" }).click();
    await expect(page).not.toHaveURL(/language=/);

    await page.getByLabel("Topic", { exact: true }).fill(BUSIEST_TOPIC);
    await expect(page).toHaveURL(new RegExp("topic=" + BUSIEST_TOPIC));
    await expect(page.getByRole("link", { name: BENCHKIT, exact: true })).toBeVisible();
    await expect(page.getByRole("link", { name: "lattice/mergekit", exact: true })).toHaveCount(0);
  });

  test("sorts by stars and by name", async ({ page }) => {
    // Other specs create public repos on the same server (cloud onboarding), so check the order
    // of the seeded repos among whatever else is listed.
    const seeded = async () => (await page.locator("[data-nav-item]").allInnerTexts()).filter((n) => (PUBLIC_REPOS as readonly string[]).includes(n));
    await page.goto("/repositories?tab=public&sort=stars");
    await expect(page.locator("[data-nav-item]").first()).toBeVisible();
    // The seeded counts differ from the name order (tl has more stars than benchkit).
    const byStars = [...PUBLIC_REPOS].sort((a, b) => STARS[b]!.length - STARS[a]!.length);
    await expect.poll(seeded).toEqual(byStars);
    expect(byStars[0]).toBe(AGENTKIT);
    await page.goto("/repositories?tab=public&sort=name");
    await expect(page.locator("[data-nav-item]").first()).toBeVisible();
    await expect.poll(seeded).toEqual([...PUBLIC_REPOS].sort());
    expect((await seeded()).at(-1)).toBe("lattice/tl");
  });

  test("signed in: tabs, New repository, / focuses the filter and j moves through the list", async ({ page }) => {
    await page.goto("/repositories");
    const tabs = page.getByRole("navigation", { name: "Repository lists" });
    await expect(tabs.getByRole("link")).toHaveText(["Yours", "Your orgs", "Starred", "Public"]);
    await expect(page.getByRole("link", { name: "New repository" })).toHaveAttribute("href", "/new");
    await tabs.getByRole("link", { name: "Starred" }).click();
    await expect(page).toHaveURL(/tab=starred/);
    await page.goto("/repositories?tab=public");
    await pressUntil(page, "/", async () => {
      await expect(page.getByRole("searchbox", { name: "Find a repository" })).toBeFocused({ timeout: 500 });
    });
    await page.locator("body").click({ position: { x: 5, y: 300 } });
    await page.locator("body").press("j");
    await expect(page.locator("[data-nav-item]").first()).toBeFocused();
  });

  test("axe passes", async ({ page }) => {
    await page.goto("/repositories?tab=public");
    await expect(page.getByRole("link", { name: AGENTKIT, exact: true })).toBeVisible();
    await expectAccessible(page);
  });
});

test.describe("Repo home", () => {
  test("shows the README, the files, the latest land and the clone panel; copy works", async ({ page, context }) => {
    await context.grantPermissions(["clipboard-read", "clipboard-write"]);
    await page.goto("/" + AGENTKIT);
    await expect(page.getByRole("heading", { level: 1, name: "agentkit" })).toBeVisible();
    await expect(page.getByText("Building blocks for coding agents that work on a shared branch.")).toBeVisible();
    await expect(page.getByRole("list", { name: "Files in the repository root" }).getByRole("link", { name: "Cargo.toml" })).toHaveAttribute("href", /\/lattice\/agentkit\/code\?path=Cargo\.toml/);
    // The info band holds the latest merge; the file list has no box around it.
    await expect(page.getByTestId("repo-band")).toContainText("merged");
    await expect(page.getByRole("list", { name: "Files in the repository root" })).not.toHaveClass(/border/);
    // The old boxed panels are gone; their facts live in one side note.
    for (const old of ["About", "Working now", "Recent changes"]) await expect(page.getByRole("heading", { name: old, exact: true })).toHaveCount(0);
    const note = page.getByRole("complementary", { name: "About this repository" });
    await expect(note.getByRole("link", { name: "agents", exact: true })).toHaveAttribute("href", "/topics/agents");
    await expect(note.getByRole("img", { name: /^Languages: Rust/ })).toBeVisible();
    await expect(note.getByRole("heading", { name: "Editing now" })).toBeVisible();
    await expect(page.getByRole("region", { name: /README/ })).toBeVisible();

    // Clone lives in the header's one menu.
    await page.getByRole("button", { name: "Clone" }).click();
    await expect(page.getByLabel("Bee CLI command", { exact: true })).toHaveText("bee git setup --repo " + AGENTKIT);
    await expect(async () => {
      await page.getByRole("button", { name: "Copy the Bee CLI command" }).click();
      expect(await page.evaluate(() => navigator.clipboard.readText())).toBe("bee git setup --repo " + AGENTKIT);
    }).toPass({ timeout: 15_000 });
  });

  test("a source archive request shows a result: a download or the server's refusal", async ({ page }) => {
    await page.goto("/" + AGENTKIT);
    const button = page.getByRole("button", { name: "Download source as zip" });
    await expect(async () => {
      await button.click();
      await expect(page.getByRole("alert").or(page.getByRole("button", { name: "Download source as zip" }).and(page.locator(":not([disabled])")))).toBeVisible({ timeout: 1000 });
    }).toPass({ timeout: 15_000 });
  });

  test("starring toggles for a signed-in visitor", async ({ page }) => {
    // A click before React attaches its handlers does nothing, and one retried after it did work
    // toggles twice. So wait for the button to be hydrated, click once, and wait for the state.
    const hydrated = (name: RegExp) =>
      page.waitForFunction((src) => {
        const b = [...document.querySelectorAll("button")].find((x) => new RegExp(src).test(x.getAttribute("aria-label") ?? x.textContent ?? ""));
        return !!b && Object.keys(b).some((k) => k.startsWith("__reactProps"));
      }, name.source);
    await page.goto("/" + BENCHKIT);
    const star = page.getByRole("button", { name: /^(Star|Starred) lattice\/benchkit/ });
    await expect(star).toHaveAttribute("aria-pressed", "false");
    await hydrated(/^(Star|Starred) lattice\/benchkit/);
    await star.click();
    await expect(star).toHaveAttribute("aria-pressed", "true");
    await page.reload();
    const starred = page.getByRole("button", { name: /^Starred lattice\/benchkit/ });
    await expect(starred).toHaveAttribute("aria-pressed", "true");
    // Leave the world as it was found.
    await hydrated(/^Starred lattice\/benchkit/);
    await starred.click();
    await expect(page.getByRole("button", { name: /^Star lattice\/benchkit/ })).toHaveAttribute("aria-pressed", "false");
  });

  test("axe passes", async ({ page }) => {
    await page.goto("/" + AGENTKIT);
    await expect(page.getByRole("heading", { level: 1, name: "agentkit" })).toBeVisible();
    await expectAccessible(page);
  });

  test.describe("signed out", () => {
    test.use({ storageState: { cookies: [], origins: [] } });

    test("a public repo reads fine and the star button asks to sign in", async ({ page }) => {
      const res = await page.goto("/" + AGENTKIT);
      expect(res?.status()).toBe(200);
      await expect(page.getByRole("heading", { level: 1, name: "agentkit" })).toBeVisible();
      await expect(page.getByRole("link", { name: /^Star lattice\/agentkit/ })).toHaveAttribute("href", /\/signin\?next=/);
      await expectAccessible(page);
    });

    test("a private repo is a plain 404", async ({ page }) => {
      const res = await page.goto("/" + PRIVATE_REPO);
      expect(res?.status()).toBe(404);
      await expect(page.getByRole("heading", { level: 1, name: "Nothing here" })).toBeVisible();
    });

    test("/repositories lists public repos only, without tabs", async ({ page }) => {
      await page.goto("/repositories");
      await expect(page.getByRole("navigation", { name: "Repository lists" })).toHaveCount(0);
      await expect(page.getByRole("link", { name: AGENTKIT, exact: true })).toBeVisible();
      await expect(page.getByRole("link", { name: "New repository" })).toHaveCount(0);
    });
  });
});

test.describe("Profiles", () => {
  test("a person: identity, counts, popular repositories and the contribution graph", async ({ page }) => {
    await page.goto("/" + LOGINS.human);
    await expect(page.getByRole("heading", { level: 1 })).toBeVisible();
    await expect(page.getByText(String(EXPECTED.devFollowers) + " followers")).toBeVisible();
    await expect(page.getByText(String(EXPECTED.devFollowing) + " following")).toBeVisible();
    await expect(page.getByRole("heading", { name: "Contributions" })).toBeVisible();
    await expectAccessible(page);
  });

  test("tabs follow ?tab= and lists link to profiles", async ({ page }) => {
    await page.goto("/" + LOGINS.human + "?tab=followers");
    const nav = page.getByRole("navigation", { name: "Profile sections" });
    await expect(nav.getByRole("link", { name: /^Followers/ })).toHaveAttribute("aria-current", "page");
    await expect(page.getByRole("link", { name: new RegExp(LOGINS.reviewer) }).first()).toHaveAttribute("href", "/" + LOGINS.reviewer);
    await page.goto("/" + LOGINS.human + "?tab=stars");
    await expect(page.getByRole("link", { name: AGENTKIT, exact: true })).toBeVisible();
  });

  test("a profile reads like a person page: a large mark beside the name, one column", async ({ page }) => {
    await page.goto("/" + LOGINS.human);
    const heading = page.getByRole("heading", { level: 1 });
    const mark = await page.getByRole("region", { name: "Profile" }).getByRole("img").first().boundingBox();
    expect(mark?.width ?? 0).toBeGreaterThanOrEqual(96);
    const box = await heading.boundingBox();
    expect(box?.x ?? 0).toBeGreaterThan((mark?.x ?? 0) + (mark?.width ?? 0) - 1);
    // No sidebar column: the tabs and the content start at the same left edge as the mark.
    const tabs = await page.getByRole("navigation", { name: "Profile sections" }).boundingBox();
    expect(Math.abs((tabs?.x ?? 0) - (mark?.x ?? 0))).toBeLessThan(2);
  });

  test("an agent shows as an agent", async ({ page }) => {
    await page.goto("/" + LOGINS.claude);
    await expect(page.getByRole("heading", { level: 1 })).toContainText(LOGINS.claude);
    await expect(page.getByText("agent", { exact: true }).first()).toBeVisible();
    await expectAccessible(page);
  });

  test("an org lists its public repos, people and topics", async ({ page }) => {
    await page.goto("/lattice");
    await expect(page.getByRole("heading", { level: 1 })).toContainText(/lattice/i);
    for (const r of PUBLIC_REPOS) await expect(page.getByRole("link", { name: r, exact: true })).toBeVisible();
    await expect(page.getByRole("region", { name: "Organization" }).getByRole("img").first()).toBeVisible();
    await expect(page.getByRole("heading", { name: "People" })).toBeVisible();
    await expect(page.getByRole("complementary", { name: "Organization details" }).getByRole("link", { name: /^rust/ })).toHaveAttribute("href", "/topics/rust");
    await expectAccessible(page);
  });

  test("unknown owners and reserved names are 404", async ({ page }) => {
    await page.goto("/no-such-owner-here");
    await expect(page.getByRole("heading", { level: 1, name: "Nothing here" })).toBeVisible();
  });

  test.describe("signed out", () => {
    test.use({ storageState: { cookies: [], origins: [] } });

    test("profiles are readable and Follow asks to sign in", async ({ page }) => {
      await page.goto("/" + LOGINS.reviewer);
      await expect(page.getByRole("link", { name: /^Follow reviewer/ })).toHaveAttribute("href", /\/signin\?next=/);
      await expectAccessible(page);
    });
  });
});
