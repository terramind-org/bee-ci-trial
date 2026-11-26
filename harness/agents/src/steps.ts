// Executes scenario steps (setup, during, assertions, teardown) against a live server.

import { readFileSync, existsSync } from "node:fs";
import { join } from "node:path";
import type { Catalog } from "./catalog";
import { blobId } from "./blake3";
import { ConnectClient, type RpcError, type RpcResult } from "./connect";
import { evaluate, select, type Predicate } from "./predicate";
import {
  isAnswer, isCall, isCommit, isExec, isHttp, isSleep, isWorkdir, stepLabel,
  type CallStep, type CommitStep, type Step,
} from "./scenario";
import { interpolate, TemplateError, type TemplateContext } from "./template";

export interface StepOutcome {
  step: string;
  ok: boolean;
  /** Why it failed, or the checks that passed. */
  details: string[];
  ms: number;
}

export interface Executor {
  client: ConnectClient;
  catalog: Catalog;
  adminToken?: string;
  ctx: TemplateContext;
  /** Per-call timeout. */
  callTimeoutMs?: number;
  log?: (line: string) => void;
}

export class StepFailure extends Error {
  constructor(readonly outcome: StepOutcome) {
    super(`${outcome.step}: ${outcome.details.join("; ")}`);
  }
}

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

function checkAll(doc: unknown, preds: Predicate[], ctx: TemplateContext): { ok: boolean; details: string[] } {
  const details: string[] = [];
  let ok = true;
  for (const p of preds) {
    const r = evaluate(doc, interpolate(ctx, p));
    details.push(r.ok ? `ok ${r.message}` : `FAIL ${r.message}`);
    if (!r.ok) ok = false;
  }
  return { ok, details };
}

async function rpc(ex: Executor, step: CallStep, request: unknown, items: unknown[] | undefined, token: string | undefined): Promise<RpcResult> {
  const m = ex.catalog.resolve(step.call);
  if (!m) throw new Error(`unknown RPC ${step.call}`);
  const opts = { token, timeoutMs: step.stream?.timeout_ms ?? ex.callTimeoutMs ?? 30_000 };
  if (m.clientStreaming) return ex.client.clientStream(m.service, m.method, items ?? [], opts);
  if (m.serverStreaming) return ex.client.serverStream(m.service, m.method, request, { ...opts, maxMessages: step.stream?.max_messages });
  return ex.client.unary(m.service, m.method, request, opts);
}

async function runCallOnce(ex: Executor, step: CallStep, ctx: TemplateContext): Promise<{ ok: boolean; details: string[]; body: unknown }> {
  const token = step.as !== undefined ? String(interpolate(ctx, step.as)) : ex.adminToken;
  const request = interpolate(ctx, step.request ?? {});
  const items = step.items ? interpolate(ctx, step.items) : undefined;
  const res = await rpc(step.endpoint ? { ...ex, client: new ConnectClient(String(interpolate(ctx, step.endpoint))) } : ex, step, request, items, token);
  ex.log?.(`  ${step.call} -> ${res.ok ? "ok" : `${res.error?.reason} ${res.error?.message ?? ""}`} (${res.ms.toFixed(0)} ms)`);
  if (step.expect_error) {
    const want = Array.isArray(step.expect_error.reason) ? step.expect_error.reason : [step.expect_error.reason];
    if (res.ok) return { ok: false, details: [`FAIL expected ${want.join("|")}, call succeeded`], body: res.body };
    const reasonOk = want.includes(res.error!.reason);
    const details = [`${reasonOk ? "ok" : "FAIL"} reason ${res.error!.reason} (want ${want.join("|")})`];
    let ok = reasonOk;
    if (step.expect_error.expect) {
      const r = checkAll(res.error!.detail, step.expect_error.expect, ctx);
      ok = ok && r.ok;
      details.push(...r.details);
    }
    return { ok, details, body: res.error };
  }
  if (!res.ok) return { ok: false, details: [`FAIL ${step.call}: ${res.error!.reason}: ${res.error!.message}`], body: res.body };
  if (!step.expect) return { ok: true, details: [], body: res.body };
  const r = checkAll(res.body, step.expect, ctx);
  return { ...r, body: res.body };
}

async function runCall(ex: Executor, step: CallStep): Promise<StepOutcome> {
  const t0 = performance.now();
  const label = stepLabel(step);
  const elements: unknown[] | undefined =
    step.each === undefined ? undefined : (interpolate(ex.ctx, step.each) as unknown[]);
  if (elements !== undefined && !Array.isArray(elements)) throw new TemplateError(`${label}: each is not a list`);
  const iterations = elements ?? [undefined];
  const saved: Record<string, unknown[]> = {};
  const details: string[] = [];
  let ok = true;
  for (const [index, item] of iterations.entries()) {
    const ctx: TemplateContext = elements ? { ...ex.ctx, vars: { ...ex.ctx.vars, item, index } } : ex.ctx;
    const attempts = step.retry?.attempts ?? 1;
    let last: { ok: boolean; details: string[]; body: unknown } = { ok: false, details: [], body: undefined };
    for (let a = 0; a < attempts; a++) {
      if (a > 0) await sleep(step.retry!.interval_ms);
      last = await runCallOnce(ex, step, ctx);
      if (last.ok) break;
    }
    details.push(...last.details.map((d) => (elements ? `[${index}] ${d}` : d)));
    if (!last.ok) {
      ok = false;
      break;
    }
    for (const [name, path] of Object.entries(step.save ?? {})) {
      const v = path === "$" ? last.body : select(last.body, path);
      if (elements) (saved[name] ??= []).push(v);
      else ex.ctx.vars[name] = v;
    }
  }
  if (ok && elements) Object.assign(ex.ctx.vars, saved);
  return { step: label, ok, details, ms: performance.now() - t0 };
}

const b64 = (b: Uint8Array) => Buffer.from(b).toString("base64");

async function runCommit(ex: Executor, step: CommitStep): Promise<StepOutcome> {
  const t0 = performance.now();
  const c = interpolate(ex.ctx, step.commit);
  const token = c.as ?? ex.adminToken;
  const repo = { fullName: c.repo };
  const opts = { token, timeoutMs: ex.callTimeoutMs ?? 30_000 };
  const fail = (why: string): StepOutcome => ({ step: stepLabel(step), ok: false, details: [why], ms: performance.now() - t0 });
  const wanted = c.expect_error ? (Array.isArray(c.expect_error.reason) ? c.expect_error.reason : [c.expect_error.reason]) : [];
  const refused = (stage: string, e: RpcError): StepOutcome =>
    wanted.includes(e.reason)
      ? { step: stepLabel(step), ok: true, details: [`ok ${stage} refused with ${e.reason}`], ms: performance.now() - t0 }
      : fail(`${wanted.length ? "FAIL " : ""}${stage}: ${e.reason} ${e.message}${wanted.length ? ` (want ${wanted.join("|")})` : ""}`);

  let parentRaw: string | undefined;
  if (c.parent) {
    const r = await ex.client.unary("QueryService", "Resolve", { repo, revisions: [{ expr: c.parent }] }, opts);
    if (!r.ok) return fail(`resolve ${c.parent}: ${r.error!.reason} ${r.error!.message}`);
    parentRaw = select(r.body, "ids[0].raw") as string | undefined;
  } else {
    const r = await ex.client.unary("ObjectService", "ResolveRef", { repo, name: c.ref }, opts);
    if (!r.ok) return fail(`resolve ref ${c.ref}: ${r.error!.reason} ${r.error!.message}`);
    parentRaw = select(r.body, "ref.target.raw") as string | undefined;
  }
  if (!parentRaw) return fail("could not determine the parent changeset");

  const delta: unknown[] = [];
  const uploads: unknown[] = [];
  const files: Record<string, string | null> = { ...c.files };
  if (c.generate) {
    for (let i = 0; i < c.generate.count; i++) {
      files[c.generate.pattern.replaceAll("{i}", String(i))] = c.generate.content.replaceAll("{i}", String(i));
    }
  }
  for (const path of Object.keys(files).sort()) {
    const content = files[path];
    if (content === null || content === undefined) {
      delta.push({ path: b64(Buffer.from(path)), delete: true });
      continue;
    }
    const bytes = new TextEncoder().encode(content);
    const id = b64(blobId(bytes));
    uploads.push({ ...(uploads.length === 0 ? { repo } : {}), id: { raw: id }, offset: "0", data: b64(bytes), last: true });
    delta.push({ path: b64(Buffer.from(path)), put: { mode: "MODE_FILE", blob: { raw: id }, size: String(bytes.length) } });
  }
  if (uploads.length) {
    // A large seed (1,000 blobs) takes longer than one ordinary call.
    const r = await ex.client.clientStream("WriteService", "UploadObjects", uploads, { ...opts, timeoutMs: (opts.timeoutMs ?? 30_000) + uploads.length * 100 });
    if (!r.ok) return refused("upload", r.error!);
  }
  const created = await ex.client.unary(
    "WriteService",
    "CreateChangeset",
    {
      repo,
      parents: [{ raw: parentRaw }],
      delta,
      changeId: { raw: interpolate(ex.ctx, `\${changeid:${c.change ?? `${c.message}`}}`) },
      author: c.author ?? "bee-harness",
      ...(c.author_time
        ? {
            authorTime: { name: c.author ?? "bee-harness", time: c.author_time },
            committerTime: { name: c.author ?? "bee-harness", time: c.author_time },
          }
        : {}),
      message: c.message,
      ...(c.provenance ? { provenance: c.provenance } : {}),
    },
    opts,
  );
  if (!created.ok) return refused("create", created.error!);
  const cs = select(created.body, "changeset") as Record<string, unknown>;
  const saved: Record<string, unknown> = { id: select(cs, "id"), changeId: select(cs, "changeId"), changeset: cs };
  if (c.ref_name) {
    // The branch is scenario scaffolding, not the committer's act: the harness admin points it, so a
    // committer whose role lacks manage_refs (an agent) can still be the author of the changeset.
    const refOpts = { ...opts, token: ex.adminToken ?? token };
    const ur = await ex.client.unary("RefService", "CreateRef", { repo, name: c.ref_name, target: select(cs, "id") }, refOpts);
    if (!ur.ok) {
      const up = await ex.client.unary(
        "RefService",
        "UpdateRef",
        { repo, name: c.ref_name, target: select(cs, "id"), allowNonFastForward: true },
        refOpts,
      );
      if (!up.ok) {
        // The create error is the real one when the ref did not exist; the update only says "not found".
        const why = up.error!.reason === "NOT_FOUND" ? ur.error! : up.error!;
        return fail(`point ${c.ref_name}: ${why.reason} ${why.message}`);
      }
    }
  }
  if (c.land !== false && c.ref) {
    const landed = await ex.client.unary("WriteService", "Land", { repo, ref: c.ref, head: select(cs, "id") }, opts);
    if (!landed.ok) return refused("land", landed.error!);
    saved.ref = select(landed.body, "ref");
    saved.landed = select(landed.body, "landed");
    saved.result = select(landed.body, "landed[0].result");
  }
  if (wanted.length) return fail(`FAIL expected ${wanted.join("|")}, the commit succeeded`);
  if (step.save) ex.ctx.vars[step.save] = saved;
  return { step: stepLabel(step), ok: true, details: [], ms: performance.now() - t0 };
}

export async function runStep(ex: Executor, step: Step): Promise<StepOutcome> {
  const t0 = performance.now();
  try {
    if (isCall(step)) return await runCall(ex, step);
    if (isCommit(step)) return await runCommit(ex, step);
    if (isAnswer(step)) {
      const file = join(ex.ctx.workdir ?? "/nonexistent", step.answer.file);
      if (!existsSync(file)) return { step: stepLabel(step), ok: false, details: [`FAIL agent did not write ${step.answer.file}`], ms: 0 };
      let doc: unknown;
      try {
        doc = JSON.parse(readFileSync(file, "utf8")) as unknown;
      } catch {
        return { step: stepLabel(step), ok: false, details: [`FAIL ${step.answer.file} is not JSON`], ms: 0 };
      }
      const r = checkAll(doc, step.answer.expect, ex.ctx);
      return { step: stepLabel(step), ...r, ms: performance.now() - t0 };
    }
    if (isWorkdir(step)) {
      const w = interpolate(ex.ctx, step.workdir);
      if (!ex.ctx.workdir) return { step: stepLabel(step), ok: false, details: ["FAIL no agent working directory"], ms: 0 };
      let n = 0;
      for (const _ of new Bun.Glob(w.glob).scanSync({ cwd: ex.ctx.workdir, dot: false, onlyFiles: true })) n++;
      const ok = (w.min === undefined || n >= w.min) && (w.max === undefined || n <= w.max);
      return { step: stepLabel(step), ok, details: [`${ok ? "ok" : "FAIL"} ${n} files match ${w.glob} (min ${w.min ?? "-"}, max ${w.max ?? "-"})`], ms: performance.now() - t0 };
    }
    if (isHttp(step)) {
      const h = interpolate(ex.ctx, step.http);
      const attempts = step.retry?.attempts ?? 1;
      let out: StepOutcome = { step: stepLabel(step), ok: false, details: [], ms: 0 };
      for (let a = 0; a < attempts; a++) {
        if (a > 0) await sleep(step.retry!.interval_ms);
        const res = await fetch(h.url, {
          method: h.method ?? "GET",
          headers: h.headers,
          body: h.body === undefined ? undefined : JSON.stringify(h.body),
          signal: AbortSignal.timeout(ex.callTimeoutMs ?? 30_000),
        });
        const text = await res.text();
        let body: unknown = text;
        try {
          body = JSON.parse(text) as unknown;
        } catch {
          /* keep text */
        }
        const doc = { status: res.status, body };
        const r = step.expect ? checkAll(doc, step.expect, ex.ctx) : { ok: res.ok, details: [`status ${res.status}`] };
        out = { step: stepLabel(step), ...r, ms: performance.now() - t0 };
        if (r.ok) {
          for (const [name, path] of Object.entries(step.save ?? {})) ex.ctx.vars[name] = select(doc, path);
          break;
        }
      }
      return out;
    }
    if (isExec(step)) {
      const argv = interpolate(ex.ctx, step.exec).map(String);
      const proc = Bun.spawn(argv, { stdout: "pipe", stderr: "pipe" });
      const [stdout, stderr, code] = await Promise.all([new Response(proc.stdout).text(), new Response(proc.stderr).text(), proc.exited]);
      if (step.save_stdout) ex.ctx.vars[step.save_stdout] = stdout.trim();
      return { step: stepLabel(step), ok: code === 0, details: code === 0 ? [] : [`exit ${code}: ${stderr.slice(0, 500)}`], ms: performance.now() - t0 };
    }
    if (isSleep(step)) {
      await sleep(step.sleep_ms);
      return { step: stepLabel(step), ok: true, details: [], ms: step.sleep_ms };
    }
    return { step: "?", ok: false, details: ["unknown step kind"], ms: 0 };
  } catch (e) {
    return { step: stepLabel(step), ok: false, details: [`FAIL ${(e as Error).message}`], ms: performance.now() - t0 };
  }
}

/** Runs steps in order; stops at the first failure when `stopOnFailure`. */
export async function runSteps(ex: Executor, steps: Step[], stopOnFailure: boolean): Promise<StepOutcome[]> {
  const out: StepOutcome[] = [];
  for (const s of steps) {
    const o = await runStep(ex, s);
    out.push(o);
    if (!o.ok && stopOnFailure) break;
  }
  return out;
}
