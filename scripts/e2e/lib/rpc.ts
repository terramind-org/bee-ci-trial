// Shared plumbing for the phase 1 exit test steps: logging, the step state file, a throwing
// Connect (HTTP/JSON) client on top of the agent harness's, and small helpers for the wire
// shapes the steps build by hand. No LLM is involved anywhere in these scripts.

import { createHash } from "node:crypto";
import { readFileSync, writeFileSync, existsSync } from "node:fs";
import { join } from "node:path";
import { ConnectClient, type CallOptions, type RpcError } from "../../../harness/agents/src/connect";
import { blobId } from "../../../harness/agents/src/blake3";

export { blobId };

/** Exit status a step uses when it ran but could not cover everything (documented offline mode). */
export const EXIT_INCOMPLETE = 3;

/** What the orchestrator (`exit-test.sh`) hands every step. */
export interface Env {
  /** Scratch directory of this run (server data, clones, logs, state.json). */
  work: string;
  endpoint: string;
  /** The dev server's root token (user `dev`). */
  adminToken: string;
}

export function env(): Env {
  const need = (name: string): string => {
    const v = process.env[name];
    if (!v) fail(`${name} is not set; run steps through scripts/e2e/exit-test.sh`);
    return v;
  };
  return { work: need("EXIT_TEST_WORK"), endpoint: need("EXIT_TEST_ENDPOINT"), adminToken: need("EXIT_TEST_ADMIN_TOKEN") };
}

export function log(msg: string): void {
  console.log(msg);
}

export function ok(msg: string): void {
  console.log(`  ok   ${msg}`);
}

/** A failed assertion: the step ends with a non-zero status and this message. */
export class StepFailure extends Error {}

export function fail(msg: string): never {
  throw new StepFailure(msg);
}

export function assert(cond: unknown, msg: string): asserts cond {
  if (!cond) fail(msg);
  ok(msg);
}

/** Runs a step body: a StepFailure prints `FAIL: ...` and exits 1; anything else is a crash, also 1. */
export async function runStep(name: string, body: () => Promise<void | "incomplete">): Promise<never> {
  try {
    const r = await body();
    process.exit(r === "incomplete" ? EXIT_INCOMPLETE : 0);
  } catch (e) {
    if (e instanceof StepFailure) console.error(`FAIL (${name}): ${redact(e.message)}`);
    else console.error(`CRASH (${name}): ${redact(e instanceof Error ? e.stack ?? e.message : String(e))}`);
    process.exit(1);
  }
}

// ---------------------------------------------------------------------------------------------
// State shared between steps (one JSON file in the work directory).

export interface State {
  /** Repo the steps work on, `org/name`. */
  repo?: string;
  repoId?: string;
  /** The agent that makes the MCP change (step 2). */
  mcpAgent?: { id: string; token: string };
  /** The human principal that pushes with git (step 3). */
  pusher?: { id: string; token: string };
  /** Ruleset id protecting the default branch. */
  rulesetId?: string;
  defaultBranch?: string;
  /** Step 1. */
  import?: { id: string; commits: number; gitIds: string[]; extra: number; source: string };
  /** Step 2. */
  mcpChange?: { id: string; path: string; lines: string[]; session: string };
  /** Step 3. */
  gitChange?: { commit: string; path: string; addedLine: string; secretRejected: boolean };
}

export function readState(work: string): State {
  const p = join(work, "state.json");
  return existsSync(p) ? (JSON.parse(readFileSync(p, "utf8")) as State) : {};
}

export function writeState(work: string, patch: Partial<State>): State {
  const next = { ...readState(work), ...patch };
  writeFileSync(join(work, "state.json"), JSON.stringify(next, null, 2));
  return next;
}

// ---------------------------------------------------------------------------------------------
// Connect client.

export class RpcFailure extends Error {
  constructor(
    readonly method: string,
    readonly error: RpcError,
  ) {
    super(`${method}: ${error.reason} ${error.message}`);
  }
}

export class Api {
  private readonly client: ConnectClient;
  constructor(
    readonly endpoint: string,
    readonly token: string,
  ) {
    this.client = new ConnectClient(endpoint, token);
  }

  /** Unary call; throws RpcFailure on a SPEC §12.2 error. */
  async call<T = any>(service: string, method: string, body: unknown = {}, opts: CallOptions = {}): Promise<T> {
    const r = await this.client.unary(service, method, body, opts);
    if (!r.ok) throw new RpcFailure(`${service}/${method}`, r.error!);
    return r.body as T;
  }

  /** Like `call`, but returns the error instead of throwing it. */
  async try(service: string, method: string, body: unknown = {}, opts: CallOptions = {}) {
    const r = await this.client.unary(service, method, body, opts);
    return r.ok ? { ok: true as const, body: r.body as any } : { ok: false as const, error: r.error! };
  }

  /** Server-streaming call: every message. */
  async stream<T = any>(service: string, method: string, body: unknown = {}, opts: CallOptions = {}): Promise<T[]> {
    const r = await this.client.serverStream(service, method, body, { timeoutMs: 120_000, ...opts });
    if (!r.ok) throw new RpcFailure(`${service}/${method}`, r.error!);
    return (r.body as { messages: T[] }).messages;
  }

  /** Client-streaming call. */
  async upload<T = any>(service: string, method: string, items: unknown[], opts: CallOptions = {}): Promise<T> {
    const r = await this.client.clientStream(service, method, items, opts);
    if (!r.ok) throw new RpcFailure(`${service}/${method}`, r.error!);
    return r.body as T;
  }
}

// ---------------------------------------------------------------------------------------------
// Wire helpers.

export const b64 = (data: string | Uint8Array): string => Buffer.from(data).toString("base64");
export const unb64 = (s: string | undefined): string => Buffer.from(s ?? "", "base64").toString("utf8");

/** Lowercase hex of a base64 `raw` id. */
export const rawHex = (id: { raw?: string } | undefined): string => Buffer.from(id?.raw ?? "", "base64").toString("hex");

/** A ChangeId (16 bytes) that is a pure function of a label, so reruns build the same ids. */
export function changeId(label: string): { raw: string } {
  return { raw: createHash("sha256").update(`exit-test\0${label}`).digest().subarray(0, 16).toString("base64") };
}

/** Upload items for `UploadObjects` plus the matching `put` delta entry of one text file. */
export function putFile(repo: { fullName: string }, path: string, text: string, first: boolean) {
  const bytes = new TextEncoder().encode(text);
  const id = { raw: b64(blobId(bytes)) };
  return {
    upload: { ...(first ? { repo } : {}), id, offset: "0", data: b64(bytes), last: true },
    delta: { path: b64(path), put: { mode: "MODE_FILE", blob: id, size: String(bytes.length) } },
  };
}

/** Reads a file at a revision through QueryService.ReadPath. */
export async function readFile(api: Api, repo: string, expr: string, path: string): Promise<string> {
  const msgs = await api.stream("QueryService", "ReadPath", { repo: { fullName: repo }, at: { expr }, path: b64(path) });
  return msgs.map((m) => Buffer.from(m.data ?? "", "base64").toString("utf8")).join("");
}

/** Runs a command, returning stdout; a non-zero exit is a failure carrying stderr. */
export async function sh(cmd: string[], opts: { cwd?: string; env?: Record<string, string>; stdin?: string; allowFail?: boolean } = {}) {
  const proc = Bun.spawn(cmd, {
    cwd: opts.cwd,
    env: { ...process.env, ...opts.env },
    stdin: opts.stdin !== undefined ? new TextEncoder().encode(opts.stdin) : "ignore",
    stdout: "pipe",
    stderr: "pipe",
  });
  const [stdout, stderr, code] = await Promise.all([new Response(proc.stdout).text(), new Response(proc.stderr).text(), proc.exited]);
  if (code !== 0 && !opts.allowFail) fail(`${cmd[0]} ${cmd[1] ?? ""} exited ${code}: ${redact(stderr || stdout).trim().slice(-2000)}`);
  return { stdout, stderr, code };
}

const secrets = new Set<string>();

/** Remember an opaque token so even unexpected error messages cannot print it. */
export function registerSecret(value: string): string {
  if (value) secrets.add(value);
  return value;
}

/** Removes known secrets and recognizable credentials from text bound for a log. */
export function redact(text: string): string {
  for (const value of [process.env.EXIT_TEST_ADMIN_TOKEN, process.env.BEE_TOKEN, process.env.GITHUB_TOKEN, process.env.GH_TOKEN, ...secrets]) {
    if (value) text = text.replaceAll(value, "***");
  }
  return text
    .replace(/gh[pousr]_[A-Za-z0-9]{20,}/g, "***")
    .replace(/github_pat_[A-Za-z0-9_]{20,}/g, "***")
    .replace(/(Authorization: *(?:Basic|Bearer) +)\S+/gi, "$1***")
    .replace(/(token|password)=\S+/gi, "$1=***");
}

/** Creates an agent principal in the repo's org and a token scoped to the repo. */
export async function mintAgent(admin: Api, org: string, repo: string, label: string, session: string) {
  const agent = await admin.call("AuthService", "CreateAgent", { display: label, runtime: "exit-test", purpose: "phase 1 exit test", org });
  const minted = await admin.call("AuthService", "MintToken", {
    principalId: agent.id,
    scope: { repos: [repo], actions: ["read", "commit", "land", "review", "lease"], session },
  });
  return { id: agent.id as string, token: registerSecret(minted.token as string) };
}

