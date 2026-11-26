// Exit test step 5: run the SPEC §16 load harness (the `load` example of bee-conformance) and
// print its report: every target with its measurement, and a line for each target that was
// missed and by how much. The step passes when the harness runs to the end and reports every
// target; a miss is a finding the report states, not a failure of the step.
//
// EXIT_TEST_LOAD_BIN is the built harness binary; EXIT_TEST_LOAD_ARGS (default below) its
// workload flags. The defaults are the CI-sized workload documented in docs/e2e.md.

import { join } from "node:path";
import { writeFileSync } from "node:fs";
import { assert, env, fail, log, runStep, sh } from "./lib/rpc";
import { parseTargets, summarize } from "./lib/loadreport";

const DEFAULT_ARGS = "--landers 1000 --samples 100 --sessions 10000 --files 10000";
/** SPEC §16 rows the harness must report on (the table is judged row by row in SPEC order). */
const MIN_ROWS = 10;

await runStep("step 5: load harness", async () => {
  const e = env();
  const bin = process.env.EXIT_TEST_LOAD_BIN;
  if (!bin) fail("EXIT_TEST_LOAD_BIN is not set; run steps through scripts/e2e/exit-test.sh");
  const args = (process.env.EXIT_TEST_LOAD_ARGS || DEFAULT_ARGS).split(/\s+/).filter(Boolean);
  const out = join(e.work, "load-report.md");
  log(`${bin.split("/").pop()} ${args.join(" ")}`);
  const t0 = Date.now();
  const run = await sh([bin, ...args, "--out", out], { allowFail: true });
  writeFileSync(join(e.work, "load.log"), run.stdout + run.stderr);
  if (run.code !== 0) fail(`the load harness exited ${run.code}: ${run.stderr.trim().slice(-1500)}`);
  log(`the harness ran for ${((Date.now() - t0) / 1000).toFixed(0)}s; its report is ${out}`);

  const report = run.stdout;
  const rows = parseTargets(report);
  assert(rows.length >= MIN_ROWS, `the report judges ${rows.length} SPEC §16 targets`);
  const s = summarize(rows);
  assert(s.notRun.length === 0, `every target was measured (not run: ${s.notRun.map((r) => r.operation).join("; ") || "none"})`);

  console.log(`\n${report.split("\n").filter((l) => !/^\| Extra measurement|^\|---\|---\|$/.test(l)).join("\n")}`);
  log(`SPEC §16: ${s.hits} targets hit, ${s.misses.length} missed, ${s.notJudged} not judged (monthly SLOs).`);
  for (const m of s.misses) {
    log(`  MISSED: ${m.operation}`);
    log(`          target ${m.target}`);
    log(`          measured p50 ${m.p50}, p99 ${m.p99} (${m.measured}); over the bound by ${m.by}${m.notes ? `; ${m.notes}` : ""}`);
  }
  if (s.misses.length === 0) log("  no target was missed.");
});
