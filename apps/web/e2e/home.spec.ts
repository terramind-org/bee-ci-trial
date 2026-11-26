import { expect, test } from "@playwright/test";
import { expectAccessible, seed } from "./helpers";

test.describe("Home: signed in", () => {
  test("is the Inbox: needs you, in progress, done today and this week, and passes axe", async ({ page }) => {
    await page.goto("/");
    await expect(page).toHaveTitle(/^Inbox/);
    const inbox = page.getByTestId("inbox");
    await expect(inbox.getByRole("heading", { level: 1, name: "Inbox" })).toBeVisible();
    for (const name of ["Needs you", "In progress", "Done today", "This week"]) await expect(inbox.getByRole("heading", { level: 2, name })).toBeVisible();
    // The lede says, in words, how much needs you.
    await expect(inbox.getByText(/^(Nothing needs you\.|\w+ things? needs? you\.)/)).toBeVisible();
    await expect(inbox.getByTestId("this-week")).toContainText(/change requests? open/);
    await expectAccessible(page);
  });

  test("every item that needs you is one sentence with one action, and the rail counts the same items", async ({ page }) => {
    await page.goto("/");
    const section = page.getByRole("region", { name: "Needs you" });
    const list = section.getByTestId("needs-you");
    await expect(list.or(section.getByText("You are all caught up."))).toBeVisible();
    const items = await list.getByRole("listitem").count();

    const rail = page.getByRole("complementary", { name: "Beehive" }).getByRole("navigation", { name: "Main" });
    const needsYou = rail.getByRole("link", { name: /^Needs you/ });
    await expect(needsYou).toHaveAccessibleName(items === 0 ? /^Needs you$/ : new RegExp(`^Needs you\\s*, ${items} waiting$`));

    for (const item of await list.getByRole("listitem").all()) {
      // Each row has one action for its safety record, pull request or post.
      const actions = item.getByRole("link", { name: /^(Review safety|Review|Open|Merge|Read post)\s*:/ });
      await expect(actions).toHaveCount(1);
      const kind = await item.getAttribute("data-kind");
      if (kind === "tripwire" || kind === "concern") {
        await expect(actions).toHaveAttribute("href", /\/safety\?work=[^&]+&kind=SAFETY_WORK_KIND_(RUN|SWARM)$/);
      } else {
        await expect(actions).toHaveAttribute("href", /\/changes\/[^/]+(#post)?$/);
      }
    }
    if (!items) return;
    // The first item is the next thing to do.
    const first = list.getByRole("listitem").first().getByRole("link", { name: /^(Review safety|Review|Open|Merge|Read post)\s*:/ });
    const href = await first.getAttribute("href");
    await first.click();
    await expect(page).toHaveURL(new RegExp((href ?? "").replace(/#.*$/, "").replace(/[.*+?^${}()|[\]\\]/g, "\\$&")));
  });

  test("merged work shows under Done today, one line per repository", async ({ page }) => {
    await page.goto("/");
    const done = page.getByRole("region", { name: "Done today" });
    const groups = done.getByTestId("done-today");
    await expect(groups.or(done.getByText("Nothing has merged in your repositories in the last day."))).toBeVisible();
    if (!(await groups.isVisible())) return;
    const line = groups.locator("details").first();
    await expect(line.locator("summary")).toContainText(/\d+ merged/);
    await line.locator("summary").click();
    await expect(line.getByRole("link", { name: /^All activity in / })).toBeVisible();
  });
});

test.describe("Home: signed out", { tag: "@enterprise" }, () => {
  test.use({ storageState: { cookies: [], origins: [] } });

  test("shows the instance landing with public activity only, and Sign in navigates", async ({ page }) => {
    await page.goto("/");
    const landing = page.getByTestId("instance-landing");
    await expect(landing).toBeVisible();
    await expect(landing.getByRole("heading", { level: 1 })).toBeVisible();
    // Nothing from the private repos of the seeded world is listed.
    const activity = landing.getByTestId("public-activity");
    await expect(activity.or(landing.getByText("Nothing has landed in a public repository yet."))).toBeVisible();
    if (await activity.isVisible()) {
      await expect(activity).not.toContainText(seed.repo);
      await expect(activity).not.toContainText("internal-roadmap");
    }
    await expectAccessible(page);
    await landing.getByRole("link", { name: "Sign in" }).click();
    await expect(page).toHaveURL(/\/signin/);
  });
});
