// Agent runtimes. Each run spawns one fresh agent process in an empty temporary directory with
// nothing but the task prompt, BEE_ENDPOINT/BEE_TOKEN, and the product's own surfaces: the
// `bee` CLI on PATH (surfaces cli|both) and/or `bee mcp` registered as an MCP server
// (surfaces mcp|both). Transcripts are written for debugging only; pass/fail never reads them.

import { existsSync, mkdirSync, writeFileSync, symlinkSync, openSync, closeSync, mkdtempSync } from "node:fs";
import { homedir, tmpdir } from "node:os";
import { dirname, join } from "node:path";
import type { Surface } from "./scenario";

export interface RuntimeInfo {
  id: string;
  available: boolean;
  version?: string;
  path?: string;
  problem?: string;
}

export interface AgentLaunch {
  /** For transcripts: "agent" or a peer name. */
  name: string;
  prompt: string;
  workdir: string;
  transcriptDir: string;
  endpoint: string;
  token: string;
  surfaces: Surface;
  timeoutMs: number;
  extraEnv: Record<string, string>;
  maxTurns?: number;
  maxUsd?: number;
}

export interface AgentExit {
  name: string;
  exitCode: number | null;
  timedOut: boolean;
  durationMs: number;
  signal?: string;
}

export interface HarnessEnv {
  /** Absolute path of the `bee` binary, when one exists. */
  beeBin?: string;
  /** Path of the API reference handed to agents (BEE_API_REFERENCE). */
  apiReference?: string;
  /** Isolate runtime config homes so no user-level instructions leak in. */
  isolate: boolean;
  claudeBin: string;
  codexBin: string;
  claudeArgs: string[];
  codexArgs: string[];
  model?: string;
}

export interface Runtime {
  id: string;
  detect(env: HarnessEnv): Promise<RuntimeInfo>;
  /** argv plus environment additions, given a prepared launch. */
  command(launch: AgentLaunch, env: HarnessEnv, mcp: McpServer | null): { argv: string[]; env: Record<string, string> };
}

export interface McpServer {
  command: string;
  args: string[];
  env: Record<string, string>;
}

async function probe(argv: string[]): Promise<{ ok: boolean; out: string }> {
  try {
    const p = Bun.spawn(argv, { stdout: "pipe", stderr: "pipe", stdin: "ignore" });
    const timer = setTimeout(() => p.kill(), 15_000);
    const [out, err, code] = await Promise.all([new Response(p.stdout).text(), new Response(p.stderr).text(), p.exited]);
    clearTimeout(timer);
    return { ok: code === 0, out: (out || err).trim() };
  } catch (e) {
    return { ok: false, out: (e as Error).message };
  }
}

function which(bin: string): string | undefined {
  if (bin.includes("/")) return existsSync(bin) ? bin : undefined;
  return Bun.which(bin) ?? undefined;
}

async function detectBinary(id: string, bin: string): Promise<RuntimeInfo> {
  const path = which(bin);
  if (!path) return { id, available: false, problem: `${bin} not found on PATH` };
  const v = await probe([path, "--version"]);
  if (!v.ok) return { id, available: false, path, problem: `\`${bin} --version\` failed: ${v.out.split("\n").slice(-3).join(" ").slice(0, 300)}` };
  return { id, available: true, path, version: v.out.split("\n")[0] };
}

/** A copy of the harness environment without anything the agent must not see. */
export function agentBaseEnv(): Record<string, string> {
  const out: Record<string, string> = {};
  for (const [k, v] of Object.entries(process.env)) {
    if (v === undefined) continue;
    if (k.startsWith("BEE_")) continue; // includes BEE_HARNESS_ADMIN_TOKEN
    if (k === "CLAUDECODE" || k.startsWith("CLAUDE_CODE_") || k === "CODEX_HOME") continue;
    out[k] = v;
  }
  return out;
}

export const claudeCode: Runtime = {
  id: "claude-code",
  detect: (env) => detectBinary("claude-code", env.claudeBin),
  command(l, env, mcp) {
    const cfgPath = join(l.transcriptDir, `${l.name}.mcp.json`);
    writeFileSync(cfgPath, JSON.stringify({ mcpServers: mcp ? { bee: { type: "stdio", ...mcp } } : {} }, null, 2));
    const argv = [
      env.claudeBin,
      "-p",
      l.prompt,
      "--output-format",
      "stream-json",
      "--verbose",
      "--strict-mcp-config",
      "--mcp-config",
      cfgPath,
      "--dangerously-skip-permissions",
    ];
    if (l.maxTurns) argv.push("--max-turns", String(l.maxTurns));
    if (env.model) argv.push("--model", env.model);
    argv.push(...env.claudeArgs);
    const extra: Record<string, string> = { DISABLE_AUTOUPDATER: "1", DISABLE_TELEMETRY: "1" };
    if (env.isolate) {
      // A fresh config dir: no user CLAUDE.md, settings, hooks or MCP servers. Needs an API key.
      const dir = join(l.transcriptDir, `${l.name}.claude-config`);
      mkdirSync(dir, { recursive: true });
      extra.CLAUDE_CONFIG_DIR = dir;
    }
    return { argv, env: extra };
  },
};

function toml(v: string): string {
  return JSON.stringify(v);
}

export const codex: Runtime = {
  id: "codex",
  detect: (env) => detectBinary("codex", env.codexBin),
  command(l, env, mcp) {
    const argv = [
      env.codexBin,
      "exec",
      "--json",
      "--skip-git-repo-check",
      "--ephemeral",
      "--dangerously-bypass-approvals-and-sandbox",
      "-C",
      l.workdir,
      "-o",
      join(l.transcriptDir, `${l.name}.last-message.txt`),
      "-c",
      "shell_environment_policy.inherit=all",
    ];
    if (mcp) {
      argv.push("-c", `mcp_servers.bee.command=${toml(mcp.command)}`);
      argv.push("-c", `mcp_servers.bee.args=[${mcp.args.map(toml).join(",")}]`);
      const envToml = Object.entries(mcp.env).map(([k, v]) => `${k}=${toml(v)}`).join(",");
      argv.push("-c", `mcp_servers.bee.env={${envToml}}`);
    }
    if (env.model) argv.push("-m", env.model);
    argv.push(...env.codexArgs, l.prompt);
    const extra: Record<string, string> = {};
    if (env.isolate) {
      // A fresh CODEX_HOME: no user AGENTS.md, config or rules; credentials linked in.
      const home = join(l.transcriptDir, `${l.name}.codex-home`);
      mkdirSync(home, { recursive: true });
      const auth = join(process.env.CODEX_HOME ?? join(homedir(), ".codex"), "auth.json");
      if (existsSync(auth) && !existsSync(join(home, "auth.json"))) symlinkSync(auth, join(home, "auth.json"));
      extra.CODEX_HOME = home;
    }
    return { argv, env: extra };
  },
};

/**
 * `command:<path>`: any executable, called as `<path> <prompt>` in the work directory with the
 * same environment. The self-tests drive the harness with a scripted fake agent this way, and
 * other agent runtimes can be wired in without code changes.
 */
export function commandRuntime(path: string): Runtime {
  return {
    id: `command:${path}`,
    async detect() {
      const p = which(path);
      return p ? { id: `command:${path}`, available: true, path: p, version: "script" } : { id: `command:${path}`, available: false, problem: `${path} not found` };
    },
    command(l) {
      return { argv: [path, l.prompt], env: {} };
    },
  };
}

export function runtimeById(id: string): Runtime {
  if (id === "claude-code") return claudeCode;
  if (id === "codex") return codex;
  if (id.startsWith("command:")) return commandRuntime(id.slice("command:".length));
  throw new Error(`unknown runtime ${id} (claude-code, codex, command:<path>)`);
}

/** Spawns one agent, enforcing the timeout (SIGTERM, then SIGKILL after 10 s). */
export async function launch(runtime: Runtime, l: AgentLaunch, env: HarnessEnv): Promise<AgentExit> {
  mkdirSync(l.workdir, { recursive: true });
  mkdirSync(l.transcriptDir, { recursive: true });
  const useCli = l.surfaces !== "mcp";
  const useMcp = l.surfaces !== "cli";
  const base = agentBaseEnv();
  const beeEnv: Record<string, string> = { BEE_ENDPOINT: l.endpoint, BEE_TOKEN: l.token };
  if (env.apiReference) beeEnv.BEE_API_REFERENCE = env.apiReference;
  let path = base.PATH ?? "/usr/bin:/bin";
  if (useCli && env.beeBin) path = `${dirname(env.beeBin)}:${path}`;
  const mcp: McpServer | null = useMcp && env.beeBin ? { command: env.beeBin, args: ["mcp"], env: beeEnv } : null;
  const { argv, env: extra } = runtime.command(l, env, mcp);
  writeFileSync(
    join(l.transcriptDir, `${l.name}.launch.json`),
    JSON.stringify({ argv: argv.map((a) => (a === l.prompt ? "<prompt>" : a)), cwd: l.workdir, surfaces: l.surfaces, mcp: Boolean(mcp) }, null, 2),
  );
  writeFileSync(join(l.transcriptDir, `${l.name}.prompt.txt`), l.prompt);
  const outFd = openSync(join(l.transcriptDir, `${l.name}.stdout.jsonl`), "w");
  const errFd = openSync(join(l.transcriptDir, `${l.name}.stderr.txt`), "w");
  const t0 = performance.now();
  let timedOut = false;
  try {
    const proc = Bun.spawn(argv, {
      cwd: l.workdir,
      env: { ...base, ...l.extraEnv, ...beeEnv, ...extra, PATH: path, HARNESS_WORKDIR: l.workdir },
      stdin: "ignore",
      stdout: outFd,
      stderr: errFd,
    });
    const term = setTimeout(() => {
      timedOut = true;
      proc.kill("SIGTERM");
      setTimeout(() => proc.kill("SIGKILL"), 10_000).unref();
    }, l.timeoutMs);
    const code = await proc.exited;
    clearTimeout(term);
    return {
      name: l.name,
      exitCode: timedOut ? null : code,
      timedOut,
      durationMs: performance.now() - t0,
      signal: proc.signalCode === null || proc.signalCode === undefined ? undefined : String(proc.signalCode),
    };
  } finally {
    closeSync(outFd);
    closeSync(errFd);
  }
}

export function freshWorkdir(): string {
  return mkdtempSync(join(tmpdir(), "bee-agent-"));
}
