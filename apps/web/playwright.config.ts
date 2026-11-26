import { defineConfig, devices } from "@playwright/test";

const PORT = Number(process.env.E2E_PORT || 3200);
const API = process.env.BEE_API_URL || "";
// The edition the bee-server under test was configured with (scripts/check-web-e2e.sh runs
// the suite once per edition). Tests tagged with the other edition are left out of this run.
const EDITION = process.env.E2E_EDITION === "cloud" ? "cloud" : "enterprise";
const OTHER_EDITION = EDITION === "cloud" ? /@enterprise/ : /@cloud/;
const START = (port: number) => (process.env.E2E_SKIP_BUILD ? "node_modules/.bin/next start --port " + port : "node_modules/.bin/next build && node_modules/.bin/next start --port " + port);

/**
 * E2E runs against a production build of the UI talking to a real bee-server that
 * scripts/check-web-e2e.sh started and seeded (E2E_DATA_DIR holds its credentials). One server
 * holds one world, so specs run serially; tests that change state create their own objects.
 * The `setup` project signs in through the UI once and shares the session cookie.
 */
export default defineConfig({
  testDir: "./e2e",
  testMatch: /\.(spec|setup)\.ts$/,
  fullyParallel: false,
  workers: 1,
  retries: process.env.CI ? 1 : 0,
  timeout: 60_000,
  expect: { timeout: 15_000 },
  reporter: [["list"]],
  use: {
    baseURL: "http://localhost:" + PORT,
    trace: "off",
    viewport: { width: 1440, height: 900 },
  },
  projects: [
    { name: "setup", testMatch: /auth\.setup\.ts/ },
    {
      name: EDITION,
      dependencies: ["setup"],
      testIgnore: [/auth\.setup\.ts/],
      grepInvert: OTHER_EDITION,
      use: { ...devices["Desktop Chrome"], viewport: { width: 1440, height: 900 }, storageState: "test-results/.auth/session.json" },
    },
  ],
  webServer: {
    command: START(PORT),
    url: "http://localhost:" + PORT + "/signin",
    timeout: 240_000,
    reuseExistingServer: false,
    // No BEE_EDITION: the web app reads the edition from the server, as it does in production.
    env: { BEE_API_URL: API, BEE_ADAPTER: "", BEE_TOKEN: "", BEE_EDITION: "" },
  },
});
