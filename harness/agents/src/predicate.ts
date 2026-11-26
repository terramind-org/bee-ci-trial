// JSON paths and predicates: the assertion language of scenario files.
//
// A path is dotted field access with array steps: `changeset.delta[0].path`, `refs[*].name`.
// `[-1]` is the last element. A `[*]` step fans out, so the path yields an array of every match. Field names may be
// lowerCamelCase or snake_case (proto3-JSON accepts both; responses use lowerCamelCase).
//
// A predicate is `{path, <op>: operand, decode?}` with exactly one operator.

import { idText } from "./template";

export type Segment = string | number | "*";

export function parsePath(path: string): Segment[] {
  const out: Segment[] = [];
  if (path === "" || path === "$") return out;
  const re = /([^.[\]]+)|\[(\*|-?\d+)\]/g;
  let m: RegExpExecArray | null;
  let consumed = 0;
  const body = path.startsWith("$.") ? path.slice(2) : path;
  while ((m = re.exec(body))) {
    if (m[1] !== undefined) out.push(m[1]);
    else out.push(m[2] === "*" ? "*" : Number(m[2]));
    consumed += m[0].length;
  }
  const dots = (body.match(/\./g) ?? []).length;
  if (consumed + dots !== body.length || /^\.|\.\.|\.$|\.\[/.test(body)) throw new Error(`bad path: ${path}`);
  return out;
}

function snake(key: string): string {
  return key.replace(/[A-Z]/g, (c) => `_${c.toLowerCase()}`);
}

function camel(key: string): string {
  return key.replace(/_([a-z0-9])/g, (_, c: string) => c.toUpperCase());
}

function field(obj: unknown, key: string): unknown {
  if (obj === null || typeof obj !== "object" || Array.isArray(obj)) return undefined;
  const rec = obj as Record<string, unknown>;
  if (key in rec) return rec[key];
  const c = camel(key);
  if (c in rec) return rec[c];
  const s = snake(key);
  return s in rec ? rec[s] : undefined;
}

/** Evaluates a path. With a `[*]` step the result is always an array (possibly empty). */
export function select(value: unknown, path: string | Segment[]): unknown {
  const segs = typeof path === "string" ? parsePath(path) : path;
  let fan = false;
  let cur: unknown[] = [value];
  for (const seg of segs) {
    const next: unknown[] = [];
    for (const v of cur) {
      if (seg === "*") {
        if (Array.isArray(v)) next.push(...v);
      } else if (typeof seg === "number") {
        if (Array.isArray(v)) next.push(v[seg < 0 ? v.length + seg : seg]);
        else next.push(undefined);
      } else {
        next.push(field(v, seg));
      }
    }
    if (seg === "*") fan = true;
    cur = fan ? next.filter((x) => x !== undefined) : next;
  }
  return fan ? cur : cur[0];
}

// ---------------------------------------------------------------------------------------------

export const OPERATORS = [
  "exists",
  "absent",
  "truthy",
  "eq",
  "ne",
  "gt",
  "gte",
  "lt",
  "lte",
  "contains",
  "not_contains",
  "matches",
  "in",
  "len",
  "len_gte",
  "len_lte",
  "some",
  "every",
  "none",
  "count_gte",
  "prefix_of",
  "within",
] as const;

export type Operator = (typeof OPERATORS)[number];

export interface Predicate {
  path: string;
  decode?: "base64";
  /** Used by count_gte: how many elements must satisfy `where`. */
  where?: Predicate[];
  [op: string]: unknown;
}

export interface PredicateResult {
  ok: boolean;
  message: string;
}

export function operatorOf(p: Predicate): Operator {
  const ops = OPERATORS.filter((o) => o in p);
  if (ops.length !== 1) throw new Error(`predicate on ${p.path} needs exactly one operator, has ${ops.length}`);
  return ops[0]!;
}

function decodeB64(v: unknown): unknown {
  if (typeof v === "string") {
    try {
      return Buffer.from(v, "base64").toString("utf8");
    } catch {
      return v;
    }
  }
  if (Array.isArray(v)) return v.map(decodeB64);
  return v;
}

const INT = /^-?\d+$/;
const DATE = /^\d{4}-\d{2}-\d{2}T/;

/** Compares two scalars: integers exactly (proto3-JSON int64 is a string), dates by time. */
export function compare(a: unknown, b: unknown): number | null {
  const isInt = (x: unknown) => (typeof x === "number" && Number.isInteger(x)) || (typeof x === "string" && INT.test(x));
  if (isInt(a) && isInt(b)) {
    const x = BigInt(a as string | number);
    const y = BigInt(b as string | number);
    return x < y ? -1 : x > y ? 1 : 0;
  }
  if (typeof a === "number" && typeof b === "number") return a - b;
  if (typeof a === "string" && typeof b === "string") {
    if (DATE.test(a) && DATE.test(b)) {
      const d = Date.parse(a) - Date.parse(b);
      if (!Number.isNaN(d)) return d;
    }
    return a < b ? -1 : a > b ? 1 : 0;
  }
  if (typeof a === "number" && typeof b === "string" && b !== "" && !Number.isNaN(Number(b))) return a - Number(b);
  if (typeof b === "number" && typeof a === "string" && a !== "" && !Number.isNaN(Number(a))) return Number(a) - b;
  return null;
}

/** A `matches` operand: a JavaScript regex (dotAll); a leading `(?i)` makes it case-insensitive. */
export function regex(source: string): RegExp {
  return source.startsWith("(?i)") ? new RegExp(source.slice(4), "si") : new RegExp(source, "s");
}

export function deepEqual(a: unknown, b: unknown): boolean {
  if (a === b) return true;
  const isScalar = (x: unknown) => typeof x === "string" || typeof x === "number" || typeof x === "boolean";
  if (isScalar(a) && isScalar(b)) {
    if (typeof a === "string" && typeof b === "string" && !(INT.test(a) && INT.test(b))) return false;
    const c = compare(a, b);
    return c !== null && c === 0;
  }
  if (Array.isArray(a) && Array.isArray(b)) return a.length === b.length && a.every((x, i) => deepEqual(x, b[i]));
  if (a && b && typeof a === "object" && typeof b === "object") {
    const ka = Object.keys(a as object);
    const kb = Object.keys(b as object);
    return (
      ka.length === kb.length &&
      ka.every((k) => deepEqual((a as Record<string, unknown>)[k], (b as Record<string, unknown>)[k]))
    );
  }
  return false;
}

function size(v: unknown): number | null {
  if (Array.isArray(v) || typeof v === "string") return v.length;
  if (v && typeof v === "object") return Object.keys(v).length;
  return null;
}

function show(v: unknown): string {
  const s = JSON.stringify(v);
  if (s === undefined) return "undefined";
  return s.length > 200 ? `${s.slice(0, 200)}…` : s;
}

function all(target: unknown, preds: Predicate[]): PredicateResult {
  for (const p of preds) {
    const r = evaluate(target, p);
    if (!r.ok) return r;
  }
  return { ok: true, message: "ok" };
}

/** Evaluates one predicate against a document. Operands must already be template-resolved. */
export function evaluate(doc: unknown, p: Predicate): PredicateResult {
  const op = operatorOf(p);
  let actual = select(doc, p.path);
  if (p.decode === "base64") actual = decodeB64(actual);
  const want = p[op];
  const fail = (why: string): PredicateResult => ({ ok: false, message: `${p.path} ${op}: ${why}` });
  const pass: PredicateResult = { ok: true, message: `${p.path} ${op}` };
  switch (op) {
    case "exists":
      return (actual !== undefined && actual !== null) === (want !== false) ? pass : fail(`got ${show(actual)}`);
    case "absent":
      return actual === undefined || actual === null || (Array.isArray(actual) && actual.length === 0) || actual === "" || actual === false
        ? pass
        : fail(`got ${show(actual)}`);
    case "truthy":
      return Boolean(actual) && !(Array.isArray(actual) && actual.length === 0) ? pass : fail(`got ${show(actual)}`);
    case "eq":
      return deepEqual(actual, want) ? pass : fail(`want ${show(want)}, got ${show(actual)}`);
    case "ne":
      return !deepEqual(actual, want) ? pass : fail(`got ${show(actual)}`);
    case "gt":
    case "gte":
    case "lt":
    case "lte": {
      // proto3-JSON omits a zero-valued number, so an absent field compares as 0.
      const c = compare(actual === undefined && typeof want === "number" ? 0 : actual, want);
      if (c === null) return fail(`cannot compare ${show(actual)} with ${show(want)}`);
      const ok = op === "gt" ? c > 0 : op === "gte" ? c >= 0 : op === "lt" ? c < 0 : c <= 0;
      return ok ? pass : fail(`want ${op} ${show(want)}, got ${show(actual)}`);
    }
    case "contains":
    case "not_contains": {
      let has: boolean;
      if (typeof actual === "string") has = typeof want === "string" && actual.includes(want);
      else if (Array.isArray(actual)) has = actual.some((x) => deepEqual(x, want) || (typeof x === "string" && typeof want === "string" && x === want));
      else has = false;
      return has === (op === "contains") ? pass : fail(`${show(actual)} vs ${show(want)}`);
    }
    case "matches": {
      const re = regex(String(want));
      const vals = Array.isArray(actual) ? actual : [actual];
      return vals.some((v) => typeof v === "string" && re.test(v)) ? pass : fail(`${show(actual)} !~ /${String(want)}/`);
    }
    case "in":
      // An absent field (proto3-JSON omits defaults) matches a listed null.
      return Array.isArray(want) && want.some((w) => deepEqual(actual === undefined ? null : actual, w))
        ? pass
        : fail(`${show(actual)} not in ${show(want)}`);
    case "len":
    case "len_gte":
    case "len_lte": {
      // proto3-JSON omits an empty repeated field, so an absent one has length 0.
      const n = size(actual === undefined ? [] : actual);
      const w = Number(want);
      if (n === null) return fail(`no length: ${show(actual)}`);
      const ok = op === "len" ? n === w : op === "len_gte" ? n >= w : n <= w;
      return ok ? pass : fail(`length ${n}, want ${op} ${w}`);
    }
    case "some":
    case "every":
    case "none": {
      if (!Array.isArray(actual)) return fail(`not an array: ${show(actual)}`);
      const preds = want as Predicate[];
      const hits = actual.map((el) => all(el, preds));
      if (op === "some") return hits.some((h) => h.ok) ? pass : fail(`no element matched (${hits[0]?.message ?? "empty"})`);
      if (op === "every") return hits.every((h) => h.ok) ? pass : fail(hits.find((h) => !h.ok)!.message);
      return hits.every((h) => !h.ok) ? pass : fail(`an element matched`);
    }
    case "prefix_of": {
      // An abbreviated id: at least 12 characters and a prefix of the full value. The wire form
      // (base64 of the raw id, which the MCP surface prints) names the same id as the text form.
      const vals = Array.isArray(actual) ? actual : [actual];
      const full = String(want);
      const asText = (v: string): string => {
        try {
          return idText(v.trim());
        } catch {
          return v.trim();
        }
      };
      return vals.some((v) => typeof v === "string" && v.length >= 12 && (full.startsWith(v.trim()) || full.startsWith(asText(v))))
        ? pass
        : fail(`${show(actual)} is not a prefix (>= 12 chars) of ${full}`);
    }
    case "within": {
      // A timestamp no earlier than `from` and at most `ms` after it.
      const w = (want ?? {}) as { from?: unknown; ms?: unknown };
      const from = Date.parse(String(w.from));
      const ms = Number(w.ms);
      const vals = (Array.isArray(actual) ? actual : [actual]).map((v) => Date.parse(String(v)));
      if (Number.isNaN(from) || Number.isNaN(ms)) return fail(`bad operand ${show(want)}`);
      return vals.some((t) => !Number.isNaN(t) && t >= from && t - from <= ms)
        ? pass
        : fail(`${show(actual)} not within ${ms} ms after ${String(w.from)}`);
    }
    case "count_gte": {
      if (!Array.isArray(actual)) return fail(`not an array: ${show(actual)}`);
      const n = actual.filter((el) => all(el, p.where ?? []).ok).length;
      return n >= Number(want) ? pass : fail(`${n} matching, want >= ${String(want)}`);
    }
  }
}

/** Every path a predicate tree touches, relative to the document it is evaluated on. */
export function predicatePaths(p: Predicate, prefix: Segment[] = []): Segment[][] {
  const op = operatorOf(p);
  const here = [...prefix, ...parsePath(p.path)];
  const out: Segment[][] = [here];
  const nested = op === "some" || op === "every" || op === "none" ? (p[op] as Predicate[]) : op === "count_gte" ? (p.where ?? []) : [];
  // Nested predicates see elements: the fanned-out values, or the items of the array at `here`.
  const element = parsePath(p.path).includes("*") ? here : [...here, "*" as const];
  for (const q of nested) out.push(...predicatePaths(q, element));
  return out;
}
