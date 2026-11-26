// Exit test step 3: `git clone` against the server and `git push` a second change, which lands
// under policy.
//
// A second principal (the dev server's human user, so blame can tell the two changes apart)
// clones over smart HTTP, commits and pushes to the default branch. A ruleset on that branch
// restricts secrets/**: the push has to go through Land and pass it. A second push that touches
// secrets/ has to be refused with the SPEC §12.2 reason POLICY_DENIED, and the branch must not
// move.
//
// The pusher is a human on purpose: a ruleset created through the API carries no
// `agents_may_land_without_human`, which SPEC §9.4 reads as "agents always need a human
// approval", so an agent push under any API ruleset waits for a review (see docs/e2e.md).

import { join } from "node:path";
import { Api, assert, env, fail, log, ok, rawHex, readFile, readState, runStep, sh, writeState } from "./lib/rpc";

const GIT_NOTE = "exit-test/git-note.txt";
const ADDED_LINE = "A git push added this line and it landed under policy.";

await runStep("step 3: git", async () => {
  const e = env();
  const state = readState(e.work);
  if (!state.repo || !state.mcpChange) fail("step 3 needs the state step 2 leaves behind; run the steps in order");
  const { repo } = state;
  const branch = state.defaultBranch ?? "main";
  const admin = new Api(e.endpoint, e.adminToken);

  const who = await admin.call("AuthService", "WhoAmI", {});
  const pusher = { id: (who.principal?.id ?? who.id) as string, token: e.adminToken };
  if (!pusher.id) fail("AuthService/WhoAmI did not name the dev user");
  writeState(e.work, { pusher });

  // Policy: nothing under secrets/ may change on the default branch.
  const ruleset = await admin.call("PolicyService", "CreateRuleset", {
    repo: { fullName: repo },
    name: "exit-test-protect-secrets",
    target: "RULESET_TARGET_BRANCH",
    enforcement: "RULESET_ENFORCEMENT_ACTIVE",
    conditions: { refNameIncludes: ["~DEFAULT_BRANCH"] },
    rules: [{ restrictedPaths: { pathGlobs: ["secrets/**"] } }],
  });
  ok(`ruleset ${ruleset.id} restricts secrets/** on the default branch`);
  writeState(e.work, { rulesetId: ruleset.id });

  const auth = {
    GIT_CONFIG_COUNT: "1",
    GIT_CONFIG_KEY_0: "http.extraHeader",
    GIT_CONFIG_VALUE_0: `Authorization: Bearer ${pusher.token}`,
    GIT_TERMINAL_PROMPT: "0",
  };
  const author = {
    GIT_AUTHOR_NAME: "Exit Test Pusher",
    GIT_AUTHOR_EMAIL: "pusher@exit-test.invalid",
    GIT_COMMITTER_NAME: "Exit Test Pusher",
    GIT_COMMITTER_EMAIL: "pusher@exit-test.invalid",
  };
  const dir = join(e.work, "clone");
  const git = (args: string[], extra: Record<string, string> = {}, allowFail = false) =>
    sh(["git", ...args], { cwd: dir, env: { ...auth, ...author, ...extra }, allowFail });

  // Clone. The working tree is skipped (a large repository's checkout is not what is under test);
  // the index is read from HEAD so a commit sees the whole tree.
  log(`git clone ${e.endpoint}/${repo}.git`);
  await sh(["git", "clone", "--quiet", "--no-checkout", `${e.endpoint}/${repo}.git`, dir], { env: auth });
  await git(["read-tree", "HEAD"]);
  const headBefore = (await git(["rev-parse", "HEAD"])).stdout.trim();
  const branchBefore = (await git(["rev-parse", `refs/remotes/origin/${branch}`])).stdout.trim();
  assert(headBefore === branchBefore, `the clone is at origin/${branch} (${headBefore.slice(0, 12)})`);
  const cloned = (await git(["show", `HEAD:${state.mcpChange.path}`])).stdout;
  assert(cloned === `${state.mcpChange.lines.join("\n")}\n`, `the clone contains the file step 2 landed over MCP (${state.mcpChange.path})`);

  // The change that has to land: one more line in step 2's file and a new file.
  const stage = async (path: string, text: string) => {
    const blob = await sh(["git", "hash-object", "-w", "--stdin"], { cwd: dir, stdin: text });
    await git(["update-index", "--add", "--cacheinfo", `100644,${blob.stdout.trim()},${path}`]);
  };
  await stage(state.mcpChange.path, `${state.mcpChange.lines.join("\n")}\n${ADDED_LINE}\n`);
  await stage(GIT_NOTE, "Pushed with plain git by the phase 1 exit test.\n");
  const when = "2026-01-01T00:00:00Z";
  await git(["commit", "--quiet", "-m", "Add a line and a note by git push\n\nPushed by the phase 1 exit test."], { GIT_AUTHOR_DATE: when, GIT_COMMITTER_DATE: when });
  const pushed = (await git(["rev-parse", "HEAD"])).stdout.trim();

  const push = await git(["push", "origin", `HEAD:refs/heads/${branch}`], {}, true);
  if (push.code !== 0) fail(`git push to the protected ${branch} was refused: ${push.stderr.trim().slice(-1500)}`);
  ok(`git push of ${pushed.slice(0, 12)} to ${branch} was accepted`);

  // It landed: the branch moved to a descendant of the old tip that carries the push.
  const ref = await admin.call("ObjectService", "ResolveRef", { repo: { fullName: repo }, name: branch });
  const log1 = await admin.call("QueryService", "Log", { repo: { fullName: repo }, from: { expr: branch }, page: { pageSize: 2 }, includeMeta: true });
  const tip = log1.changesets[0];
  assert(rawHex(tip.id) === rawHex(ref.ref.target), `the log's first entry is the branch tip`);
  assert(rawHex(log1.changesets[1].id) === state.mcpChange.id, `the push landed on top of step 2's change`);
  assert(tip.meta?.creator === pusher.id, `the pushed change was created by the pusher (${tip.meta?.creator})`);
  const added = (await readFile(admin, repo, branch, state.mcpChange.path)).split("\n");
  assert(added[3] === ADDED_LINE, `${state.mcpChange.path} on ${branch} has the pushed 4th line`);
  const remote = (await git(["ls-remote", "origin", `refs/heads/${branch}`])).stdout.split(/\s+/)[0];
  assert(remote === pushed, `git ls-remote reports ${branch} at the pushed commit ${pushed.slice(0, 12)}`);

  // The server records a land's ruleset evaluation after it answers the land (it never holds
  // a land for the record), so the record is awaited, for at most five seconds.
  const evaluationRecorded = async (matches: (x: any) => boolean): Promise<boolean> => {
    for (let waited = 0; waited <= 5000; waited += 100) {
      const r = await admin.call("PolicyService", "ListRulesetEvaluations", { repo: { fullName: repo }, ref: branch, actorId: pusher.id });
      if ((r.evaluations ?? []).some(matches)) return true;
      await new Promise((done) => setTimeout(done, 100));
    }
    return false;
  };

  // Policy evaluated it and passed it.
  assert(
    await evaluationRecorded((x) => x.result === "RULESET_EVALUATION_RESULT_PASS"),
    `policy evaluated the push against the ruleset and passed it`,
  );

  // A push the ruleset forbids: a file under secrets/.
  const secret = "secrets/exit-test.txt";
  await stage(secret, "this path is restricted by the default-branch ruleset\n");
  await git(["commit", "--quiet", "-m", "Add a restricted file"], { GIT_AUTHOR_DATE: when, GIT_COMMITTER_DATE: when });
  const refused = await git(["push", "origin", `HEAD:refs/heads/${branch}`], {}, true);
  assert(refused.code !== 0, `git push touching ${secret} was refused`);
  const output = refused.stderr + refused.stdout;
  assert(/POLICY_DENIED/.test(output), `the refusal names the SPEC §12.2 reason POLICY_DENIED: ${output.split("\n").find((l) => l.includes("POLICY_DENIED"))?.trim()}`);
  const after = await admin.call("ObjectService", "ResolveRef", { repo: { fullName: repo }, name: branch });
  assert(rawHex(after.ref.target) === rawHex(ref.ref.target) && after.ref.version === ref.ref.version, `${branch} did not move after the refused push (version ${after.ref.version})`);
  assert(
    await evaluationRecorded((x) => x.result !== "RULESET_EVALUATION_RESULT_PASS"),
    `policy recorded the refused push as a ruleset evaluation that did not pass`,
  );

  writeState(e.work, { gitChange: { commit: rawHex(tip.id), path: state.mcpChange.path, addedLine: ADDED_LINE, secretRejected: true } });
});
