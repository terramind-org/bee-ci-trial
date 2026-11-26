#!/usr/bin/env node
// Lighthouse performance on the change request Brief tabs, run by scripts/check-web-e2e.sh when a `lighthouse`
// binary is on PATH. Starts `next start` against the seeded server (BEE_API_URL, E2E_DATA_DIR,
// E2E_PORT), signs in through /api/session, and fails below a performance score of 0.90.
import { execFileSync, spawn } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const port = Number(process.env.E2E_PORT || 3200);
const base = "http://localhost:" + port;
const cred = readFileSync(join(process.env.E2E_DATA_DIR ?? "", "credentials"), "utf8");
const token = /token = "(.*)"/.exec(cred)?.[1] ?? "";
const seed = JSON.parse(readFileSync(new URL("./.seed.json", import.meta.url), "utf8"));
const MIN = 0.9;

const server = spawn("node_modules/.bin/next", ["start", "--port", String(port)], { stdio: "ignore", env: process.env });
const out = mkdtempSync(join(tmpdir(), "bee-lh-"));
try {
  for (let i = 0; i < 120; i++) {
    const ok = await fetch(base + "/signin").then((r) => r.ok, () => false);
    if (ok) break;
    await new Promise((r) => setTimeout(r, 500));
  }
  const s = await fetch(base + "/api/session", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ token }) });
  const cookie = (s.headers.get("set-cookie") || "").split(";")[0];
  if (!cookie) throw new Error("sign-in failed: " + s.status);
  // The Brief tab of the `fix` (CI test report) and `ui` (before/after, video) changes.
  for (const key of ["fix", "ui"]) {
    const url = base + "/" + seed.repo + "/changes/" + seed.changes[key].id;
    const report = join(out, key + ".json");
    execFileSync("lighthouse", [url, "--only-categories=performance", "--output=json", "--output-path=" + report, "--quiet", "--chrome-flags=--headless=new", "--extra-headers=" + JSON.stringify({ Cookie: cookie })], { stdio: "inherit" });
    const score = JSON.parse(readFileSync(report, "utf8")).categories.performance.score;
    console.log("lighthouse performance on the " + key + " brief: " + score);
    if (score < MIN) {
      console.error("below " + MIN);
      process.exitCode = 1;
    }
  }
} finally {
  server.kill();
  rmSync(out, { recursive: true, force: true });
}
