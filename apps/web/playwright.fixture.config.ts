import { defineConfig, devices } from "@playwright/test";

/**
 * The signed-out walk (e2e/public-browse.spec.ts) the feed (e2e/feed.spec.ts) and adoption (e2e/adopt.spec.ts) against the fixture world instead of a real
 * server: `bunx playwright test -c playwright.fixture.config.ts`. It starts `next dev` with
 * BEE_ADAPTER=fixture and no token, so every request is anonymous, exactly as the fixture's
 * gate (lib/api/fixture/anonymous.ts) mirrors the server's ANONYMOUS_READ_METHODS.
 */
const PORT = Number(process.env.E2E_FIXTURE_PORT || 3210);
process.env.E2E_ADAPTER = "fixture";

export default defineConfig({
  testDir: "./e2e",
  testMatch: /(public-browse|feed|adopt|bees|safety)\.spec\.ts$/,
  fullyParallel: false,
  workers: 1,
  timeout: 180_000,
  expect: { timeout: 30_000 },
  reporter: [["list"]],
  use: { baseURL: "http://localhost:" + PORT, trace: "off", viewport: { width: 1440, height: 900 } },
  projects: [{ name: "fixture", use: { ...devices["Desktop Chrome"], viewport: { width: 1440, height: 900 } } }],
  webServer: {
    command: "node_modules/.bin/next dev --port " + PORT,
    url: "http://localhost:" + PORT + "/signin",
    timeout: 240_000,
    reuseExistingServer: !!process.env.E2E_FIXTURE_PORT,
    env: { BEE_ADAPTER: "fixture", BEE_TOKEN: "", BEE_API_URL: "" },
  },
});
