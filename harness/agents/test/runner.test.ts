// The orchestrator end to end: a scripted fake agent (command: runtime) against the fake
// server, proving pass/fail detection, timeouts, 3/3 aggregation, blocking and isolation.

import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { chmodSync, existsSync, mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { Catalog } from "../src/catalog";
import { orchestrate, type OrchestrateOptions, type Results } from "../src/orchestrate";
import { summary } from "../src/report";
import { codex, claudeCode, commandRuntime, type HarnessEnv } from "../src/runners";
import { loadFixtures, loadYaml, validateScenario, errorReasons, type Scenario } from "../src/scenario";
import { startFakeServer, type FakeServer } from "./fake-server";

const HERE = resolve(import.meta.dir, "..");
const AGENT = join(import.meta.dir, "fake-agent.ts");
const catalog = Catalog.load();
const fixtures = loadFixtures(join(HERE, "fixtures"));
let server: FakeServer;
let runsDir: string;
const ADAPTER_DIR = mkdtempSync(join(tmpdir(), "harness-adapter-"));

beforeAll(() => {
  chmodSync(AGENT, 0o755);
  // The fake agent is a bun script; make sure the agent's PATH finds bun.
  process.env.PATH = `${dirname(process.execPath)}:${process.env.PATH ?? ""}`;
  process.env.BEE_HARNESS_ADMIN_TOKEN = "must-not-leak";
  server = startFakeServer();
  runsDir = mkdtempSync(join(tmpdir(), "harness-runs-"));
});
afterAll(() => {
  server.stop();
  rmSync(runsDir, { recursive: true, force: true });
  rmSync(ADAPTER_DIR, { recursive: true, force: true });
});

function scenario(mode: string, patch: Partial<Scenario> = {}): Scenario {
  const s = loadYaml<Scenario>(join(import.meta.dir, "scenarios", "AS-TEST-01.yaml"));
  s.agent.env = { FAKE_AGENT_MODE: mode, FAKE_AGENT_COUNTER: join(runsDir, `counter-${mode}-${Math.random()}`) };
  return { ...s, ...patch };
}

const env: HarnessEnv = { beeBin: AGENT, isolate: false, claudeBin: "claude", codexBin: "codex", claudeArgs: [], codexArgs: [] };

function options(s: Scenario, extra: Partial<OrchestrateOptions> = {}): OrchestrateOptions {
  return {
    server: server.url,
    adminToken: server.adminToken,
    scenarios: [s],
    fixtures,
    runtimes: [commandRuntime(AGENT)],
    runs: 3,
    parallel: 1,
    catalog,
    env,
    runsDir,
    namedFixtures: {},
    keepWorkdirs: false,
    timeoutScale: 1,
    log: () => {},
    ...extra,
  };
}

describe("self-test scenario", () => {
  test("validates against the real proto catalog", () => {
    const s = scenario("good");
    expect(validateScenario(s, { catalog, reasons: errorReasons(catalog.protoDir), fixtures })).toEqual([]);
  });
});

describe("orchestration", () => {
  test("a correct agent passes 3/3 and the matrix is ok", async () => {
    const r = await orchestrate(options(scenario("good")));
    expect(r.cells).toEqual([{ scenario: "AS-TEST-01", runtime: `command:${AGENT}`, passes: 3, runs: 3, statuses: ["pass", "pass", "pass"] }]);
    expect(r.ok).toBe(true);
    // Each run had its own namespace and session.
    expect(new Set(r.runs.map((x) => x.ns)).size).toBe(3);
    expect(new Set(r.runs.map((x) => x.session)).size).toBe(3);
    expect(existsSync(join(r.dir, "results.json"))).toBe(true);
    expect(existsSync(join(r.runs[0]!.transcriptDir, "agent.stdout.jsonl"))).toBe(true);
    expect(existsSync(join(r.runs[0]!.transcriptDir, "result.json"))).toBe(true);
  }, 60_000);

  test("an agent that does nothing fails every run, from assertions alone", async () => {
    const r = await orchestrate(options(scenario("noop")));
    expect(r.cells[0]!.passes).toBe(0);
    expect(r.cells[0]!.statuses).toEqual(["fail", "fail", "fail"]);
    expect(r.ok).toBe(false);
    expect(r.runs[0]!.reason).toContain("main moved past the seed");
  }, 60_000);

  test("2/3 is not a pass", async () => {
    const r = await orchestrate(options(scenario("flaky")));
    expect(r.cells[0]!.passes).toBe(2);
    expect(r.cells[0]!.statuses).toEqual(["pass", "pass", "fail"]);
    expect(r.ok).toBe(false);
    const md = summary(r, [scenario("flaky")]);
    expect(md).toContain("| AS-TEST-01 | 4.25 |");
    expect(md).toContain("2/3 FAIL");
    expect(md).toContain("Result: FAIL");
  }, 60_000);

  test("an agent past its timeout is killed and the run is a timeout", async () => {
    const t0 = Date.now();
    const r = await orchestrate(options(scenario("slow", { timeout: 1 }), { runs: 1 }));
    expect(Date.now() - t0).toBeLessThan(20_000);
    expect(r.runs[0]!.agents[0]!.timedOut).toBe(true);
    expect(r.runs[0]!.status).toBe("timeout");
    expect(r.ok).toBe(false);
  }, 30_000);

  test("parallel runs stay independent", async () => {
    const r = await orchestrate(options(scenario("good"), { parallel: 3 }));
    expect(r.cells[0]!.passes).toBe(3);
  }, 60_000);

  test("agents never see the harness admin token", async () => {
    const r = await orchestrate(options(scenario("leak"), { runs: 1, keepWorkdirs: true }));
    expect(r.runs[0]!.status).toBe("pass");
    const launch = JSON.parse(readFileSync(join(r.runs[0]!.transcriptDir, "agent.launch.json"), "utf8"));
    expect(launch.argv).toContain("<prompt>");
    // The workdir was kept: read the environment the agent saw.
    const prompt = readFileSync(join(r.runs[0]!.transcriptDir, "agent.prompt.txt"), "utf8");
    expect(prompt).toContain("src/billing/tax.py");
    const seen = Bun.spawnSync(["sh", "-c", `grep -l must-not-leak ${tmpdir()}/bee-agent-*/env.json 2>/dev/null || true`]).stdout.toString();
    expect(seen.trim()).toBe("");
    const envs = Bun.spawnSync(["sh", "-c", `ls ${tmpdir()}/bee-agent-*/env.json 2>/dev/null | head -1`]).stdout.toString().trim();
    expect(envs).not.toBe("");
    const agentEnv = JSON.parse(readFileSync(envs, "utf8"));
    expect(agentEnv.BEE_ENDPOINT).toBe(server.url);
    expect(agentEnv.BEE_TOKEN).toStartWith("tok-");
    expect(agentEnv.BEE_HARNESS_ADMIN_TOKEN).toBeUndefined();
    Bun.spawnSync(["sh", "-c", `rm -rf ${tmpdir()}/bee-agent-*`]);
  }, 30_000);

  test("unmet host requirements block the run instead of passing or failing it", async () => {
    const r = await orchestrate(options(scenario("good", { requires: ["bin:definitely-not-installed-xyz"] }), { runs: 1 }));
    expect(r.runs[0]!.status).toBe("blocked");
    expect(r.runs[0]!.reason).toContain("definitely-not-installed-xyz");
    expect(r.ok).toBe(false);
  });

  test("an unavailable runtime blocks its cells and is reported", async () => {
    const r: Results = await orchestrate(options(scenario("good"), { runtimes: [commandRuntime("/nonexistent/agent")], runs: 1 }));
    expect(r.runtimes[0]!.available).toBe(false);
    expect(r.runs[0]!.status).toBe("blocked");
    expect(summary(r, [scenario("good")])).toContain("unavailable");
  });

  test("setup failures are reported as setup errors", async () => {
    const s = scenario("good");
    s.setup = [{ call: "ChangeService/ListChanges", request: {} }];
    const r = await orchestrate(options(s, { runs: 1 }));
    expect(r.runs[0]!.status).toBe("setup-error");
    expect(r.runs[0]!.reason).toContain("UNIMPLEMENTED");
  });
});

describe("runtime adapters", () => {
  const launch = {
    name: "agent",
    prompt: "do it",
    workdir: "/tmp/w",
    transcriptDir: ADAPTER_DIR,
    endpoint: "http://h",
    token: "t",
    surfaces: "both" as const,
    timeoutMs: 1000,
    extraEnv: {},
    maxTurns: 7,
  };
  const mcp = { command: "/bin/bee", args: ["mcp"], env: { BEE_ENDPOINT: "http://h", BEE_TOKEN: "t" } };
  test("claude-code runs headless with stream-json and only the bee MCP server", () => {
    const { argv, env: extra } = claudeCode.command(launch, { ...env, isolate: true }, mcp);
    expect(argv.slice(0, 3)).toEqual(["claude", "-p", "do it"]);
    expect(argv).toContain("stream-json");
    expect(argv).toContain("--strict-mcp-config");
    expect(argv.join(" ")).toContain("--max-turns 7");
    const cfg = JSON.parse(readFileSync(argv[argv.indexOf("--mcp-config") + 1]!, "utf8"));
    expect(Object.keys(cfg.mcpServers)).toEqual(["bee"]);
    expect(cfg.mcpServers.bee.args).toEqual(["mcp"]);
    expect(extra.CLAUDE_CONFIG_DIR).toBeDefined();
  });
  test("codex runs exec --json in the workdir with the bee MCP server", () => {
    const { argv, env: extra } = codex.command(launch, { ...env, isolate: true }, mcp);
    expect(argv.slice(0, 3)).toEqual(["codex", "exec", "--json"]);
    expect(argv.join(" ")).toContain("-C /tmp/w");
    expect(argv.join(" ")).toContain('mcp_servers.bee.command="/bin/bee"');
    expect(argv.at(-1)).toBe("do it");
    expect(extra.CODEX_HOME).toBeDefined();
  });
});
