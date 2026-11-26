// Exit test step 1: `bee-github import <owner/name>` into the dev server, then prove that
//   (a) every commit reachable from the source's branches and tags is in the imported history under
//       the same git id (the imported repo is cloned back through the git gateway and compared
//       with a fresh `git clone --mirror` of the source), and
//   (b) the fidelity report has no unexplained drops.
//
// EXIT_TEST_REPO=<owner/name> picks another source repository. EXIT_TEST_SKIP_IMPORT=1 is the
// documented offline mode: nothing is imported, later steps use the dev server's own
// `dev/playground`, and this step reports INCOMPLETE.

import { join } from "node:path";
import { Api, assert, env, fail, log, ok, redact, registerSecret, runStep, sh, writeState } from "./lib/rpc";
import { checkFidelity } from "./lib/fidelity";

const SKIP = process.env.EXIT_TEST_SKIP_IMPORT === "1";
const REPO = process.env.EXIT_TEST_REPO || "terramind-org/bee";
const BEE_GITHUB = process.env.EXIT_TEST_BEE_GITHUB || "bee-github";
const TIMEOUT_MS = Number(process.env.EXIT_TEST_IMPORT_TIMEOUT_SECS || 3600) * 1000;

/** The GitHub token, never printed: GITHUB_TOKEN, GH_TOKEN, else `gh auth token`. */
async function githubToken(): Promise<string> {
  const fromEnv = process.env.GITHUB_TOKEN || process.env.GH_TOKEN;
  if (fromEnv) return registerSecret(fromEnv);
  const r = await sh(["gh", "auth", "token"], { allowFail: true });
  const token = r.stdout.trim();
  if (r.code !== 0 || !token) fail("no GitHub token: set GITHUB_TOKEN or GH_TOKEN, or run `gh auth login`");
  return registerSecret(token);
}

/** Git config passed through the environment so no credential appears on a command line. */
function gitAuth(header: string): Record<string, string> {
  return {
    GIT_CONFIG_COUNT: "1",
    GIT_CONFIG_KEY_0: "http.extraHeader",
    GIT_CONFIG_VALUE_0: header,
    GIT_TERMINAL_PROMPT: "0",
  };
}

const lines = (s: string): string[] => s.split("\n").filter(Boolean);

await runStep("step 1: import", async () => {
  const e = env();
  if (SKIP) {
    log("EXIT_TEST_SKIP_IMPORT=1: offline mode. Nothing is imported; steps 2-4 run on dev/playground.");
    log("This run cannot pass: step 1 is INCOMPLETE until the import runs.");
    writeState(e.work, { repo: "dev/playground", defaultBranch: "main" });
    return "incomplete";
  }

  const [owner, name] = REPO.split("/");
  if (!owner || !name || REPO.split("/").length !== 2) fail(`EXIT_TEST_REPO must be owner/name, got ${REPO}`);
  const gh = await githubToken();
  const admin = new Api(e.endpoint, e.adminToken);

  // 1. The source: a fresh mirror clone, so the commit list does not depend on the importer.
  const source = join(e.work, "source.git");
  log(`cloning a mirror of github.com/${REPO}`);
  const basic = Buffer.from(`x-access-token:${gh}`).toString("base64");
  await sh(["git", "clone", "--quiet", "--mirror", `https://github.com/${REPO}.git`, source], {
    env: gitAuth(`Authorization: Basic ${basic}`),
  });
  const head = (await sh(["git", "--git-dir", source, "symbolic-ref", "HEAD"])).stdout.trim();
  const branch = head.replace(/^refs\/heads\//, "");
  const sourceCommits = lines((await sh(["git", "--git-dir", source, "rev-list", "--branches", "--tags"])).stdout);
  const sourceGitIds = new Set(lines((await sh(["git", "--git-dir", source, "rev-list", "--all"])).stdout));
  const sourceTip = (await sh(["git", "--git-dir", source, "rev-parse", head])).stdout.trim();
  assert(sourceCommits.length > 0, `source ${REPO} branches and tags contain ${sourceCommits.length} commits (default branch ${branch})`);

  // 2. Import. The org is the GitHub owner; the dev user creates it and is its admin.
  const orgExists = await admin.try("OrgService", "GetOrg", { nameOrId: owner });
  if (!orgExists.ok) await admin.call("OrgService", "CreateOrg", { name: owner, display: owner });
  const started = Date.now();
  const stateDir = join(e.work, "import-state");
  const run = await sh(
    [BEE_GITHUB, "import", REPO, "--server", e.endpoint, "--org", owner, "--state", stateDir, "--json"],
    { env: { BEE_TOKEN: e.adminToken, GH_TOKEN: gh, GITHUB_TOKEN: gh }, cwd: e.work },
  );
  const started_doc = JSON.parse(run.stdout) as { import_id?: string; report?: unknown };
  if (!started_doc.import_id) {
    fail(`bee-github did not hand the import to the server: ${redact(run.stdout).slice(0, 500)}`);
  }
  const importId = started_doc.import_id;
  log(`import ${importId} started`);

  // 3. Wait for it.
  let imp: any;
  let lastProgress = "";
  for (;;) {
    imp = await admin.call("ImportService", "GetImport", { id: importId });
    const progress = (imp.progress ?? []).map((p: any) => `${p.category}=${p.done}`).join(" ");
    if (progress !== lastProgress) log(`  ${imp.state} ${progress}`);
    lastProgress = progress;
    if (imp.state === "IMPORT_STATE_LIVE") break;
    if (imp.state === "IMPORT_STATE_FAILED" || imp.state === "IMPORT_STATE_CANCELLED") {
      fail(`import ${importId} ended ${imp.state}`);
    }
    if (Date.now() - started > TIMEOUT_MS) fail(`import ${importId} still ${imp.state} after ${TIMEOUT_MS / 1000}s`);
    await Bun.sleep(2000);
  }
  ok(`import finished in ${((Date.now() - started) / 1000).toFixed(0)}s: state ${imp.state}`);
  const repoId: string | undefined = imp.repoIds?.[0];
  if (!repoId) fail("the import names no repository");

  // 4. Fidelity report: no unexplained drops.
  const report = await admin.call("ImportService", "GetFidelityReport", { id: importId });
  const fidelity = checkFidelity(report);
  log(`fidelity report: ${fidelity.categories} categories, ${fidelity.imported} imported, ${fidelity.archived} archived, ${fidelity.dropped} dropped`);
  for (const item of report.items ?? []) {
    log(`  ${String(item.category).padEnd(16)} imported ${String(item.imported ?? 0).padStart(6)}  dropped ${String(item.dropped ?? 0).padStart(4)}`);
  }
  for (const x of fidelity.explained) log(`  explained drop: ${x.category} x${x.count}: ${x.reason}`);
  for (const m of report.manualSteps ?? []) log(`  manual step: ${m}`);
  if (fidelity.unexplained.length) fail(`unexplained drops in the fidelity report:\n    ${fidelity.unexplained.join("\n    ")}`);
  ok(`the fidelity report has no unexplained drops (${fidelity.dropped} dropped, all with a reason)`);
  const commitsItem = (report.items ?? []).find((i: any) => i.category === "commits");
  assert(commitsItem && Number(commitsItem.dropped ?? 0) === 0, "no commit was dropped by the import");

  // 5. Clone the imported repository back through the git gateway; every commit keeps its id.
  const back = join(e.work, "imported.git");
  const repoName = imp.source?.replace(/^github:/, "") ?? REPO;
  log(`cloning ${e.endpoint}/${repoName}.git back through the git gateway`);
  await sh(["git", "clone", "--quiet", "--mirror", `${e.endpoint}/${repoName}.git`, back], {
    env: gitAuth(`Authorization: Bearer ${e.adminToken}`),
  });
  const importedCommits = new Set(lines((await sh(["git", "--git-dir", back, "rev-list", "--all"])).stdout));
  const missing = sourceCommits.filter((c) => !importedCommits.has(c));
  if (missing.length) {
    fail(`${missing.length} of ${sourceCommits.length} source commits are missing or have a different id after import, e.g. ${missing.slice(0, 5).join(", ")}`);
  }
  assert(importedCommits.size === Number(commitsItem.imported), `the full mirror contains all ${commitsItem.imported} commits in the fidelity report, including PR refs`);
  // The source can advance while the importer runs. Fetch any later oid directly from
  // GitHub to verify its original identity, including old PR revisions outside branch tips.
  let fetched = 0;
  for (const oid of importedCommits) {
    if (sourceGitIds.has(oid)) continue;
    await sh(["git", "--git-dir", source, "fetch", "--quiet", "origin", oid], {
      env: gitAuth(`Authorization: Basic ${basic}`),
    });
    fetched++;
  }
  ok(`verified every imported git id against GitHub (${importedCommits.size} commits, ${fetched} fetched beyond the initial mirror)`);
  const extra = importedCommits.size - sourceCommits.length;
  ok(`all ${sourceCommits.length} branch/tag commits of ${REPO} are in the imported history under the same git id` +
    (extra > 0 ? ` (${extra} additional commit(s) were imported beyond the source branch/tag snapshot)` : ""));
  await sh(["git", "--git-dir", back, "merge-base", "--is-ancestor", sourceTip, `refs/heads/${branch}`]);
  ok(`the source tip ${sourceTip.slice(0, 12)} is reachable from imported ${branch}`);
  if (extra === 0) {
    const importedTip = (await sh(["git", "--git-dir", back, "rev-parse", `refs/heads/${branch}`])).stdout.trim();
    assert(importedTip === sourceTip, `the imported ${branch} tip equals the source tip (${sourceTip.slice(0, 12)})`);
  }

  // 6. The API's view: the default branch resolves.
  const ref = await admin.call("ObjectService", "ResolveRef", { repo: { fullName: repoName }, name: branch });
  assert(ref.ref?.target?.raw, `${repoName} ref ${branch} resolves at version ${ref.ref?.version}`);

  writeState(e.work, {
    repo: repoName,
    repoId,
    defaultBranch: branch,
    import: { id: importId, commits: importedCommits.size, gitIds: [...importedCommits], extra: Math.max(extra, 0), source: REPO },
  });
});
