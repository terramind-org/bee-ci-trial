#!/usr/bin/env node
/**
 * Captures every real page of the web app at 1440×900 and 390×844, dark and light, into
 * .terramind/swarm/bee-redesign-complete/shots/<edition>/<page>-<width>-<theme>.png.
 *
 *   bun run page:shots                                  # both editions on the fixture world
 *   bun run page:shots -- --edition cloud --only feed,change
 *   bun run page:shots -- --base-url http://127.0.0.1:3100 --edition enterprise
 *
 * --edition   cloud | enterprise | both (default both). Without --base-url, each edition starts its
 *             own `next start` of the current build (run `bun run build` first) on the fixture
 *             world (BEE_FIXTURE_EDITION) on a free port.
 * --dev       start `next dev` instead. Dev streams differently: a notFound() under a loading.tsx
 *             leaves the skeleton on screen there, which production never does.
 * --base-url  an already-running app (fixture or real server); --edition then only names the folder
 * --only      comma-separated page names (substring match), for re-capturing a few after a fix
 * --out       the output directory (default above)
 *
 * Signed-in pages carry the session cookie the sign-in form sets (the fixture accepts any token of
 * eight or more characters as the demo user); signed-out pages carry none. Ids in dynamic routes
 * (a change, a post, an agent, a run, an issue, a topic) are read from the links on their list
 * pages, so the script works against any seeded world. It exits non-zero when a page answers
 * anything but 200, shows an error, still shows a loading skeleton after 30 s, or scrolls sideways
 * at 390 px. The hosted edition's /pricing
 * and billing must show the not-found page on enterprise (streamed, so still HTTP 200).
 */
import { spawn } from "node:child_process";
import { existsSync, mkdirSync } from "node:fs";
import { createServer } from "node:net";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { chromium } from "@playwright/test";

const webDir = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const DEFAULT_OUT = path.resolve(webDir, "../../.terramind/swarm/bee-redesign-complete/shots");
const THEMES = ["dark", "light"];
const WIDTHS = [
  [1440, 900],
  [390, 844],
];
const THEME_KEY = "bee-theme";
const TOKEN_COOKIE = "bee_token";
const TOKEN = process.env.SHOTS_TOKEN || "fixture-page-shots";
const REPO = process.env.SHOTS_REPO || "acme/api";
const POSTS_REPO = process.env.SHOTS_POSTS_REPO || "lattice/engine";
const ORG = REPO.split("/")[0];

/** Pages of the hosted edition only: another edition shows the not-found page. */
const CLOUD_ONLY = new Set(["pricing", "org-billing"]);

/** The heading of app/not-found.tsx. */
const NOT_FOUND = "Nothing here";

/** Text that only shows when a page failed to render. */
const ERROR_TEXT = ["UNAUTHENTICATED", "Application error", "Unhandled Runtime Error", "This page could not be found", "Internal Server Error"];

function parseArgs(argv) {
  const args = { edition: "both", baseUrl: "", only: [], out: DEFAULT_OUT, dev: false };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a === "--") continue;
    if (a === "--edition") args.edition = argv[++i] ?? "";
    else if (a === "--base-url") args.baseUrl = (argv[++i] ?? "").replace(/\/+$/, "");
    else if (a === "--only") args.only = (argv[++i] ?? "").split(",").filter(Boolean);
    else if (a === "--out") args.out = path.resolve(argv[++i] ?? "");
    else if (a === "--dev") args.dev = true;
    else throw new Error(`unknown argument ${a}`);
  }
  if (!["cloud", "enterprise", "both"].includes(args.edition)) throw new Error(`--edition must be cloud, enterprise or both (got "${args.edition}")`);
  if (args.baseUrl && args.edition === "both") throw new Error("--base-url serves one edition: pass --edition cloud or enterprise");
  return args;
}

function freePort() {
  return new Promise((resolve, reject) => {
    const srv = createServer();
    srv.once("error", reject);
    srv.listen(0, "127.0.0.1", () => {
      const { port } = srv.address();
      srv.close(() => resolve(port));
    });
  });
}

async function startServer(edition, dev) {
  if (!dev && !existsSync(path.join(webDir, ".next/BUILD_ID"))) throw new Error("no build: run `bun run build` first, or pass --dev");
  const port = await freePort();
  const child = spawn(path.join(webDir, "node_modules/.bin/next"), [dev ? "dev" : "start", "--port", String(port)], {
    cwd: webDir,
    // The live fixture world keeps changing (the Inbox count cycles 4, 5, 4 as its docs agent opens
    // and lands changes), so captures taken seconds apart disagree. Freeze the world for the run.
    env: { BEE_FIXTURE_LIVE: "0", ...process.env, BEE_ADAPTER: "fixture", BEE_FIXTURE_EDITION: edition, BEE_TOKEN: "", BEE_EDITION: "", NEXT_TELEMETRY_DISABLED: "1" },
    stdio: ["ignore", "pipe", "pipe"],
  });
  let log = "";
  child.stdout.on("data", (d) => (log += d));
  child.stderr.on("data", (d) => (log += d));
  const url = `http://127.0.0.1:${port}`;
  const deadline = Date.now() + 120_000;
  for (;;) {
    if (child.exitCode !== null) throw new Error(`next exited early:\n${log}`);
    if (Date.now() > deadline) {
      child.kill("SIGTERM");
      throw new Error(`next did not start within 120s:\n${log}`);
    }
    try {
      await fetch(url + "/signin", { redirect: "manual" });
      break;
    } catch {
      await new Promise((r) => setTimeout(r, 500));
    }
  }
  return { url, stop: () => child.kill("SIGTERM") };
}

/**
 * Opens `url` and waits for it to settle: loaded, no loading skeleton left, images decoded. Not
 * "networkidle": live pages keep an event stream open for as long as they are shown.
 */
async function open(page, url) {
  const res = await page.goto(url, { waitUntil: "load", timeout: 120_000 });
  await page.waitForFunction(() => !document.querySelector("[aria-busy=true]"), null, { timeout: 30_000 }).catch(() => {});
  await page.waitForFunction(() => [...document.images].every((i) => i.complete), null, { timeout: 15_000 }).catch(() => {});
  await page.waitForTimeout(400);
  return res;
}

/** The first link on `listPath` whose path matches `re`, as a path; throws when there is none. */
async function discover(page, base, listPath, re) {
  await open(page, base + listPath);
  const hrefs = await page.locator("a[href]").evaluateAll((as) => as.map((a) => new URL(a.href).pathname));
  const hit = hrefs.find((h) => re.test(h));
  if (!hit) throw new Error(`no link matching ${re} on ${listPath}`);
  return hit;
}

const esc = (s) => s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");

/** Every page: [name, path, signedIn]. Dynamic ids come from their list pages. */
async function targets(page, base) {
  const r = "/" + REPO;
  const rx = esc(r);
  const change = await discover(page, base, r + "/changes", new RegExp(`^${rx}/changes/[^/]+$`));
  const run = await discover(page, base, r + "/runs", new RegExp(`^${rx}/runs/[^/]+$`));
  const agent = await discover(page, base, r + "/agents", new RegExp(`^${rx}/agents/[^/]+$`));
  const issue = await discover(page, base, r + "/work", new RegExp(`^${rx}/work/\\d+$`));
  const post = await discover(page, base, "/" + POSTS_REPO + "/updates", new RegExp(`^/${esc(POSTS_REPO)}/posts/[^/]+$`));
  const topic = await discover(page, base, "/explore", /^\/topics\/[^/]+$/);
  const s = [
    ["inbox", "/"],
    ["feed", "/feed"],
    ["explore", "/explore"],
    ["trending", "/trending"],
    ["leaderboards", "/leaderboards"],
    ["repositories", "/repositories"],
    ["topic", topic],
    ["profile-user", "/mara"],
    ["profile-org", "/" + ORG],
    ["new", "/new"],
    ["import", "/import"],
    ["pricing", "/pricing"],
    ["prompts", "/prompts"],
    ["search", "/search?q=invoice"],
    ["admin", "/admin"],
    ["admin-enterprise", "/admin/enterprise"],
    ["admin-identity", "/admin/identity"],
    ["admin-plans", "/admin/plans"],
    ["admin-policies", "/admin/policies"],
    ["admin-readiness", "/admin/readiness"],
    ["admin-security", "/admin/security"],
    ["org-settings", `/orgs/${ORG}/settings`],
    ["org-members", `/orgs/${ORG}/settings/members`],
    ["org-teams", `/orgs/${ORG}/settings/teams`],
    ["org-apps", `/orgs/${ORG}/settings/apps`],
    ["org-audit", `/orgs/${ORG}/settings/audit`],
    ["org-billing", `/orgs/${ORG}/settings/billing`],
    ["repo", r],
    ["code", r + "/code"],
    ["code-file", r + "/code?path=README.md"],
    ["activity", r + "/activity"],
    ["agents", r + "/agents"],
    ["agent", agent],
    ["changes", r + "/changes"],
    ["change-brief", change],
    ["change-code", change + "/code"],
    ["change-checks", change + "/checks"],
    ["change-agents", change + "/agents"],
    ["runs", r + "/runs"],
    ["run", run],
    ["issues", r + "/work"],
    ["issue", issue],
    ["board", r + "/work/board"],
    ["releases", r + "/releases"],
    ["packages", r + "/packages"],
    ["pages", r + "/pages"],
    ["security", r + "/security"],
    ["repo-settings", r + "/settings"],
    ["updates", "/" + POSTS_REPO + "/updates"],
    ["post", post],
  ].map(([n, p]) => [n, p, true]);
  return [
    ...s,
    ["out-home", "/", false],
    ["out-signin", "/signin", false],
    ["out-explore", "/explore", false],
    ["out-repo", "/" + POSTS_REPO, false],
    ["out-post", post, false],
  ];
}

async function capture(browser, base, edition, outDir, only, failures) {
  const dir = path.join(outDir, edition);
  mkdirSync(dir, { recursive: true });
  const host = new URL(base).hostname;
  const signedIn = async (context) => context.addCookies([{ name: TOKEN_COOKIE, value: TOKEN, domain: host, path: "/" }]);

  const probe = await browser.newContext({ viewport: { width: 1440, height: 900 } });
  await signedIn(probe);
  let list = await targets(await probe.newPage(), base);
  await probe.close();
  if (only.length) list = list.filter(([n]) => only.some((o) => n.includes(o)));

  const files = [];
  for (const theme of THEMES) {
    for (const [width, height] of WIDTHS) {
      for (const auth of [true, false]) {
        const pages = list.filter(([, , s]) => s === auth);
        if (!pages.length) continue;
        const context = await browser.newContext({ viewport: { width, height }, deviceScaleFactor: 1 });
        // The same switch components/theme.tsx reads before paint.
        await context.addInitScript(([key, t]) => localStorage.setItem(key, t), [THEME_KEY, theme]);
        if (auth) await signedIn(context);
        const page = await context.newPage();
        for (const [name, route] of pages) {
          const where = `${edition} ${route} ${width} ${theme}`;
          const res = await open(page, base + route);
          const status = res?.status() ?? 0;
          const text = await page.locator("body").innerText();
          const actual = await page.evaluate(() => document.documentElement.dataset.theme);
          const scroll = await page.evaluate(() => document.documentElement.scrollWidth);
          const gone = edition !== "cloud" && CLOUD_ONLY.has(name);
          if (status !== 200) failures.push(`${where}: HTTP ${status}`);
          if (gone && !text.includes(NOT_FOUND)) failures.push(`${where}: expected the not-found page`);
          const shown = gone ? undefined : ERROR_TEXT.find((t) => text.includes(t));
          if (shown) failures.push(`${where}: page shows "${shown}"`);
          if (actual !== theme) failures.push(`${where}: theme is ${actual}`);
          if (scroll > width) failures.push(`${where}: scrolls sideways (${scroll}px)`);
          if (await page.locator("[aria-busy=true]").count()) failures.push(`${where}: still shows a loading skeleton after 30s`);
          const file = path.join(dir, `${name}-${width}-${theme}.png`);
          await page.screenshot({ path: file });
          files.push(file);
          console.log(`${status} ${where} -> ${path.relative(outDir, file)}`);
        }
        await context.close();
      }
    }
  }
  return files;
}

async function main() {
  const args = parseArgs(process.argv.slice(2));
  const editions = args.edition === "both" ? ["cloud", "enterprise"] : [args.edition];
  const browser = await chromium.launch();
  const failures = [];
  let total = 0;
  try {
    for (const edition of editions) {
      const server = args.baseUrl ? { url: args.baseUrl, stop: () => {} } : await startServer(edition, args.dev);
      try {
        total += (await capture(browser, server.url, edition, args.out, args.only, failures)).length;
      } finally {
        server.stop();
      }
    }
  } finally {
    await browser.close();
  }
  console.log(`${total} screenshots in ${path.relative(process.cwd(), args.out) || "."}`);
  for (const f of failures) console.error("FAIL " + f);
  return failures.length ? 1 : 0;
}

main().then(
  (code) => process.exit(code),
  (err) => {
    console.error(err instanceof Error ? err.stack : err);
    process.exit(1);
  },
);
