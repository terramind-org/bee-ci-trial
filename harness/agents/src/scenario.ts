// Scenario files: types, loading and static validation (`--dry-run`).
//
// One YAML file per PRODUCT §4 agent scenario, in scenarios/<id>.yaml. Shared setup lives in
// fixtures/<name>.yaml (a `setup:` list) and is pulled in with `fixtures: [name]`.

import { readdirSync, readFileSync, existsSync } from "node:fs";
import { join, basename } from "node:path";
import { Catalog, checkPath, checkRequest, type Method } from "./catalog";
import { OPERATORS, operatorOf, parsePath, predicatePaths, regex, type Predicate } from "./predicate";
import { FUNCTIONS, expressions } from "./template";

export type Surface = "cli" | "mcp" | "both";

export interface CallStep {
  name?: string;
  /** "Service/Method" from proto/bee/v1. */
  call: string;
  /** Bearer token (template). Defaults to the harness admin token. */
  as?: string;
  /** Another endpoint serving the same RPC surface (template), e.g. a cache tier the agent started. */
  endpoint?: string;
  request?: unknown;
  /** Client-streaming calls: the items to send (the first usually carries `repo`). */
  items?: unknown[];
  /** Run the call once per element; `${item}` is the element, `${index}` its position. */
  each?: unknown[] | string;
  /** var name -> path in the response ("$" = whole response). With `each`, saves arrays. */
  save?: Record<string, string>;
  expect?: Predicate[];
  /** The call must fail with one of these SPEC §12.2 reasons. */
  expect_error?: { reason: string | string[]; expect?: Predicate[] };
  /** Poll until the expectations hold (eventual state such as auto-merge or bisect). */
  retry?: { attempts: number; interval_ms: number };
  /** Server-streaming calls: how much of the stream to read. */
  stream?: { max_messages?: number; timeout_ms?: number };
}

export interface CommitStep {
  name?: string;
  /** Upload blobs, CreateChangeset on top of `parent` (or the ref tip), then Land unless land: false. */
  commit: {
    repo: string;
    ref?: string;
    /** Revision of the parent; defaults to the tip of `ref`. */
    parent?: string;
    /** path -> content; null deletes the path. */
    files: Record<string, string | null>;
    /** Many generated files: {i} in pattern and content is replaced by 0..count-1. */
    generate?: { pattern: string; count: number; content: string };
    message: string;
    author?: string;
    as?: string;
    land?: boolean;
    /** Label for the ChangeId; the same label in one run gives the same ChangeId (amend). */
    change?: string;
    author_time?: string;
    provenance?: Record<string, unknown>;
    /** After creating, point this (non-landing) ref at the changeset, creating it if needed. */
    ref_name?: string;
    /** The commit must be refused with one of these reasons (at upload, create or land). */
    expect_error?: { reason: string | string[] };
  };
  /** var name for {id, changeId, ref}. */
  save?: string;
}

export interface AnswerStep {
  name?: string;
  /** A JSON file the agent was asked to write in its working directory. */
  answer: { file: string; expect: Predicate[] };
}

export interface WorkdirStep {
  name?: string;
  /** Files the agent left in its working directory, matched by a glob. */
  workdir: { glob: string; min?: number; max?: number };
}

export interface HttpStep {
  name?: string;
  /** An HTTP call outside the bee API (e.g. the GitHub REST API for migration checks). */
  http: { method?: string; url: string; headers?: Record<string, string>; body?: unknown };
  save?: Record<string, string>;
  expect?: Predicate[];
  retry?: { attempts: number; interval_ms: number };
}

export interface ExecStep {
  name?: string;
  /** A host command (argv, templated), e.g. a fault-injection hook. Must exit 0. */
  exec: string[];
  save_stdout?: string;
}

export interface SleepStep {
  name?: string;
  sleep_ms: number;
}

export type Step = CallStep | CommitStep | AnswerStep | WorkdirStep | HttpStep | ExecStep | SleepStep;

export interface DuringBlock {
  /** Start after this delay from agent launch... */
  after_ms?: number;
  /** ...or once the agent creates this file in its working directory. */
  wait_for_file?: string;
  /** Give up waiting after this long (default: the scenario timeout). */
  wait_timeout_s?: number;
  steps: Step[];
}

export interface AgentDef {
  /** Token the agent gets as BEE_TOKEN (template). */
  token: string;
  /** Extra environment (templated), e.g. GH_HOST for the gh scenario. */
  env?: Record<string, string>;
}

export interface PeerDef extends AgentDef {
  name: string;
  prompt: string;
  /** Start the peer this long after the primary agent. */
  delay_ms?: number;
}

export interface Scenario {
  id: string;
  area: string;
  title: string;
  surfaces: Surface;
  /** Seconds of agent wall-clock time per run. */
  timeout: number;
  budget?: { max_turns?: number; max_usd?: number };
  /** Host capabilities: `bin:<name>`, `env:<NAME>`, `fixture:<name>`. Unmet = blocked. */
  requires?: string[];
  fixtures?: string[];
  setup: Step[];
  agent: AgentDef;
  prompt: string;
  /** Further agents run concurrently with the same runtime (e.g. a reviewer). */
  peers?: PeerDef[];
  during?: DuringBlock[];
  assertions: Step[];
  teardown?: Step[];
  notes?: string;
}

export interface Fixture {
  name: string;
  setup: Step[];
}

export const isCall = (s: Step): s is CallStep => "call" in s;
export const isCommit = (s: Step): s is CommitStep => "commit" in s;
export const isAnswer = (s: Step): s is AnswerStep => "answer" in s;
export const isHttp = (s: Step): s is HttpStep => "http" in s;
export const isWorkdir = (s: Step): s is WorkdirStep => "workdir" in s;
export const isExec = (s: Step): s is ExecStep => "exec" in s;
export const isSleep = (s: Step): s is SleepStep => "sleep_ms" in s;

export function stepLabel(s: Step): string {
  if (s.name) return s.name;
  if (isCall(s)) return s.call;
  if (isCommit(s)) return `commit ${s.commit.message}`;
  if (isAnswer(s)) return `answer ${s.answer.file}`;
  if (isHttp(s)) return `http ${s.http.url}`;
  if (isWorkdir(s)) return `workdir ${s.workdir.glob}`;
  if (isExec(s)) return `exec ${s.exec[0]}`;
  return `sleep ${s.sleep_ms}ms`;
}

// ---------------------------------------------------------------------------------------------

export function loadYaml<T>(path: string): T {
  return Bun.YAML.parse(readFileSync(path, "utf8")) as T;
}

export function scenarioFiles(dir: string): string[] {
  return readdirSync(dir)
    .filter((f) => /^AS-[A-Z]+-\d+\.ya?ml$/.test(f))
    .sort()
    .map((f) => join(dir, f));
}

export function loadScenarios(dir: string, ids: string[] | "all"): Scenario[] {
  const all = scenarioFiles(dir).map((f) => loadYaml<Scenario>(f));
  if (ids === "all") return all;
  const byId = new Map(all.map((s) => [s.id, s]));
  return ids.map((id) => {
    const s = byId.get(id);
    if (!s) throw new Error(`no scenario ${id} in ${dir}`);
    return s;
  });
}

export function loadFixtures(dir: string): Map<string, Fixture> {
  const out = new Map<string, Fixture>();
  if (!existsSync(dir)) return out;
  for (const f of readdirSync(dir).filter((x) => /\.ya?ml$/.test(x))) {
    const fx = loadYaml<Fixture>(join(dir, f));
    out.set(basename(f).replace(/\.ya?ml$/, ""), fx);
  }
  return out;
}

/** Every `AS-…` id in docs/PRODUCT.md §4 with the area (section) that lists it. */
export function productScenarios(productMd: string): Map<string, string> {
  const out = new Map<string, string>();
  let area = "";
  for (const line of readFileSync(productMd, "utf8").split("\n")) {
    const h = /^### (4\.\d+) /.exec(line);
    if (h) area = h[1]!;
    if (/^## [^4]/.test(line) && area) area = "";
    const m = /^- `(AS-[A-Z]+-\d+)`/.exec(line);
    if (m && area) out.set(m[1]!, area);
  }
  return out;
}

/** The SPEC §12.2 reason names (ErrorReason values without their prefix). */
export function errorReasons(protoDir: string): Set<string> {
  const text = readFileSync(join(protoDir, "common.proto"), "utf8");
  const block = /enum ErrorReason \{([\s\S]*?)\n\}/.exec(text)?.[1] ?? "";
  return new Set([...block.matchAll(/ERROR_REASON_(\w+)\s*=/g)].map((m) => m[1]!).filter((r) => r !== "UNSPECIFIED"));
}

// ---------------------------------------------------------------------------------------------
// Validation

const BUILTIN_ROOTS = ["run", "scenario", "admin", "item", "index"];
const SCENARIO_KEYS = new Set([
  "id", "area", "title", "surfaces", "timeout", "budget", "requires", "fixtures", "setup", "agent", "prompt",
  "peers", "during", "assertions", "teardown", "notes",
]);
const MAX_TIMEOUT_S = 4 * 3600;

export interface ValidationContext {
  catalog: Catalog;
  reasons: Set<string>;
  fixtures: Map<string, Fixture>;
  product?: Map<string, string>;
  file?: string;
}

class Scope {
  private readonly vars = new Set<string>(BUILTIN_ROOTS);
  define(name: string): void {
    this.vars.add(name.split(".")[0]!);
  }
  has(name: string): boolean {
    return this.vars.has(name.split(/[.[]/)[0]!);
  }
}

function checkTemplates(value: unknown, scope: Scope, where: string, errors: string[], allowAgentFiles: boolean): void {
  for (const e of expressions(value)) {
    const colon = e.indexOf(":");
    if (colon > 0) {
      const fn = e.slice(0, colon);
      if (!(FUNCTIONS as readonly string[]).includes(fn)) errors.push(`${where}: unknown template function \${${e}}`);
      if ((fn === "file" || fn === "json") && !allowAgentFiles) errors.push(`${where}: \${${e}} only works after the agent ran`);
      if ((fn === "b64v" || fn === "id") && !scope.has(e.slice(colon + 1))) errors.push(`${where}: undefined variable in \${${e}}`);
      continue;
    }
    if (!scope.has(e)) errors.push(`${where}: undefined variable \${${e}}`);
  }
}

function checkPredicates(preds: unknown, where: string, errors: string[]): Predicate[] {
  if (!Array.isArray(preds) || preds.length === 0) {
    errors.push(`${where}: expect must be a non-empty list`);
    return [];
  }
  const ok: Predicate[] = [];
  for (const [i, p] of (preds as Predicate[]).entries()) {
    const at = `${where}[${i}]`;
    if (!p || typeof p !== "object" || typeof p.path !== "string") {
      errors.push(`${at}: predicate needs a string path`);
      continue;
    }
    try {
      parsePath(p.path);
      const op = operatorOf(p);
      if (op === "matches") regex(String(p.matches));
      if (op === "some" || op === "every" || op === "none") checkPredicates(p[op], `${at}.${op}`, errors);
      if (op === "count_gte") checkPredicates(p.where, `${at}.where`, errors);
      const extra = Object.keys(p).filter((k) => k !== "path" && k !== "decode" && k !== "where" && !(OPERATORS as readonly string[]).includes(k));
      if (extra.length) errors.push(`${at}: unknown keys ${extra.join(", ")}`);
      if (p.decode !== undefined && p.decode !== "base64") errors.push(`${at}: decode must be base64`);
      ok.push(p);
    } catch (e) {
      errors.push(`${at}: ${(e as Error).message}`);
    }
  }
  return ok;
}

/** Response document shape for path checks: server streams are `{messages: [...]}`. */
function responsePathError(catalog: Catalog, m: Method, path: string): string | null {
  if (path === "$" || path === "") return null;
  let segs = parsePath(path);
  if (m.serverStreaming) {
    if (segs[0] !== "messages") return `server-streaming ${m.key}: paths start with messages`;
    segs = segs.slice(1);
    if (segs.length === 0) return null;
    if (segs[0] !== "*" && typeof segs[0] !== "number") return `messages must be indexed`;
    segs = segs.slice(1);
  }
  return checkPath(catalog, m.output, segs);
}

function validateStep(step: Step, where: string, ctx: ValidationContext, scope: Scope, errors: string[], afterAgent: boolean): void {
  if (!step || typeof step !== "object") {
    errors.push(`${where}: not a step`);
    return;
  }
  const kinds = ["call", "commit", "answer", "workdir", "http", "exec", "sleep_ms"].filter((k) => k in step);
  if (kinds.length !== 1) {
    errors.push(`${where}: a step has exactly one of call, commit, answer, workdir, http, exec, sleep_ms`);
    return;
  }
  if (isCall(step)) {
    const m = ctx.catalog.resolve(step.call);
    if (!m) {
      errors.push(`${where}: ${step.call} is not an RPC in proto/bee/v1`);
      return;
    }
    if (!step.call.includes("/")) errors.push(`${where}: write calls as Service/Method (${m.key})`);
    if (step.each !== undefined) {
      checkTemplates(step.each, scope, `${where}.each`, errors, afterAgent);
    }
    checkTemplates([step.as, step.endpoint, step.request, step.items], scope, where, errors, afterAgent);
    if (m.clientStreaming) {
      if (!Array.isArray(step.items) || step.items.length === 0) errors.push(`${where}: client-streaming ${m.key} needs items`);
      else step.items.forEach((it, i) => errors.push(...checkRequest(ctx.catalog, m.input, it).map((e) => `${where}.items[${i}]: ${e}`)));
    } else {
      if (step.items) errors.push(`${where}: items only apply to client-streaming calls`);
      errors.push(...checkRequest(ctx.catalog, m.input, step.request ?? {}).map((e) => `${where}: ${e}`));
    }
    if (step.stream && !m.serverStreaming) errors.push(`${where}: stream options on a unary call`);
    if (step.expect && step.expect_error) errors.push(`${where}: expect and expect_error are exclusive`);
    if (step.expect) {
      for (const p of checkPredicates(step.expect, `${where}.expect`, errors)) {
        for (const segs of predicatePaths(p)) {
          const path = segs.map((s) => (typeof s === "string" && s !== "*" ? `.${s}` : `[${s}]`)).join("").replace(/^\./, "");
          const err = responsePathError(ctx.catalog, m, path);
          if (err) errors.push(`${where}.expect: ${err}`);
        }
        checkTemplates(p, scope, `${where}.expect`, errors, afterAgent);
      }
    }
    if (step.expect_error) {
      const reasons = Array.isArray(step.expect_error.reason) ? step.expect_error.reason : [step.expect_error.reason];
      for (const r of reasons) if (!ctx.reasons.has(r)) errors.push(`${where}: ${r} is not a SPEC §12.2 reason`);
      if (step.expect_error.expect) checkPredicates(step.expect_error.expect, `${where}.expect_error.expect`, errors);
    }
    if (step.retry && !(step.retry.attempts > 0 && step.retry.interval_ms >= 0)) errors.push(`${where}: bad retry`);
    for (const [name, path] of Object.entries(step.save ?? {})) {
      const err = responsePathError(ctx.catalog, m, path);
      if (err) errors.push(`${where}.save.${name}: ${err}`);
      scope.define(name);
    }
    return;
  }
  if (isCommit(step)) {
    const c = step.commit;
    if (!c || typeof c.repo !== "string" || typeof c.message !== "string" || !c.files || typeof c.files !== "object") {
      errors.push(`${where}: commit needs repo, message and files`);
      return;
    }
    if (!c.ref && !c.parent) errors.push(`${where}: commit needs ref or parent`);
    if (c.land !== false && !c.ref) errors.push(`${where}: a landing commit needs ref`);
    checkTemplates(c, scope, where, errors, afterAgent);
    if (c.expect_error) {
      const reasons = Array.isArray(c.expect_error.reason) ? c.expect_error.reason : [c.expect_error.reason];
      for (const r of reasons) if (!ctx.reasons.has(r)) errors.push(`${where}: ${r} is not a SPEC §12.2 reason`);
      if (step.save) errors.push(`${where}: a refused commit saves nothing`);
    }
    if (step.save) scope.define(step.save);
    return;
  }
  if (isAnswer(step)) {
    if (!afterAgent) errors.push(`${where}: answer steps only belong in assertions`);
    if (typeof step.answer?.file !== "string" || step.answer.file.includes("..")) errors.push(`${where}: answer needs a plain file name`);
    for (const p of checkPredicates(step.answer?.expect, `${where}.expect`, errors)) checkTemplates(p, scope, `${where}.expect`, errors, true);
    return;
  }
  if (isWorkdir(step)) {
    if (!afterAgent) errors.push(`${where}: workdir steps only belong in assertions`);
    const w = step.workdir;
    if (!w || typeof w.glob !== "string" || w.glob.includes("..") || w.glob.startsWith("/")) errors.push(`${where}: workdir needs a relative glob`);
    else if (w.min === undefined && w.max === undefined) errors.push(`${where}: workdir needs min or max`);
    return;
  }
  if (isHttp(step)) {
    if (typeof step.http?.url !== "string") errors.push(`${where}: http needs a url`);
    checkTemplates(step.http, scope, where, errors, afterAgent);
    if (step.expect) checkPredicates(step.expect, `${where}.expect`, errors);
    for (const name of Object.keys(step.save ?? {})) scope.define(name);
    return;
  }
  if (isExec(step)) {
    if (!Array.isArray(step.exec) || step.exec.length === 0) errors.push(`${where}: exec needs an argv list`);
    checkTemplates(step.exec, scope, where, errors, afterAgent);
    if (step.save_stdout) scope.define(step.save_stdout);
    return;
  }
  if (isSleep(step) && !(step.sleep_ms >= 0)) errors.push(`${where}: bad sleep_ms`);
}

/** Phrases that would tell the agent which command or RPC to use. */
function promptHints(prompt: string, catalog: Catalog): string[] {
  const hits: string[] = [];
  const cmd = /`bee\s+[a-z][\w-]*|\bbee\s+(?:cr|land|commit|lease|review|mcp|checkout|log|blame|token|audit|fsck|gc|issue|release|check|workflow)\b/.exec(prompt);
  if (cmd) hits.push(`names a CLI command (${cmd[0]})`);
  const words = new Set(prompt.match(/\b[A-Z][a-z]+(?:[A-Z][a-z0-9]+)+\b/g) ?? []);
  for (const w of words) if (catalog.resolve(w)) hits.push(`names an RPC (${w})`);
  if (/\bmcp tool\b|\btool named\b/i.test(prompt)) hits.push("mentions MCP tool names");
  return hits;
}

export function validateScenario(s: Scenario, ctx: ValidationContext): string[] {
  const errors: string[] = [];
  const req = (cond: boolean, msg: string) => {
    if (!cond) errors.push(msg);
  };
  req(typeof s.id === "string" && /^AS-[A-Z]+-\d{2}$/.test(s.id), "id must look like AS-AREA-NN");
  if (ctx.file) req(basename(ctx.file).replace(/\.ya?ml$/, "") === s.id, `file name must be ${s.id}.yaml`);
  for (const k of Object.keys(s)) req(SCENARIO_KEYS.has(k), `unknown key ${k}`);
  req(typeof s.area === "string" && /^4\.\d+$/.test(s.area), "area must be a PRODUCT §4 section such as 4.1");
  if (ctx.product) {
    const area = ctx.product.get(s.id);
    req(area !== undefined, `${s.id} is not an agent scenario in docs/PRODUCT.md §4`);
    if (area) req(area === s.area, `area ${s.area} but PRODUCT lists ${s.id} under ${area}`);
  }
  req(typeof s.title === "string" && s.title.length > 0, "title required");
  req(["cli", "mcp", "both"].includes(s.surfaces), "surfaces must be cli, mcp or both");
  req(typeof s.timeout === "number" && s.timeout > 0 && s.timeout <= MAX_TIMEOUT_S, `timeout must be 1..${MAX_TIMEOUT_S} seconds`);
  req(typeof s.prompt === "string" && s.prompt.trim().length >= 20, "prompt required");
  if (typeof s.prompt === "string") for (const h of promptHints(s.prompt, ctx.catalog)) errors.push(`prompt ${h}: prompts carry no hints`);
  for (const r of s.requires ?? []) req(/^(bin|env|fixture):[\w.-]+$/.test(r), `bad requirement ${r}`);
  req(Array.isArray(s.setup), "setup must be a list (may be empty)");
  req(Array.isArray(s.assertions) && s.assertions.length > 0, "assertions required");
  req(Boolean(s.agent && typeof s.agent.token === "string"), "agent.token required");

  const scope = new Scope();
  for (const name of s.fixtures ?? []) {
    const fx = ctx.fixtures.get(name);
    if (!fx) {
      errors.push(`unknown fixture ${name}`);
      continue;
    }
    (fx.setup ?? []).forEach((st, i) => validateStep(st, `fixture ${name}[${i}]`, ctx, scope, errors, false));
  }
  (s.setup ?? []).forEach((st, i) => validateStep(st, `setup[${i}]`, ctx, scope, errors, false));
  if (s.agent) {
    checkTemplates(s.agent, scope, "agent", errors, false);
  }
  checkTemplates(s.prompt, scope, "prompt", errors, false);
  for (const [i, p] of (s.peers ?? []).entries()) {
    req(typeof p.name === "string" && /^[a-z][\w-]*$/.test(p.name), `peers[${i}].name must be a lowercase word`);
    req(typeof p.prompt === "string" && p.prompt.length >= 20, `peers[${i}].prompt required`);
    req(typeof p.token === "string", `peers[${i}].token required`);
    checkTemplates(p, scope, `peers[${i}]`, errors, false);
    for (const h of promptHints(p.prompt ?? "", ctx.catalog)) errors.push(`peers[${i}].prompt ${h}`);
  }
  for (const [i, d] of (s.during ?? []).entries()) {
    req(d.after_ms !== undefined || d.wait_for_file !== undefined, `during[${i}] needs after_ms or wait_for_file`);
    (d.steps ?? []).forEach((st, j) => validateStep(st, `during[${i}].steps[${j}]`, ctx, scope, errors, false));
  }
  let checks = 0;
  (s.assertions ?? []).forEach((st, i) => {
    validateStep(st, `assertions[${i}]`, ctx, scope, errors, true);
    if ((isCall(st) && (st.expect || st.expect_error)) || (isCommit(st) && st.commit.expect_error) || isAnswer(st) || isWorkdir(st) || (isHttp(st) && st.expect)) checks++;
  });
  req(checks > 0, "assertions must check something (expect, expect_error or answer)");
  req(
    (s.assertions ?? []).some((st) => (isCall(st) && (st.expect || st.expect_error)) || (isCommit(st) && st.commit.expect_error !== undefined)),
    "at least one assertion must check server state through the API",
  );
  (s.teardown ?? []).forEach((st, i) => validateStep(st, `teardown[${i}]`, ctx, scope, errors, true));
  return errors;
}
