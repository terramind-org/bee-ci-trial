// The orchestrator has replaced the server process, preserving only its data directory.
// A fresh clone must contain the landed content and every imported git commit, without a cache.
import { join } from "node:path";
import { assert, env, fail, readState, runStep, sh } from "./lib/rpc";

await runStep("step 6: restart", async () => {
  const e = env();
  const state = readState(e.work);
  if (!state.repo || !state.mcpChange || !state.gitChange) fail("restart check requires the landed changes");
  const dest = join(e.work, "restarted-clone.git");
  const gitEnv = {
    GIT_CONFIG_COUNT: "1",
    GIT_CONFIG_KEY_0: "http.extraHeader",
    GIT_CONFIG_VALUE_0: `Authorization: Bearer ${e.adminToken}`,
  };
  await sh(["git", "clone", "--quiet", "--mirror", `${e.endpoint}/${state.repo}.git`, dest], { env: gitEnv });
  const git = (args: string[]) => sh(["git", "--git-dir", dest, ...args]);
  await git(["fsck", "--full"]);
  const branch = state.defaultBranch ?? "main";
  const content = (await git(["show", `${branch}:${state.mcpChange.path}`])).stdout;
  assert(content === [...state.mcpChange.lines, state.gitChange.addedLine, ""].join("\n"), "fresh clone after restart contains both landed changes");
  if (state.import) {
    const before = state.import.gitIds;
    const after = new Set((await git(["rev-list", "--all"])).stdout.trim().split("\n"));
    assert(before.every((oid) => after.has(oid)), `all ${before.length} imported git ids survived restart`);
  }
  const verify = await sh([process.execPath, join(import.meta.dir, "step4-verify.ts")]);
  process.stdout.write(verify.stdout);
  assert(verify.code === 0, "log, diff and blame assertions pass after restart");
});
