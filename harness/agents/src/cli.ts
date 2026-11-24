#!/usr/bin/env bun
// Entry point: `bun run harness [options]`, `bun run harness --dry-run`, `bun run harness describe X`.

import { existsSync, mkdirSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { Catalog, repoRoot } from "./catalog";
import { describe } from "./describe";
import { orchestrate } from "./orchestrate";
import { summary } from "./report";
import { runtimeById, type HarnessEnv } from "./runners";
import {
  errorReasons, loadFixtures, loadScenarios, loadYaml, productScenarios, scenarioFiles, validateScenario,
  type Scenario,
} from "./scenario";

const HERE = resolve(import.meta.dir, "..");

export const HELP = `bee agent scenario harness (PRODUCT §3.1)

Runs each PRODUCT §4 agent scenario with fresh agents (Claude Code, Codex) against a bee
server and checks the goal state through API assertions only. Exits 0 only when every selected
scenario passes every run on every selected runtime.

Usage:
  bun run harness --server <url> [options]      run the matrix
  bun run harness --dry-run [--scenarios …]     validate scenario files against the proto catalog
  bun run harness describe <Method|Message>…    print proto definitions for scenario authors
  bun run harness list                          list scenarios
  bun test                                      harness self-tests (fake server, fake agent)

Options:
  --server <url>          bee server base URL (env BEE_HARNESS_SERVER), e.g. http://127.0.0.1:7420
  --admin-token-env <V>   env var holding the setup/assertion token (default BEE_HARNESS_ADMIN_TOKEN);
                          never passed to agents
  --runtimes <list>       claude-code,codex (default) or command:<path> for a scripted agent
  --runs <n>              runs per scenario per runtime (default 3; the pass bar is 3)
  --scenarios <list|all>  AS-CORE-01,AS-CR-02,… or all (default all)
  --parallel <n>          concurrent runs (default 1)
  --bee-bin <path>      the bee CLI given to agents (default: bee on PATH, else target/{release,debug}/bee)
  --api-reference <path>  API reference exposed as BEE_API_REFERENCE (default schemas/openapi/bee.v1.json)
  --fixture <name=value>  a pre-provisioned fixture, satisfies "requires: fixture:<name>" (repeatable)
  --runs-dir <dir>        transcripts and results (default harness/agents/runs, gitignored)
  --model <id>            model override passed to the runtime
  --claude-bin <path>     default: claude          --codex-bin <path>   default: codex
  --claude-arg <arg>      extra claude argument (repeatable)    --codex-arg <arg>  extra codex argument (repeatable)
  --no-isolate            use the user's runtime config homes (default isolates: no user instructions)
  --timeout-scale <x>     multiply every scenario timeout (default 1)
  --keep-workdirs         keep agent working directories after each run
  -h, --help              this text

Outputs runs/<timestamp>/results.json, runs/<timestamp>/summary.md and per-run transcripts in
runs/<timestamp>/<scenario>/<runtime>/<n>/ (debugging only; pass/fail never reads them).`;

interface Args {
  [k: string]: string | string[] | boolean | undefined;
}

const REPEATABLE = new Set(["fixture", "claude-arg", "codex-arg"]);
const FLAGS = new Set(["dry-run", "help", "no-isolate", "keep-workdirs"]);

export function parseArgs(argv: string[]): { args: Args; positional: string[] } {
  const args: Args = {};
  const positional: string[] = [];
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i]!;
    if (a === "-h") {
      args.help = true;
      continue;
    }
    if (!a.startsWith("--")) {
      positional.push(a);
      continue;
    }
    let key = a.slice(2);
    let val: string | undefined;
    const eq = key.indexOf("=");
    if (eq >= 0) {
      val = key.slice(eq + 1);
      key = key.slice(0, eq);
    }
    if (FLAGS.has(key)) {
      args[key] = true;
      continue;
    }
    if (val === undefined) {
      val = argv[++i];
      if (val === undefined) throw new Error(`--${key} needs a value`);
    }
    if (REPEATABLE.has(key)) ((args[key] ??= []) as string[]).push(val);
    else args[key] = val;
  }
  return { args, positional };
}

function str(a: Args, k: string): string | undefined {
  const v = a[k];
  return typeof v === "string" ? v : undefined;
}

function list(a: Args, k: string): string[] {
  const v = a[k];
  return Array.isArray(v) ? v : [];
}

function findBee(root: string): string | undefined {
  const onPath = Bun.which("bee");
  if (onPath) return onPath;
  for (const p of ["target/release/bee", "target/debug/bee"]) if (existsSync(join(root, p))) return join(root, p);
  return undefined;
}

export function dryRun(ids: string[] | "all", log: (s: string) => void = console.log): number {
  const root = repoRoot();
  const catalog = Catalog.load(root);
  const fixtures = loadFixtures(join(HERE, "fixtures"));
  const product = productScenarios(join(root, "docs", "PRODUCT.md"));
  const reasons = errorReasons(catalog.protoDir);
  const files = scenarioFiles(join(HERE, "scenarios"));
  const selected = ids === "all" ? files : files.filter((f) => ids.includes(f.split("/").pop()!.replace(/\.ya?ml$/, "")));
  let bad = 0;
  if (ids !== "all") {
    for (const id of ids) if (!selected.some((f) => f.endsWith(`/${id}.yaml`))) {
      log(`MISSING ${id}: no scenarios/${id}.yaml`);
      bad++;
    }
  }
  const seen = new Set<string>();
  let calls = 0;
  for (const f of selected) {
    let s: Scenario;
    try {
      s = loadYaml<Scenario>(f);
    } catch (e) {
      log(`INVALID ${f}: ${(e as Error).message}`);
      bad++;
      continue;
    }
    const errors = validateScenario(s, { catalog, reasons, fixtures, product, file: f });
    seen.add(s.id);
    const steps = [...(s.fixtures ?? []).flatMap((x) => fixtures.get(x)?.setup ?? []), ...(s.setup ?? []), ...(s.assertions ?? [])];
    calls += steps.filter((x) => "call" in x).length;
    if (errors.length) {
      bad++;
      log(`INVALID ${s.id}`);
      for (const e of errors) log(`  - ${e}`);
    } else {
      log(`ok      ${s.id}  ${s.area.padEnd(5)} ${s.surfaces.padEnd(4)} ${String(s.assertions.length).padStart(2)} assertions  ${s.title}`);
    }
  }
  if (ids === "all") {
    for (const id of product.keys()) if (!seen.has(id)) {
      log(`MISSING ${id}: PRODUCT §4 lists it but scenarios/${id}.yaml does not exist`);
      bad++;
    }
  }
  log(`${selected.length} scenario file(s), ${product.size} in PRODUCT §4, ${calls} RPC steps checked against ${catalog.methods.size} RPCs; ${bad} problem(s)`);
  return bad === 0 ? 0 : 1;
}

export async function main(argv: string[]): Promise<number> {
  const { args, positional } = parseArgs(argv);
  if (args.help) {
    console.log(HELP);
    return 0;
  }
  const root = repoRoot();
  const ids = (str(args, "scenarios") ?? "all") === "all" ? ("all" as const) : str(args, "scenarios")!.split(",").map((s) => s.trim()).filter(Boolean);
  if (positional[0] === "describe") {
    console.log(describe(Catalog.load(root), positional.slice(1)));
    return 0;
  }
  if (positional[0] === "list") {
    for (const s of loadScenarios(join(HERE, "scenarios"), ids)) console.log(`${s.id}\t${s.area}\t${s.surfaces}\t${s.title}`);
    return 0;
  }
  if (positional.length) throw new Error(`unknown command ${positional[0]}; see --help`);
  if (args["dry-run"]) return dryRun(ids);

  const server = str(args, "server") ?? process.env.BEE_HARNESS_SERVER;
  if (!server) throw new Error("--server <url> is required (or --dry-run); see --help");
  const tokenVar = str(args, "admin-token-env") ?? "BEE_HARNESS_ADMIN_TOKEN";
  const adminToken = process.env[tokenVar];
  if (!adminToken) console.error(`warning: ${tokenVar} is not set; setup calls go unauthenticated`);
  const scenarios = loadScenarios(join(HERE, "scenarios"), ids);
  const catalog = Catalog.load(root);
  const fixtures = loadFixtures(join(HERE, "fixtures"));
  if (dryRun(ids, () => {}) !== 0) throw new Error("scenario files are invalid; run --dry-run");
  const namedFixtures: Record<string, string> = {};
  for (const f of list(args, "fixture")) {
    const eq = f.indexOf("=");
    if (eq < 1) throw new Error(`--fixture wants name=value, got ${f}`);
    namedFixtures[f.slice(0, eq)] = f.slice(eq + 1);
  }
  const env: HarnessEnv = {
    beeBin: str(args, "bee-bin") ? resolve(str(args, "bee-bin")!) : findBee(root),
    apiReference: resolve(str(args, "api-reference") ?? join(root, "schemas", "openapi", "bee.v1.json")),
    isolate: !args["no-isolate"],
    claudeBin: str(args, "claude-bin") ?? "claude",
    codexBin: str(args, "codex-bin") ?? "codex",
    claudeArgs: list(args, "claude-arg"),
    codexArgs: list(args, "codex-arg"),
    model: str(args, "model"),
  };
  const runtimes = (str(args, "runtimes") ?? "claude-code,codex").split(",").map((r) => runtimeById(r.trim()));
  const runs = Number(str(args, "runs") ?? 3);
  const parallel = Number(str(args, "parallel") ?? 1);
  if (!(runs >= 1 && parallel >= 1)) throw new Error("--runs and --parallel must be >= 1");
  const runsDir = resolve(str(args, "runs-dir") ?? join(HERE, "runs"));
  mkdirSync(runsDir, { recursive: true });
  const results = await orchestrate({
    server,
    adminToken,
    scenarios,
    fixtures,
    runtimes,
    runs,
    parallel,
    catalog,
    env,
    runsDir,
    namedFixtures,
    keepWorkdirs: Boolean(args["keep-workdirs"]),
    timeoutScale: Number(str(args, "timeout-scale") ?? 1),
    log: (l) => console.error(l),
  });
  const md = summary(results, scenarios);
  const dir = results.dir;
  writeFileSync(join(dir, "summary.md"), md);
  console.log(md);
  console.log(`results: ${join(dir, "results.json")}\nsummary: ${join(dir, "summary.md")}`);
  return results.ok ? 0 : 1;
}

if (import.meta.main) {
  main(process.argv.slice(2)).then(
    (code) => process.exit(code),
    (e: unknown) => {
      console.error(`error: ${(e as Error).message}`);
      process.exit(2);
    },
  );
}
