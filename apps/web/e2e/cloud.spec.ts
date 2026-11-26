import { expect, test } from "@playwright/test";
import { expectAccessible, orgPath, seed } from "./helpers";
import { PUBLIC_REPOS } from "./seed/discovery";

/**
 * The edition surfaces. scripts/check-web-e2e.sh runs the suite once per edition against a
 * bee-server configured with that `[server] edition`; playwright.config.ts selects the
 * `@cloud` or `@enterprise` tests for the run.
 */
const signedOut = { cookies: [], origins: [] };

test.describe("Cloud: landing", { tag: "@cloud" }, () => {
  test.use({ storageState: signedOut });

  test("renders the hero, real counts, live activity and the three-way example, and passes axe", async ({ page }) => {
    await page.goto("/");
    const landing = page.getByTestId("cloud-landing");
    await expect(landing).toBeVisible();
    await expect(landing.getByRole("heading", { level: 1, name: "Source control built for agents" })).toBeVisible();
    await expect(landing.getByRole("link", { name: "Get started" })).toHaveAttribute("href", "/signin?next=/new");
    await expect(landing.getByRole("link", { name: "Self-host Beehive" })).toHaveAttribute("href", "#self-host");

    // Counts come from the server: at least the public repos of the seeded discovery org.
    const counts = landing.getByTestId("landing-counts");
    await expect(counts).toBeVisible();
    const repos = Number(/(\d[\d,.]*\w*)\s+public repositor/.exec((await counts.innerText()).replace(/\n/g, " "))?.[1]?.replace(/\D/g, ""));
    expect(repos).toBeGreaterThanOrEqual(PUBLIC_REPOS.length);

    const live = landing.getByRole("region", { name: "Live right now" }).or(landing.getByText("Live right now").locator("xpath=ancestor::section[1]"));
    await expect(live.first()).toBeVisible();
    const rows = landing.getByTestId("public-land");
    expect(await rows.count()).toBeLessThanOrEqual(8);
    await expect(landing.getByTestId("public-activity").or(landing.getByText("Nothing has landed in a public repository yet."))).toBeVisible();
    // Nothing private shows.
    await expect(landing).not.toContainText(seed.repo);

    await expect(landing.getByLabel("CLI example", { exact: true })).toContainText("bee cr list --repo ");
    await expect(landing.getByLabel("MCP example", { exact: true })).toContainText("change_list_changes");
    await expect(landing.getByLabel("HTTP example", { exact: true })).toContainText("ChangeService/ListChanges");
    await expect(landing.getByRole("heading", { name: "Self-host for your company" })).toBeVisible();
    await expect(landing.getByRole("link", { name: "Pricing" })).toHaveAttribute("href", "/pricing");
    await expect(landing.getByRole("link", { name: "llms.txt" })).toHaveAttribute("href", "/llms.txt");
    await expectAccessible(page);
  });

  test("the sign-in page offers account creation and Get started leads to it", async ({ page }) => {
    await page.goto("/");
    await page.getByTestId("cloud-landing").getByRole("link", { name: "Get started" }).click();
    await expect(page).toHaveURL(/\/signin\?next=\/new$|\/signin\?next=%2Fnew$/);
    await expect(page.getByRole("heading", { level: 1, name: "Create your account, or sign in" })).toBeVisible();
    await expect(page.getByRole("button", { name: "Continue with GitHub" })).toBeVisible();
    await expectAccessible(page);
  });

  test("/new without a session goes to sign-in", async ({ page }) => {
    await page.goto("/new");
    await expect(page).toHaveURL(/\/signin\?next=(%2F|\/)new$/);
  });

  test("pricing asks a signed-out visitor to sign in for plans, and still shows self-hosting and questions", async ({ page }) => {
    await page.goto("/pricing");
    await expect(page.getByRole("heading", { level: 1, name: "Pricing" })).toBeVisible();
    await expect(page.getByTestId("pricing-sign-in")).toBeVisible();
    await expect(page.getByTestId("self-hosted-card")).toBeVisible();
    await expect(page.getByTestId("pricing-faq").getByRole("term")).toHaveCount(5);
    await expect(page.getByText("Education and nonprofit plans are assigned by an administrator.")).toBeVisible();
    await expectAccessible(page);
  });
});

test.describe("Cloud: pricing", { tag: "@cloud" }, () => {
  test("shows the plans from the server, or says billing is not available, and passes axe", async ({ page }) => {
    await page.goto("/pricing");
    await expect(page.getByRole("heading", { level: 1, name: "Pricing" })).toBeVisible();
    const plans = page.getByTestId("plan-card");
    const unavailable = page.getByTestId("pricing-unavailable");
    await expect(plans.first().or(unavailable)).toBeVisible();
    if (await plans.first().isVisible()) {
      expect(await plans.count()).toBeGreaterThanOrEqual(1);
      // A plan sold by contact never has a buy button.
      for (const card of await plans.all()) {
        const contact = card.getByRole("link", { name: "Contact sales" });
        if (await contact.count()) await expect(card.getByRole("link", { name: /^Start with/ })).toHaveCount(0);
      }
    }
    await expect(page.getByTestId("self-hosted-card")).toBeVisible();
    if (await plans.first().isVisible()) {
      await expect(page.getByTestId("pricing-faq").getByRole("term")).toHaveCount(5);
      await expect(page.getByText("Education and nonprofit plans are assigned by an administrator.")).toBeVisible();
    }
    await expectAccessible(page);
  });
});

test.describe("Cloud: onboarding", { tag: "@cloud" }, () => {
  test("/new creates an organization and a repository and ends at the repository", async ({ page }) => {
    const org = "cloud" + Date.now().toString(36);
    await page.goto("/new");
    await expect(page.getByRole("heading", { level: 1, name: "New repository" })).toBeVisible();
    await expect(page.getByRole("listitem").filter({ hasText: "Owner" }).first()).toHaveAttribute("aria-current", "step");

    await page.getByRole("radio", { name: /A new organization/ }).check();
    await page.getByLabel("Organization name").fill(org);
    await page.getByRole("button", { name: "Copy as" }).click();
    await expect(page.getByRole("dialog").getByLabel("CLI command")).toContainText(org);
    await page.keyboard.press("Escape");
    await expectAccessible(page);
    await page.getByRole("button", { name: "Create and continue" }).click();

    await expect(page.getByRole("heading", { name: /Name the repository/ })).toBeVisible();
    await page.getByLabel("Repository name").fill("hello");
    await page.getByLabel("Description").fill("Created from onboarding");
    await page.getByLabel("Visibility").selectOption("VISIBILITY_PUBLIC");
    await expect(page.getByRole("link", { name: "Import instead" })).toHaveAttribute("href", "/import");
    await page.getByRole("button", { name: "Create repository" }).click();

    await expect(page.getByRole("heading", { name: new RegExp(org + "/hello is ready") })).toBeVisible();
    await expect(page.getByRole("heading", { name: "Set up Bee for a repository" })).toBeVisible();
    await expectAccessible(page);
    await page.getByRole("link", { name: "Go to repository" }).click();
    await expect(page).toHaveURL(new RegExp("/" + org + "/hello$"));
    // The repository exists and is readable: its activity page answers.
    await page.goto("/" + org + "/hello/activity");
    await expect(page.getByRole("heading", { level: 1, name: "Activity" })).toBeVisible();
  });

  test("a name the server refuses is shown with its reason", async ({ page }) => {
    await page.goto("/new");
    await page.getByRole("radio", { name: /A new organization/ }).check();
    await page.getByLabel("Organization name").fill("Not A Valid Name!");
    await page.getByRole("button", { name: "Create and continue" }).click();
    await expect(page.getByRole("alert").filter({ hasText: /[A-Z_]{4,}:/ })).toBeVisible();
  });
});

test.describe("Cloud: billing", { tag: "@cloud" }, () => {
  test("renders the plan, seats and usage, or says billing is not available, and passes axe", async ({ page }) => {
    await page.goto(orgPath("billing"));
    await expect(page.getByRole("heading", { level: 1, name: "Billing" })).toBeVisible();
    await expect(page.getByRole("link", { name: "Billing" })).toBeVisible();
    const plan = page.getByTestId("current-plan");
    const unavailable = page.getByTestId("billing-unavailable");
    await expect(plan.or(unavailable)).toBeVisible();
    if (await plan.isVisible()) {
      // An account with billing off has nothing to change.
      const review = page.getByRole("button", { name: "Review change" });
      await expect(review.or(page.getByText("Billing is off for this account"))).toBeVisible();
      if (await review.isVisible() && await review.isEnabled()) {
        await expect(review).toHaveAttribute("data-agent-confirm", /Changes the billing plan/);
        const before = await plan.innerText();
        await review.click();
        const confirmation = page.getByRole("alertdialog", { name: /Change the plan/ });
        await expect(confirmation.getByRole("button", { name: "Confirm change" })).toBeVisible();
        await confirmation.getByRole("button", { name: "Cancel" }).click();
        await expect(confirmation).toHaveCount(0);
        await expect(plan).toHaveText(before);
      }
      // Copy as lives in the page's "…" menu.
      await page.getByRole("button", { name: "More actions" }).click();
      await expect(page.getByRole("menuitem", { name: /Copy as/ })).toBeVisible();
      await page.keyboard.press("Escape");
    }
    await expectAccessible(page);
    const wasAvailable = await plan.isVisible();
    await page.goto(orgPath("billing") + "?mode=bee");
    const facts = page.locator('[data-bee-page="org-billing"]');
    await expect(facts.getByRole("heading", { name: /tools on this page/i })).toBeVisible();
    await expect(facts.getByRole("row", { name: /billing_get_account_billing_plan/ })).toBeVisible();
    await expect(facts).toContainText(wasAvailable ? "Current plan" : "Billing is not available on this server", { ignoreCase: true });
  });
});

test.describe("Enterprise edition", { tag: "@enterprise" }, () => {
  test.use({ storageState: signedOut });

  test("has no pricing page and shows the instance landing at /", async ({ page }) => {
    // The page streams, so the status line is already 200 when notFound() renders: check the page.
    await page.goto("/pricing");
    await expect(page.getByRole("heading", { level: 1, name: "Nothing here" })).toBeVisible();
    await expect(page.getByRole("heading", { name: "Pricing" })).toHaveCount(0);
    await page.goto("/");
    await expect(page.getByTestId("instance-landing")).toBeVisible();
    await expect(page.getByTestId("cloud-landing")).toHaveCount(0);
    await expect(page.getByRole("link", { name: "Pricing" })).toHaveCount(0);
    await page.goto("/signin");
    await expect(page.getByRole("heading", { level: 1, name: /^Sign in to / })).toBeVisible();
    await expect(page.getByText(/create your account/i)).toHaveCount(0);
    await expectAccessible(page);
  });
});
