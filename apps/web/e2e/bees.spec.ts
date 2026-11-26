import { expect, test, type Locator } from "@playwright/test";

const moving = ["loading", "capping", "claiming", "landing", "refusal", "pause", "arrival"];

function sampleAnimations(sample: Locator) {
  return sample.evaluate((row) => row.querySelector("[data-preview]")!.getAnimations({ subtree: true }).length);
}

test.describe("event motion review", () => {
  test.skip(process.env.E2E_ADAPTER !== "fixture" && process.env.BEE_ENABLE_MOTION_REVIEW !== "1",
    "The review route is enabled only in development or by an explicit flag.");

  test("each event plays once and returns to its still", async ({ page }) => {
    await page.goto("/dev/bees?mode=keeper");
    await expect(page.getByRole("heading", { name: "Event motion" })).toBeVisible();
    await page.evaluate(() => {
      document.addEventListener("animationstart", (event) => {
        const row = (event.target as Element).closest<HTMLElement>("[data-motion-sample]");
        if (row && (event.target as Element).closest("[data-preview]")) {
          row.dataset.starts = String(Number(row.dataset.starts ?? 0) + 1);
        }
      });
    });

    for (const name of [...moving, "quiet"]) {
      const sample = page.locator(`[data-motion-sample="${name}"]`);
      await expect(sample.locator(`[data-still] [data-motion="${name}"]`)).toBeVisible();
      await expect(sample.locator(`[data-preview] [data-motion="${name}"]`)).toBeVisible();
      await sample.getByRole("button", { name: /Play/ }).click();
      if (name !== "quiet") {
        await expect(sample).toHaveAttribute("data-starts", "1");
        await expect(sample.locator("[data-play]")).toHaveCount(0);
        await expect(sample).toHaveAttribute("data-starts", "1");
      } else {
        await expect(sample).not.toHaveAttribute("data-starts");
      }
      await expect.poll(() => sampleAnimations(sample)).toBe(0);
    }
  });

  for (const mode of ["reduced motion", "agent view"]) {
    test(`every picture is still in ${mode}`, async ({ page }) => {
      if (mode === "reduced motion") await page.emulateMedia({ reducedMotion: "reduce" });
      await page.goto(`/dev/bees?mode=${mode === "agent view" ? "bee" : "keeper"}`);
      if (mode === "agent view") await expect(page.locator("html")).toHaveAttribute("data-mode", "bee");
      for (const name of [...moving, "quiet"]) {
        const sample = page.locator(`[data-motion-sample="${name}"]`);
        await expect(sample.locator(`[data-preview] [data-motion="${name}"]`)).toBeVisible();
        await sample.getByRole("button", { name: /Play/ }).click();
        expect(await sampleAnimations(sample)).toBe(0);
        await expect(sample.locator("[data-play]")).toHaveCount(0);
      }
      await expect(page.locator('[data-motion-sample="loading"] [data-preview]')).toHaveText("Loading");
    });
  }
});

test("a confirmed pause plays through a real fixture refresh", async ({ page }) => {
  test.skip(process.env.E2E_ADAPTER !== "fixture", "The fixture provides a working swarm.");
  const session = await page.request.post("/api/session", { data: { token: "demo-token-1" } });
  expect(session.ok()).toBe(true);
  await page.addInitScript(() => {
    document.addEventListener("animationstart", (event) => {
      if (!(event.target as Element).closest('[data-motion="pause"]')) return;
      const root = document.documentElement;
      root.dataset.pauseStarts = String(Number(root.dataset.pauseStarts ?? 0) + 1);
    });
  });
  await page.goto("/acme/api/agents?mode=keeper");
  await page.getByRole("link", { name: /^When we demote an admin/ }).click();
  await page.getByRole("button", { name: "Pause", exact: true }).click();
  await expect(page.getByRole("button", { name: "Resume", exact: true })).toBeVisible();
  await expect(page.locator("html")).toHaveAttribute("data-pause-starts", "1");
  await expect(page.locator('[data-motion="pause"][data-play]')).toHaveCount(0);
  await expect(page.locator('[data-motion="pause"]')).toBeVisible();
  await expect(page.locator("html")).toHaveAttribute("data-pause-starts", "1");
  await page.getByRole("button", { name: "Resume", exact: true }).click();
  await expect(page.getByRole("button", { name: "Pause", exact: true })).toBeVisible();
});
