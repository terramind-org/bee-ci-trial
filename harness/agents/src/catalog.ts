// The RPC catalog: every method in proto/bee/v1, plus the generated JSON Schema of every
// request and response message (schemas/v1/messages). Scenario validation uses it to prove that
// each call names a real RPC, each request field exists and each asserted path can exist.

import { readdirSync, readFileSync, existsSync } from "node:fs";
import { join, resolve } from "node:path";

export interface Method {
  service: string;
  method: string;
  /** "Service/Method", the catalog key. */
  key: string;
  input: string;
  output: string;
  clientStreaming: boolean;
  serverStreaming: boolean;
  file: string;
}

type Schema = Record<string, unknown>;

export const PACKAGE = "bee.v1";

/** The repository root: harness/agents/src/../../.. */
export function repoRoot(): string {
  return resolve(import.meta.dir, "..", "..", "..");
}

export class Catalog {
  readonly methods = new Map<string, Method>();
  private readonly byName = new Map<string, Method[]>();
  private readonly schemaCache = new Map<string, Schema | null>();

  constructor(
    readonly protoDir: string,
    readonly messageDir: string,
  ) {
    for (const file of readdirSync(protoDir).filter((f) => f.endsWith(".proto")).sort()) {
      const text = readFileSync(join(protoDir, file), "utf8");
      let service = "";
      for (const raw of text.split("\n")) {
        const line = raw.replace(/\/\/.*$/, "");
        const svc = /^\s*service\s+(\w+)\s*\{/.exec(line);
        if (svc?.[1]) {
          service = svc[1];
          continue;
        }
        const rpc = /^\s*rpc\s+(\w+)\s*\(\s*(stream\s+)?([\w.]+)\s*\)\s*returns\s*\(\s*(stream\s+)?([\w.]+)\s*\)/.exec(
          line,
        );
        if (rpc && service) {
          const [, method, cs, input, ss, output] = rpc;
          const m: Method = {
            service,
            method: method!,
            key: `${service}/${method}`,
            input: qualify(input!),
            output: qualify(output!),
            clientStreaming: Boolean(cs),
            serverStreaming: Boolean(ss),
            file,
          };
          this.methods.set(m.key, m);
          const list = this.byName.get(m.method) ?? [];
          list.push(m);
          this.byName.set(m.method, list);
        }
        if (/^\}/.test(line)) service = "";
      }
    }
  }

  static load(root = repoRoot()): Catalog {
    return new Catalog(join(root, "proto", "bee", "v1"), join(root, "schemas", "v1", "messages"));
  }

  /** Accepts "Service/Method", "bee.v1.Service/Method" or a bare method name when unique. */
  resolve(name: string): Method | undefined {
    const trimmed = name.replace(/^\/?bee\.v1\./, "");
    if (trimmed.includes("/")) return this.methods.get(trimmed);
    const list = this.byName.get(trimmed);
    return list && list.length === 1 ? list[0] : undefined;
  }

  /** The generated JSON Schema of a message, or null when none was generated. */
  schema(message: string): Schema | null {
    const name = qualify(message);
    if (!this.schemaCache.has(name)) {
      const path = join(this.messageDir, `${name}.schema.json`);
      this.schemaCache.set(name, existsSync(path) ? (JSON.parse(readFileSync(path, "utf8")) as Schema) : null);
    }
    return this.schemaCache.get(name) ?? null;
  }
}

function qualify(name: string): string {
  if (name.startsWith("google.")) return name;
  return name.startsWith(`${PACKAGE}.`) ? name : `${PACKAGE}.${name}`;
}

// ---------------------------------------------------------------------------------------------
// Structural checks against a message schema.

const TEMPLATE = /\$\{[^}]+\}/;

function deref(root: Schema, node: Schema | undefined): Schema | undefined {
  let cur = node;
  for (let i = 0; cur && typeof cur.$ref === "string" && i < 16; i++) {
    const ref = cur.$ref as string;
    const m = /^#\/\$defs\/(.+)$/.exec(ref);
    const defs = root.$defs as Record<string, Schema> | undefined;
    cur = m && defs ? defs[m[1]!] : undefined;
  }
  return cur;
}

function types(node: Schema): string[] {
  const t = node.type;
  if (Array.isArray(t)) return t as string[];
  return typeof t === "string" ? [t] : [];
}

export function camel(key: string): string {
  return key.replace(/_([a-z0-9])/g, (_, c: string) => c.toUpperCase());
}

function property(node: Schema, key: string): Schema | undefined {
  const props = node.properties as Record<string, Schema> | undefined;
  if (props) {
    const hit = props[key] ?? props[camel(key)];
    if (hit) return hit;
  }
  const extra = node.additionalProperties;
  if (extra && typeof extra === "object") return extra as Schema;
  return undefined;
}

/** Checks the keys and literal enum values of a request body against its message schema. */
export function checkRequest(catalog: Catalog, message: string, body: unknown): string[] {
  const root = catalog.schema(message);
  if (!root) return [`no generated schema for ${message}`];
  const errors: string[] = [];
  walk(root, root, body, message, errors);
  return errors;
}

function walk(root: Schema, node: Schema | undefined, value: unknown, at: string, errors: string[]): void {
  const n = deref(root, node);
  if (!n || value === null || value === undefined) return;
  if (typeof value === "string" && TEMPLATE.test(value)) return; // resolved at run time
  const t = types(n);
  if (Array.isArray(value)) {
    if (t.length && !t.includes("array")) {
      errors.push(`${at}: array given, schema says ${t.join("|")}`);
      return;
    }
    value.forEach((v, i) => walk(root, n.items as Schema | undefined, v, `${at}[${i}]`, errors));
    return;
  }
  if (typeof value === "object") {
    if (t.length && !t.includes("object")) {
      errors.push(`${at}: object given, schema says ${t.join("|")}`);
      return;
    }
    for (const [k, v] of Object.entries(value as Record<string, unknown>)) {
      const child = property(n, k);
      if (!child) {
        errors.push(`${at}.${k}: no such field`);
        continue;
      }
      walk(root, child, v, `${at}.${k}`, errors);
    }
    return;
  }
  const allowed = n.enum as unknown[] | undefined;
  if (allowed && typeof value === "string" && !allowed.includes(value)) {
    errors.push(`${at}: ${JSON.stringify(value)} is not one of ${allowed.join(", ")}`);
  }
}

/**
 * Checks that a JSON path (see jsonpath.ts) can exist in a message. Returns an error string, or
 * null when every segment names a field (wildcards and indexes need an array).
 */
export function checkPath(catalog: Catalog, message: string, segments: (string | number | "*")[]): string | null {
  const root = catalog.schema(message);
  if (!root) return `no generated schema for ${message}`;
  let node: Schema | undefined = root;
  let at = message;
  for (const seg of segments) {
    node = deref(root, node);
    if (!node) return `${at}: unresolvable schema`;
    if (seg === "*" || typeof seg === "number") {
      if (!types(node).includes("array")) return `${at}: indexed, but not an array`;
      node = node.items as Schema | undefined;
      at = `${at}[${seg}]`;
      continue;
    }
    const child = property(node, seg);
    if (!child) return `${at}.${seg}: no such field`;
    node = child;
    at = `${at}.${seg}`;
  }
  return null;
}
