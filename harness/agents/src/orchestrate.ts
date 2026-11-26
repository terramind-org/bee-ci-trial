// Runs the scenario × runtime × run matrix and aggregates the results.
//
// Every run is independent: it gets its own org namespace (`run.ns`), its own setup, its own
// agent token minted for that run (`run.session` lands in provenance) and a fresh empty working
// directory. Pass/fail comes only from the assertions.

import { existsSync, mkdirSync, writeFileSync, rmSync } from "node:fs";
import { join } from "node:path";
import { randomBytes } from "node:crypto";
import type { Catalog } from "./catalog";
import { ConnectClient } from "./connect";
import { freshWorkdir, launch, type AgentExit, type HarnessEnv, type Runtime, type RuntimeInfo } from "./runners";
import type { Fixture, Scenario, Step } from "./scenario";
import { runSteps, type Executor, type StepOutcome } from "./steps";
import { interpolate, type TemplateContext } from "./template";

export type RunStatus = "pass" | "fail" | "timeout" | "setup-error" | "blocked" | "harness-error";

export interface RunResult {
  scenario: string;
  runtime: string;
  run: number;
  status: RunStatus;
  reason?: string;
  ns: string;
  session: string;
  startedAt: string;
  durationMs: number;
  agents: AgentExit[];
  setup: StepOutcome[];
  during: StepOutcome[];
  assertions: StepOutcome[];
  transcriptDir: string;
}

export interface CellResult {
  scenario: string;
  runtime: string;
  passes: number;
  runs: number;
  statuses: RunStatus[];
}

export interface Results {
  startedAt: string;
  finishedAt: string;
  server: string;
  runsPerCell: number;
  runtimes: RuntimeInfo[];
  cells: CellResult[];
  runs: RunResult[];
  ok: boolean;
  /** runs/<timestamp>, where results.json, summary.md and transcripts live. */
  dir: string;
}

export interface OrchestrateOptions {
  server: string;
  adminToken?: string;
  scenarios: Scenario[];
  fixtures: Map<string, Fixture>;
  runtimes: Runtime[];
  runs: number;
  parallel: number;
  catalog: Catalog;
  env: HarnessEnv;
  runsDir: string;
  /** Named pre-provisioned fixtures (`--fixture name=value`), satisfying `fixture:<name>`. */
  namedFixtures: Record<string, string>;
  keepWorkdirs: boolean;
  /** Multiplies every scenario timeout (slow machines). */
  timeoutScale: number;
  log: (line: string) => void;
}

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

export function stamp(d = new Date()): string {
  return d.toISOString().replace(/[-:]/g, "").replace(/\..+/, "Z");
}

function namespace(): string {
  // Lowercase, starts with a letter, short enough for org names everywhere.
  return `h${Date.now().toString(36)}${randomBytes(3).toString("hex")}`;
}

/** A short, path-safe name for a runtime: claude-code, codex, command-<script name>. */
export function runtimeSlug(id: string): string {
  const short = id.startsWith("command:") ? `command-${id.split("/").pop()}` : id;
  return short.replace(/[^\w.-]/g, "_");
}

/** Host requirements that are not met, as human-readable reasons. */
export function unmetRequirements(s: Scenario, named: Record<string, string>, env: HarnessEnv): string[] {
  const out: string[] = [];
  for (const r of s.requires ?? []) {
    const [kind, name] = r.split(":") as [string, string];
    if (kind === "bin" && name === "bee" && !env.beeBin) out.push("no bee binary (--bee-bin)");
    else if (kind === "bin" && name !== "bee" && !Bun.which(name)) out.push(`${name} not on PATH`);
    else if (kind === "env" && !process.env[name]) out.push(`env ${name} not set`);
    else if (kind === "fixture" && !(name in named)) out.push(`fixture ${name} not provided (--fixture ${name}=…)`);
  }
  if (s.surfaces !== "cli" && !env.beeBin) out.push("surfaces include mcp but no bee binary");
  if (s.surfaces !== "mcp" && !env.beeBin) out.push("surfaces include cli but no bee binary");
  return [...new Set(out)];
}

async function runOne(
  o: OrchestrateOptions,
  s: Scenario,
  rt: Runtime,
  info: RuntimeInfo,
  n: number,
  root: string,
): Promise<RunResult> {
  const startedAt = new Date();
  const ns = namespace();
  const session = `harness/${s.id}/${runtimeSlug(rt.id)}/${n}/${ns}`;
  const transcriptDir = join(root, s.id, runtimeSlug(rt.id), String(n));
  mkdirSync(transcriptDir, { recursive: true });
  const base: RunResult = {
    scenario: s.id,
    runtime: rt.id,
    run: n,
    status: "harness-error",
    ns,
    session,
    startedAt: startedAt.toISOString(),
    durationMs: 0,
    agents: [],
    setup: [],
    during: [],
    assertions: [],
    transcriptDir,
  };
  const finish = (r: RunResult): RunResult => {
    r.durationMs = Date.now() - startedAt.getTime();
    writeFileSync(join(transcriptDir, "result.json"), JSON.stringify(r, null, 2));
    o.log(`${s.id} ${rt.id} #${n}: ${r.status}${r.reason ? ` (${r.reason})` : ""}`);
    return r;
  };
  if (!info.available) return finish({ ...base, status: "blocked", reason: `runtime unavailable: ${info.problem}` });
  const unmet = unmetRequirements(s, o.namedFixtures, o.env);
  if (unmet.length) return finish({ ...base, status: "blocked", reason: unmet.join("; ") });

  const workdir = freshWorkdir();
  const url = new URL(o.server);
  const ctx: TemplateContext = {
    now: startedAt,
    runId: ns,
    vars: {
      run: { id: ns, ns, session, n, runtime: rt.id, server: { url: o.server.replace(/\/+$/, ""), host: url.host, hostname: url.hostname }, fixtures: o.namedFixtures },
      scenario: { id: s.id, area: s.area },
      admin: { token: o.adminToken ?? "" },
    },
  };
  const ex: Executor = {
    client: new ConnectClient(o.server),
    catalog: o.catalog,
    adminToken: o.adminToken,
    ctx,
    log: (l) => o.log(`[${s.id} ${rt.id} #${n}]${l}`),
  };
  try {
    const setupSteps: Step[] = [...(s.fixtures ?? []).flatMap((f) => o.fixtures.get(f)?.setup ?? []), ...s.setup];
    base.setup = await runSteps(ex, setupSteps, true);
    if (base.setup.some((x) => !x.ok)) {
      const bad = base.setup.find((x) => !x.ok)!;
      return finish({ ...base, status: "setup-error", reason: `${bad.step}: ${bad.details.join("; ")}` });
    }

    const timeoutMs = Math.round(s.timeout * 1000 * o.timeoutScale);
    const agentToken = String(interpolate(ctx, s.agent.token));
    const agents: Promise<AgentExit>[] = [];
    agents.push(
      launch(rt, {
        name: "agent",
        prompt: String(interpolate(ctx, s.prompt)).trim(),
        workdir,
        transcriptDir,
        endpoint: o.server,
        token: agentToken,
        surfaces: s.surfaces,
        timeoutMs,
        extraEnv: interpolate(ctx, s.agent.env ?? {}),
        maxTurns: s.budget?.max_turns,
        maxUsd: s.budget?.max_usd,
      }, o.env),
    );
    for (const p of s.peers ?? []) {
      const peerDir = join(workdir, "..", `${workdir.split("/").pop()}-${p.name}`);
      agents.push(
        (async () => {
          if (p.delay_ms) await sleep(p.delay_ms);
          return launch(rt, {
            name: p.name,
            prompt: String(interpolate(ctx, p.prompt)).trim(),
            workdir: peerDir,
            transcriptDir,
            endpoint: o.server,
            token: String(interpolate(ctx, p.token)),
            surfaces: s.surfaces,
            timeoutMs: Math.max(1000, timeoutMs - (p.delay_ms ?? 0)),
            extraEnv: interpolate(ctx, p.env ?? {}),
            maxTurns: s.budget?.max_turns,
            maxUsd: s.budget?.max_usd,
          }, o.env);
        })(),
      );
    }
    // `during` blocks: harness actions while agents work (inject an event, a fault, a peer's push).
    const agentsDone = Promise.all(agents);
    let done = false;
    void agentsDone.then(() => (done = true));
    const duringRuns = (s.during ?? []).map(async (d) => {
      const t0 = Date.now();
      if (d.wait_for_file) {
        const limit = (d.wait_timeout_s ?? s.timeout) * 1000 * o.timeoutScale;
        while (!existsSync(join(workdir, d.wait_for_file)) && !done && Date.now() - t0 < limit) await sleep(200);
        if (!existsSync(join(workdir, d.wait_for_file))) {
          return [{ step: `wait for ${d.wait_for_file}`, ok: false, details: ["agent never created it"], ms: Date.now() - t0 }];
        }
      } else if (d.after_ms) {
        while (!done && Date.now() - t0 < d.after_ms * o.timeoutScale) await sleep(100);
      }
      return runSteps({ ...ex, ctx: { ...ctx, workdir } }, d.steps, true);
    });
    base.agents = await agentsDone;
    base.during = (await Promise.all(duringRuns)).flat();

    ctx.workdir = workdir;
    base.assertions = await runSteps(ex, s.assertions, false);
    if (s.teardown) await runSteps(ex, s.teardown, false);
    const failed = base.assertions.filter((a) => !a.ok);
    const timedOut = base.agents.some((a) => a.timedOut);
    if (failed.length === 0) return finish({ ...base, status: "pass" });
    const reason = failed.map((f) => `${f.step}: ${f.details.filter((d) => d.startsWith("FAIL")).join("; ") || f.details.join("; ")}`).join(" | ");
    return finish({ ...base, status: timedOut ? "timeout" : "fail", reason });
  } catch (e) {
    return finish({ ...base, status: "harness-error", reason: (e as Error).stack ?? String(e) });
  } finally {
    if (!o.keepWorkdirs) {
      rmSync(workdir, { recursive: true, force: true });
      for (const p of s.peers ?? []) rmSync(join(workdir, "..", `${workdir.split("/").pop()}-${p.name}`), { recursive: true, force: true });
    }
  }
}

async function pool<T>(jobs: (() => Promise<T>)[], width: number): Promise<T[]> {
  const out: T[] = new Array(jobs.length);
  let next = 0;
  const worker = async () => {
    while (next < jobs.length) {
      const i = next++;
      out[i] = await jobs[i]!();
    }
  };
  await Promise.all(Array.from({ length: Math.max(1, Math.min(width, jobs.length)) }, worker));
  return out;
}

export function aggregate(runs: RunResult[], scenarios: string[], runtimes: string[], perCell: number): CellResult[] {
  const cells: CellResult[] = [];
  for (const s of scenarios) {
    for (const r of runtimes) {
      const mine = runs.filter((x) => x.scenario === s && x.runtime === r).sort((a, b) => a.run - b.run);
      cells.push({ scenario: s, runtime: r, passes: mine.filter((x) => x.status === "pass").length, runs: perCell, statuses: mine.map((x) => x.status) });
    }
  }
  return cells;
}

export async function orchestrate(o: OrchestrateOptions): Promise<Results> {
  const startedAt = new Date();
  const root = join(o.runsDir, stamp(startedAt));
  mkdirSync(root, { recursive: true });
  const infos = await Promise.all(o.runtimes.map((r) => r.detect(o.env)));
  for (const i of infos) o.log(`runtime ${i.id}: ${i.available ? `${i.version} (${i.path})` : `UNAVAILABLE: ${i.problem}`}`);
  const jobs: (() => Promise<RunResult>)[] = [];
  for (const s of o.scenarios) {
    o.runtimes.forEach((rt, i) => {
      for (let n = 1; n <= o.runs; n++) jobs.push(() => runOne(o, s, rt, infos[i]!, n, root));
    });
  }
  const runs = await pool(jobs, o.parallel);
  const cells = aggregate(runs, o.scenarios.map((s) => s.id), o.runtimes.map((r) => r.id), o.runs);
  const results: Results = {
    startedAt: startedAt.toISOString(),
    finishedAt: new Date().toISOString(),
    server: o.server,
    runsPerCell: o.runs,
    runtimes: infos,
    cells,
    runs,
    ok: cells.length > 0 && cells.every((c) => c.passes === c.runs),
    dir: root,
  };
  writeFileSync(join(root, "results.json"), JSON.stringify(results, null, 2));
  return results;
}
