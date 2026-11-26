// `${…}` interpolation in scenario files.
//
//   ${name.path}       a variable (setup `save`, run info such as run.ns, or a fixture)
//   ${b64:text}        base64 of a literal (bytes fields such as paths are base64 in proto3-JSON)
//   ${b64v:name.path}  base64 of a variable's value
//   ${id:name.path}     text form of an id variable: c1_<hex> for objects ({raw} or base64), k_<hex> for ChangeIds
//   ${changeid:label}  a ChangeId raw value (base64 of 16 bytes) unique to this run and label
//   ${time:-24h}       RFC 3339 time relative to the run start (s, m, h, d units)
//   ${env:NAME}        a BEE_HARNESS_* environment variable (never the admin token), e.g. sandbox credentials
//   ${file:name}       trimmed contents of a file the agent left in its working directory
//   ${json:name}       a JSON file the agent left, parsed; ${json:name#path} selects a field
//
// A string that is exactly one `${…}` takes the raw value (object, number, array); otherwise the
// pieces are stringified and joined. `$${…}` is a literal `${…}` (for shell or JS in fixtures).

import { createHash } from "node:crypto";
import { readFileSync, existsSync } from "node:fs";
import { join } from "node:path";
import { select } from "./predicate";

export interface TemplateContext {
  vars: Record<string, unknown>;
  /** Run start, the anchor of ${time:…}. */
  now: Date;
  /** Unique per run; seeds ${changeid:…}. */
  runId: string;
  /** The agent's working directory, for ${file:…} and ${json:…}. */
  workdir?: string;
}

export class TemplateError extends Error {}

const WHOLE = /^\$\{([^}]+)\}$/;
const ANY = /\$?\$\{([^}]+)\}/g;

function lookup(ctx: TemplateContext, name: string): unknown {
  const v = select(ctx.vars, name);
  if (v === undefined) throw new TemplateError(`undefined variable \${${name}}`);
  return v;
}

function readAgentFile(ctx: TemplateContext, name: string): string {
  if (!ctx.workdir) throw new TemplateError(`\${file:${name}} needs an agent working directory`);
  if (name.includes("..")) throw new TemplateError(`\${file:${name}}: no parent paths`);
  const path = join(ctx.workdir, name);
  if (!existsSync(path)) throw new TemplateError(`agent did not write ${name}`);
  return readFileSync(path, "utf8");
}

export function expr(ctx: TemplateContext, e: string): unknown {
  const colon = e.indexOf(":");
  const fn = colon > 0 ? e.slice(0, colon) : "";
  const arg = colon > 0 ? e.slice(colon + 1) : e;
  switch (fn) {
    case "":
      return lookup(ctx, e);
    case "b64":
      return Buffer.from(arg, "utf8").toString("base64");
    case "b64v":
      return Buffer.from(String(lookup(ctx, arg)), "utf8").toString("base64");
    case "id":
      return idText(lookup(ctx, arg));
    case "changeid":
      return createHash("sha256").update(`${ctx.runId}\0${arg}`).digest().subarray(0, 16).toString("base64");
    case "time": {
      const m = /^([+-]?\d+)([smhd])$/.exec(arg);
      if (!m) throw new TemplateError(`bad \${time:${arg}}`);
      const unit = { s: 1e3, m: 6e4, h: 3.6e6, d: 8.64e7 }[m[2] as "s" | "m" | "h" | "d"];
      return new Date(ctx.now.getTime() + Number(m[1]) * unit).toISOString();
    }
    case "file":
      return readAgentFile(ctx, arg).trim();
    case "env": {
      if (!arg.startsWith("BEE_HARNESS_") || arg === "BEE_HARNESS_ADMIN_TOKEN") {
        throw new TemplateError(`\${env:${arg}}: only BEE_HARNESS_* variables other than the admin token`);
      }
      const v = process.env[arg];
      if (v === undefined) throw new TemplateError(`env ${arg} is not set`);
      return v;
    }
    case "json": {
      const [file, path] = arg.split("#", 2) as [string, string | undefined];
      let doc: unknown;
      try {
        doc = JSON.parse(readAgentFile(ctx, file)) as unknown;
      } catch (e) {
        if (e instanceof TemplateError) throw e;
        throw new TemplateError(`${file} is not valid JSON`);
      }
      if (path === undefined) return doc;
      const v = select(doc, path);
      if (v === undefined) throw new TemplateError(`${file} has no ${path}`);
      return v;
    }
    default:
      // Not a function: a variable path that happens to contain a colon is not allowed.
      throw new TemplateError(`unknown template function ${fn}`);
  }
}

const KIND_LETTER: Record<number, string> = { 1: "b", 2: "t", 3: "c", 4: "m" };

/** SPEC §3.1/§3.3 text forms. Accepts {raw: base64}, a base64 string, or an existing text id. */
export function idText(v: unknown): string {
  const raw = v && typeof v === "object" ? (v as Record<string, unknown>).raw : v;
  if (typeof raw !== "string") throw new TemplateError(`not an id: ${JSON.stringify(v)}`);
  if (/^[a-z]\d?_[0-9a-f]+$/.test(raw)) return raw;
  const b = Buffer.from(raw, "base64");
  if (b.length === 16) return `k_${b.toString("hex")}`;
  if (b.length === 34) {
    const letter = b[0] === 2 ? "g" : (KIND_LETTER[b[1]!] ?? `d${b[0]}${b[1]!.toString(16).padStart(2, "0")}`);
    return b[0] === 2 ? `g2_${b.subarray(2).toString("hex")}` : `${letter}${letter.startsWith("d") ? "" : b[0]}_${b.subarray(2).toString("hex")}`;
  }
  throw new TemplateError(`id has ${b.length} bytes`);
}

export function interpolate<T>(ctx: TemplateContext, value: T): T {
  return walk(ctx, value) as T;
}

function walk(ctx: TemplateContext, value: unknown): unknown {
  if (typeof value === "string") {
    const whole = WHOLE.exec(value);
    if (whole) return expr(ctx, whole[1]!);
    return value.replace(ANY, (m: string, e: string) => {
      if (m.startsWith("$$")) return m.slice(1);
      const v = expr(ctx, e);
      return typeof v === "string" ? v : JSON.stringify(v);
    });
  }
  if (Array.isArray(value)) return value.map((v) => walk(ctx, v));
  if (value && typeof value === "object") {
    const out: Record<string, unknown> = {};
    for (const [k, v] of Object.entries(value)) out[k] = walk(ctx, v);
    return out;
  }
  return value;
}

/** Template functions a scenario may use; the validator rejects any other. */
export const FUNCTIONS = ["id", "env", "b64", "b64v", "changeid", "time", "file", "json"] as const;

/** Every `${…}` expression in a value, for static validation. */
export function expressions(value: unknown): string[] {
  const out: string[] = [];
  const visit = (v: unknown) => {
    if (typeof v === "string") {
      for (const m of v.matchAll(ANY)) if (!m[0].startsWith("$$")) out.push(m[1]!);
    } else if (Array.isArray(v)) v.forEach(visit);
    else if (v && typeof v === "object") Object.values(v).forEach(visit);
  };
  visit(value);
  return out;
}
