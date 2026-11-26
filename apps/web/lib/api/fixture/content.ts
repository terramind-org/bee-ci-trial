/**
 * File contents for the demo repositories. Plain strings, no template literals, so content can
 * contain any characters. Versions are full file texts; history and blame are derived from them.
 */

export const POLICY_V1 = [
  "# .beehive/policy.toml: merge and access rules for acme/api.",
  "# Effective policy = org defaults overlaid by this file at the tip of the target ref.",
  "version = 1",
  "",
  "[groups]",
  'engineers = ["mara", "dev", "lin", "sam"]',
  'security = ["mara", "sam"]',
  "",
  "[[access]]",
  'path = "/"',
  'read = ["@acme/members"]',
  'write = ["@engineers", "@agents"]',
  "",
  "[[access]]",
  'path = "/src/auth"',
  'write = ["@security", "triage"]',
  "",
  "[[owners]]",
  'path = "/src/auth"',
  'owners = ["@security"]',
  "",
  "[[owners]]",
  'path = "/src/billing"',
  'owners = ["dev"]',
  "",
  "[[owners]]",
  'path = "/docs"',
  'owners = ["lin"]',
  "",
  "[[land]]",
  'ref = "main"',
  'required_checks = ["test", "lint"]',
  'agents_may_land_without_human = ["generated", "docs", "comment_only", "test_only", "rename_only"]',
  "",
  "[[land.approvals]]",
  'classes = ["dependency", "logic"]',
  'from = "owners"',
  "include_agents = true",
  "",
  "[[land.approvals]]",
  'classes = ["sensitive"]',
  "count = 2",
  "",
  "[paths]",
  'sensitive = ["/src/auth", "/.beehive", "/.github/workflows", "/migrations"]',
  'exclusive_lease = ["/src/auth"]',
  "",
].join("\n");

export const POLICY_V2 = POLICY_V1.replace(
  'exclusive_lease = ["/src/auth"]',
  'exclusive_lease = ["/src/auth", "/migrations"]',
);

export const AGENT_INSTRUCTIONS = [
  "Run `bun test` before opening a pull request. The required checks are `test` and `lint`.",
  "",
  "- Take an exclusive lease before editing `src/auth`.",
  "- Keep API behavior and the examples under `docs/` in sync.",
  "",
].join("\n");

export const README = [
  "# acme/api",
  "",
  "The billing and session API behind Acme Robotics' fleet console.",
  "",
  "## Working here",
  "",
  "- Agents and people merge through pull requests. Docs, tests and comment-only changes",
  "  can be merged by agents without a person's approval; logic needs an owner.",
  "- Take a lease before editing `src/auth`. It is exclusive and policy enforces it.",
  "- Run `bun test` before you merge. CI runs `test`, `lint` and `e2e`.",
  "",
  "## Layout",
  "",
  "| Path | What |",
  "|---|---|",
  "| `src/auth` | Sessions and bearer tokens |",
  "| `src/billing` | Invoices and tax |",
  "| `src/routes` | HTTP routes |",
  "| `docs` | API reference |",
  "",
].join("\n");

export const PACKAGE_V1 = [
  "{",
  '  "name": "@acme/api",',
  '  "private": true,',
  '  "type": "module",',
  '  "scripts": {',
  '    "dev": "bun --watch src/server.ts",',
  '    "test": "bun test",',
  '    "lint": "biome check ."',
  "  },",
  '  "dependencies": {',
  '    "hono": "4.5.11",',
  '    "zod": "3.23.8"',
  "  },",
  '  "devDependencies": {',
  '    "@biomejs/biome": "1.9.4"',
  "  }",
  "}",
  "",
].join("\n");

export const PACKAGE_V2 = PACKAGE_V1.replace('"hono": "4.5.11"', '"hono": "4.6.3"');
export const PACKAGE_ZOD4 = PACKAGE_V2.replace('"zod": "3.23.8"', '"zod": "4.1.0"');

function lock(hono: string, zod: string): string {
  return [
    "{",
    '  "lockfileVersion": 1,',
    '  "packages": {',
    '    "hono": ["hono@' + hono + '", "", {}, "sha512-' + hono.replace(/\./g, "") + 'Qm9vYmFy"],',
    '    "zod": ["zod@' + zod + '", "", {}, "sha512-' + zod.replace(/\./g, "") + 'WmFwWmFw"],',
    '    "@biomejs/biome": ["@biomejs/biome@1.9.4", "", {}, "sha512-QmlvbWU"]',
    "  }",
    "}",
    "",
  ].join("\n");
}
export const LOCK_V1 = lock("4.5.11", "3.23.8");
export const LOCK_V2 = lock("4.6.3", "3.23.8");
export const LOCK_ZOD4 = lock("4.6.3", "4.1.0");

export const CI_YML = [
  "name: ci",
  "on:",
  "  change_request:",
  "  land:",
  "    refs: [main]",
  "jobs:",
  "  test:",
  "    runs-on: linux-arm64",
  "    steps:",
  "      - uses: actions/checkout@v4",
  "      - run: bun install --frozen-lockfile",
  "      - run: bun test",
  "  lint:",
  "    runs-on: linux-arm64",
  "    steps:",
  "      - uses: actions/checkout@v4",
  "      - run: bun install --frozen-lockfile",
  "      - run: bun run lint",
  "  e2e:",
  "    runs-on: linux-arm64",
  "    needs: [test]",
  "    steps:",
  "      - uses: actions/checkout@v4",
  "      - run: bun install --frozen-lockfile",
  "      - run: bun run e2e",
  "",
].join("\n");

export const SERVER = [
  'import { Hono } from "hono";',
  'import { health } from "./routes/health";',
  'import { invoices } from "./routes/invoices";',
  'import { requireSession } from "./auth/session";',
  "",
  "const app = new Hono();",
  "",
  'app.route("/health", health);',
  'app.use("/v1/*", requireSession);',
  'app.route("/v1/invoices", invoices);',
  "",
  "export default {",
  "  port: Number(process.env.PORT ?? 8080),",
  "  fetch: app.fetch,",
  "};",
  "",
].join("\n");

export const HEALTH_V1 = [
  'import { Hono } from "hono";',
  "",
  "export const health = new Hono();",
  "",
  'health.get("/", (c) => c.json({ ok: true }));',
  "",
].join("\n");

export const HEALTH_V2 = [
  'import { Hono } from "hono";',
  "",
  "export const health = new Hono();",
  "",
  "// The build sha is stamped by CI so operators can tell which build answered.",
  'const BUILD_SHA = process.env.BUILD_SHA ?? "dev";',
  "",
  'health.get("/", (c) => c.json({ ok: true, build: BUILD_SHA }));',
  "",
].join("\n");

export const HEALTH_V3 = [
  'import { Hono } from "hono";',
  'import { histogram } from "../metrics";',
  "",
  "export const health = new Hono();",
  "",
  "// The build sha is stamped by CI so operators can tell which build answered.",
  'const BUILD_SHA = process.env.BUILD_SHA ?? "dev";',
  'const latency = histogram("http_health_latency_ms", [1, 5, 10, 25, 50, 100]);',
  "",
  'health.get("/", (c) => {',
  "  const started = performance.now();",
  "  const body = { ok: true, build: BUILD_SHA };",
  "  latency.observe(performance.now() - started);",
  "  return c.json(body);",
  "});",
  "",
].join("\n");

export const INVOICES_ROUTE = [
  'import { Hono } from "hono";',
  'import { z } from "zod";',
  'import { buildInvoice } from "../billing/invoice";',
  "",
  "export const invoices = new Hono();",
  "",
  "const Line = z.object({",
  "  sku: z.string().min(1),",
  "  quantity: z.number().int().positive(),",
  "  unitCents: z.number().int().nonnegative(),",
  "});",
  "",
  "const CreateInvoice = z.object({",
  "  customerId: z.string(),",
  "  region: z.string().length(2),",
  "  lines: z.array(Line).min(1),",
  "});",
  "",
  'invoices.post("/", async (c) => {',
  "  const input = CreateInvoice.safeParse(await c.req.json());",
  "  if (!input.success) {",
  '    return c.json({ error: "INVALID_INVOICE", issues: input.error.issues }, 400);',
  "  }",
  "  const invoice = buildInvoice(input.data);",
  "  return c.json(invoice, 201);",
  "});",
  "",
  'invoices.get("/:id", async (c) => {',
  '  const id = c.req.param("id");',
  '  const back = c.req.query("return_to");',
  "  if (back) return c.redirect(back);",
  "  return c.json({ id });",
  "});",
  "",
].join("\n");

export const SESSION_V1 = [
  'import type { MiddlewareHandler } from "hono";',
  'import { verifyToken } from "./tokens";',
  "",
  "export interface Session {",
  "  id: string;",
  "  userId: string;",
  '  role: "viewer" | "operator" | "admin";',
  "  lastSeen: number;",
  "}",
  "",
  "const sessions = new Map<string, Session>();",
  "",
  "export const requireSession: MiddlewareHandler = async (c, next) => {",
  '  const header = c.req.header("authorization") ?? "";',
  '  const token = header.replace(/^Bearer /, "");',
  "  const claims = await verifyToken(token);",
  '  if (!claims) return c.json({ error: "UNAUTHENTICATED" }, 401);',
  "  const session = sessions.get(claims.sid);",
  '  if (!session) return c.json({ error: "SESSION_EXPIRED" }, 401);',
  "  session.lastSeen = Date.now();",
  '  c.set("session", session);',
  "  await next();",
  "};",
  "",
].join("\n");

export const SESSION_V2 = [
  'import type { MiddlewareHandler } from "hono";',
  'import { verifyToken } from "./tokens";',
  "",
  "export interface Session {",
  "  id: string;",
  "  userId: string;",
  '  role: "viewer" | "operator" | "admin";',
  "  lastSeen: number;",
  "}",
  "",
  "// Idle sessions expire after 30 minutes.",
  "const IDLE_MS = 30 * 60 * 1000;",
  "const sessions = new Map<string, Session>();",
  "",
  "export const requireSession: MiddlewareHandler = async (c, next) => {",
  '  const header = c.req.header("authorization") ?? "";',
  '  const token = header.replace(/^Bearer /, "");',
  "  const claims = await verifyToken(token);",
  '  if (!claims) return c.json({ error: "UNAUTHENTICATED" }, 401);',
  "  const session = sessions.get(claims.sid);",
  "  if (!session || Date.now() - session.lastSeen > IDLE_MS) {",
  "    sessions.delete(claims.sid);",
  '    return c.json({ error: "SESSION_EXPIRED" }, 401);',
  "  }",
  "  session.lastSeen = Date.now();",
  '  c.set("session", session);',
  "  await next();",
  "};",
  "",
].join("\n");

/** CR "Rotate session tokens on privilege change", revision 1: rotates on every request. */
export const SESSION_CR_R1 = SESSION_V2.replace(
  "  session.lastSeen = Date.now();\n",
  [
    "  // Rotate the session id so a stolen id stops working.",
    "  sessions.delete(session.id);",
    "  session.id = crypto.randomUUID();",
    "  sessions.set(session.id, session);",
    "  session.lastSeen = Date.now();",
    "",
  ].join("\n"),
);

const ROTATE_FN = [
  "",
  "/** Called when a user's role changes. Issues a new session id and drops the old one. */",
  "export function rotateOnRoleChange(sessionId: string, role: Session[\"role\"]): Session | null {",
  "  const session = sessions.get(sessionId);",
  "  if (!session) return null;",
  "  sessions.delete(sessionId);",
  "  const next = { ...session, id: crypto.randomUUID(), role, lastSeen: Date.now() };",
  "  sessions.set(next.id, next);",
  "  return next;",
  "}",
  "",
].join("\n");

/** Revision 2: rotate only on role change. */
export const SESSION_CR_R2 = SESSION_V2 + ROTATE_FN;

export const TOKENS_V1 = [
  'import { jwtVerify, createRemoteJWKSet } from "jose";',
  "",
  'const JWKS = createRemoteJWKSet(new URL(process.env.JWKS_URL ?? "http://localhost:9000/jwks"));',
  "",
  "export interface Claims {",
  "  sub: string;",
  "  sid: string;",
  "}",
  "",
  "export async function verifyToken(token: string): Promise<Claims | null> {",
  "  try {",
  "    const { payload } = await jwtVerify(token, JWKS);",
  "    return { sub: String(payload.sub), sid: String(payload.sid) };",
  "  } catch {",
  "    return null;",
  "  }",
  "}",
  "",
].join("\n");

export const TOKENS_V2 = [
  'import { jwtVerify, createRemoteJWKSet } from "jose";',
  "",
  'const JWKS = createRemoteJWKSet(new URL(process.env.JWKS_URL ?? "http://localhost:9000/jwks"));',
  'const AUDIENCE = "acme-api";',
  "",
  "export interface Claims {",
  "  sub: string;",
  "  sid: string;",
  "}",
  "",
  "export async function verifyToken(token: string): Promise<Claims | null> {",
  "  try {",
  "    // Reject tokens minted for other Acme services.",
  "    const { payload } = await jwtVerify(token, JWKS, { audience: AUDIENCE });",
  "    return { sub: String(payload.sub), sid: String(payload.sid) };",
  "  } catch {",
  "    return null;",
  "  }",
  "}",
  "",
].join("\n");

export const INVOICE = [
  'import { taxLines } from "./tax";',
  "",
  "export interface InvoiceLine {",
  "  sku: string;",
  "  quantity: number;",
  "  unitCents: number;",
  "}",
  "",
  "export interface Invoice {",
  "  customerId: string;",
  "  lines: InvoiceLine[];",
  "  subtotalCents: number;",
  "  taxCents: number;",
  "  totalCents: number;",
  "}",
  "",
  "export function buildInvoice(input: { customerId: string; region: string; lines: InvoiceLine[] }): Invoice {",
  "  const subtotalCents = input.lines.reduce((sum, l) => sum + l.quantity * l.unitCents, 0);",
  "  const taxCents = taxLines(input.lines, input.region).reduce((sum, t) => sum + t.cents, 0);",
  "  return { customerId: input.customerId, lines: input.lines, subtotalCents, taxCents, totalCents: subtotalCents + taxCents };",
  "}",
  "",
].join("\n");

export const TAX_V1 = [
  'import type { InvoiceLine } from "./invoice";',
  "",
  "const RATES: Record<string, number> = { US: 0.0725, DE: 0.19, GB: 0.2, JP: 0.1 };",
  "",
  "export interface TaxLine {",
  "  sku: string;",
  "  cents: number;",
  "}",
  "",
  "export function taxLines(lines: InvoiceLine[], region: string): TaxLine[] {",
  "  const rate = RATES[region] ?? 0;",
  "  return lines.map((l) => ({",
  "    sku: l.sku,",
  "    cents: Math.round(l.quantity * l.unitCents * rate),",
  "  }));",
  "}",
  "",
].join("\n");

/** CR "Fix rounding in invoice tax lines", r1: rounds once on the total. */
export const TAX_CR_R1 = [
  'import type { InvoiceLine } from "./invoice";',
  "",
  "const RATES: Record<string, number> = { US: 0.0725, DE: 0.19, GB: 0.2, JP: 0.1 };",
  "",
  "export interface TaxLine {",
  "  sku: string;",
  "  cents: number;",
  "}",
  "",
  "export function taxLines(lines: InvoiceLine[], region: string): TaxLine[] {",
  "  const rate = RATES[region] ?? 0;",
  "  return lines.map((l) => ({",
  "    sku: l.sku,",
  "    cents: Math.floor(l.quantity * l.unitCents * rate),",
  "  }));",
  "}",
  "",
].join("\n");

/** r2: largest-remainder allocation so line taxes sum to the rounded total. */
export const TAX_CR_R2 = [
  'import type { InvoiceLine } from "./invoice";',
  "",
  "const RATES: Record<string, number> = { US: 0.0725, DE: 0.19, GB: 0.2, JP: 0.1 };",
  "",
  "export interface TaxLine {",
  "  sku: string;",
  "  cents: number;",
  "}",
  "",
  "/**",
  " * Per-line tax that always sums to the tax on the invoice total. Rounding each line on its",
  " * own drifted by a cent on multi-line invoices (issue #15).",
  " */",
  "export function taxLines(lines: InvoiceLine[], region: string): TaxLine[] {",
  "  const rate = RATES[region] ?? 0;",
  "  const exact = lines.map((l) => l.quantity * l.unitCents * rate);",
  "  const floors = exact.map(Math.floor);",
  "  const total = Math.round(exact.reduce((a, b) => a + b, 0));",
  "  let remainder = total - floors.reduce((a, b) => a + b, 0);",
  "  const order = exact",
  "    .map((x, i) => ({ i, frac: x - Math.floor(x) }))",
  "    .sort((a, b) => b.frac - a.frac || a.i - b.i);",
  "  for (const { i } of order) {",
  "    if (remainder <= 0) break;",
  "    floors[i] = (floors[i] ?? 0) + 1;",
  "    remainder--;",
  "  }",
  "  return lines.map((l, i) => ({ sku: l.sku, cents: floors[i] ?? 0 }));",
  "}",
  "",
].join("\n");

export const TAX_PDF = TAX_CR_R2.replace(
  "export function taxLines(",
  [
    "export function formatTaxLine(t: TaxLine): string {",
    '  return t.sku + "  " + (t.cents / 100).toFixed(2);',
    "}",
    "",
    "export function taxLines(",
  ].join("\n"),
);

export const BILLING_TEST_V1 = [
  'import { expect, test } from "bun:test";',
  'import { taxLines } from "../src/billing/tax";',
  "",
  'test("US tax on one line", () => {',
  '  expect(taxLines([{ sku: "arm", quantity: 1, unitCents: 10000 }], "US")).toEqual([{ sku: "arm", cents: 725 }]);',
  "});",
  "",
].join("\n");

export const BILLING_TEST_CR = [
  'import { expect, test } from "bun:test";',
  'import { taxLines } from "../src/billing/tax";',
  "",
  'test("US tax on one line", () => {',
  '  expect(taxLines([{ sku: "arm", quantity: 1, unitCents: 10000 }], "US")).toEqual([{ sku: "arm", cents: 725 }]);',
  "});",
  "",
  'test("line taxes sum to the tax on the total", () => {',
  "  const lines = [",
  '    { sku: "a", quantity: 1, unitCents: 333 },',
  '    { sku: "b", quantity: 1, unitCents: 333 },',
  '    { sku: "c", quantity: 1, unitCents: 334 },',
  "  ];",
  '  const sum = taxLines(lines, "DE").reduce((s, t) => s + t.cents, 0);',
  "  expect(sum).toBe(190);",
  "});",
  "",
].join("\n");

export const AUTH_TEST_V1 = [
  'import { expect, test } from "bun:test";',
  'import { verifyToken } from "../src/auth/tokens";',
  "",
  'test("rejects garbage", async () => {',
  '  expect(await verifyToken("not-a-token")).toBeNull();',
  "});",
  "",
].join("\n");

export const AUTH_TEST_CR = AUTH_TEST_V1 + [
  "",
  'test("role change issues a new session id", async () => {',
  '  const { rotateOnRoleChange } = await import("../src/auth/session");',
  '  expect(rotateOnRoleChange("missing", "viewer")).toBeNull();',
  "});",
  "",
].join("\n");

export const DOCS_API_V1 = [
  "# API reference",
  "",
  "All endpoints live under `/v1` and need a bearer token.",
  "",
  "## POST /v1/invoices",
  "",
  "Creates an invoice. Body: `customerId`, `region` (ISO 3166 alpha-2), `lines[]`.",
  "",
  "## GET /v1/invoices/:id",
  "",
  "Returns one invoice.",
  "",
].join("\n");

export const DOCS_API_V2 = DOCS_API_V1 + [
  "## Rate limits",
  "",
  "Every response carries `RateLimit-Limit`, `RateLimit-Remaining` and `RateLimit-Reset`.",
  "A 429 includes `Retry-After` in seconds.",
  "",
].join("\n");

export const DOCS_WEBHOOKS_V1 = [
  "# Webhooks",
  "",
  "Acme sends `invoice.created` and `invoice.paid` to your endpoint.",
  "",
].join("\n");

export const DOCS_WEBHOOKS_V2 = [
  "# Webhooks",
  "",
  "Acme sends `invoice.created` and `invoice.paid` to your endpoint.",
  "",
  "## Delivery",
  "",
  "Deliveries are at least once. Each carries an `Acme-Signature` header: an HMAC-SHA256 of the",
  "raw body with your endpoint secret. Deduplicate on the `id` field.",
  "",
].join("\n");

export const DOCS_WEBHOOKS_CR = DOCS_WEBHOOKS_V2 + [
  "## Retries",
  "",
  "A delivery that does not get a 2xx within 10 seconds is retried with exponential backoff:",
  "1 minute, 5 minutes, 30 minutes, 2 hours, then every 6 hours for 3 days. After that the",
  "delivery is marked failed and shows in the dashboard with its last response.",
  "",
].join("\n");

export const METRICS = [
  "export interface Histogram {",
  "  observe(value: number): void;",
  "}",
  "",
  "export function histogram(name: string, buckets: number[]): Histogram {",
  "  const counts = new Array(buckets.length + 1).fill(0);",
  "  return {",
  "    observe(value) {",
  "      const i = buckets.findIndex((b) => value <= b);",
  "      counts[i === -1 ? buckets.length : i]++;",
  "    },",
  "  };",
  "}",
  "",
].join("\n");

export const CACHE_TOTALS = INVOICE.replace(
  "export function buildInvoice(",
  [
    "const totals = new Map<string, number>();",
    "",
    "export function customerTotal(customerId: string): number {",
    "  return totals.get(customerId) ?? 0;",
    "}",
    "",
    "export function buildInvoice(",
  ].join("\n"),
).replace(
  "  return { customerId: input.customerId,",
  "  totals.set(input.customerId, customerTotal(input.customerId) + subtotalCents + taxCents);\n  return { customerId: input.customerId,",
);

// ---------------------------------------------------------------- acme/web

export const WEB_README = [
  "# acme/web",
  "",
  "The fleet console. Next.js, talks to acme/api.",
  "",
].join("\n");

export const WEB_PAGE_V1 = [
  'import { Fleet } from "../components/fleet";',
  "",
  "export default function Page() {",
  "  return <Fleet />;",
  "}",
  "",
].join("\n");

export const WEB_FLEET_V1 = [
  "export function Fleet() {",
  "  return (",
  "    <section>",
  "      <h1>Fleet</h1>",
  "      <p>No robots connected.</p>",
  "    </section>",
  "  );",
  "}",
  "",
].join("\n");

export const WEB_FLEET_V2 = [
  'import { useRobots } from "../lib/robots";',
  "",
  "export function Fleet() {",
  "  const robots = useRobots();",
  "  return (",
  "    <section>",
  "      <h1>Fleet</h1>",
  "      {robots.length === 0 ? <p>No robots connected.</p> : null}",
  "      <ul>",
  "        {robots.map((r) => (",
  "          <li key={r.id}>{r.name}</li>",
  "        ))}",
  "      </ul>",
  "    </section>",
  "  );",
  "}",
  "",
].join("\n");

export const WEB_ROBOTS = [
  "export interface Robot {",
  "  id: string;",
  "  name: string;",
  "}",
  "",
  "export function useRobots(): Robot[] {",
  "  return [];",
  "}",
  "",
].join("\n");

/** The sessions list a change request adds to acme/web, as first written (r1). */
export const WEB_SESSIONS_R1 = [
  "export interface SessionRow {",
  "  id: string;",
  "  device: string;",
  "  current: boolean;",
  "}",
  "",
  "export function SessionsList({ rows }: { rows: SessionRow[] }) {",
  "  return (",
  '    <ul className="sessions">',
  "      {rows.map((row) => (",
  "        <li key={row.id}>{row.device}</li>",
  "      ))}",
  "    </ul>",
  "  );",
  "}",
  "",
].join("\n");

/** r2: the current session is marked and the others can be signed out. */
export const WEB_SESSIONS_R2 = [
  "export interface SessionRow {",
  "  id: string;",
  "  device: string;",
  "  current: boolean;",
  "}",
  "",
  "export function SessionsList({ rows, onSignOut }: { rows: SessionRow[]; onSignOut: (id: string) => void }) {",
  "  return (",
  '    <ul className="sessions">',
  "      {rows.map((row) => (",
  '        <li key={row.id} aria-current={row.current ? "true" : undefined}>',
  "          {row.device}",
  '          {row.current ? <span className="badge">This device</span> : <button onClick={() => onSignOut(row.id)}>Sign out</button>}',
  "        </li>",
  "      ))}",
  "    </ul>",
  "  );",
  "}",
  "",
].join("\n");

// ---------------------------------------------------------------- acme/infra

export const INFRA_README = ["# acme/infra", "", "Terraform for the Acme cells.", ""].join("\n");
export const INFRA_MAIN_V1 = [
  'module "cell_eu1" {',
  '  source = "./modules/cell"',
  '  region = "eu-central-1"',
  "  size   = 3",
  "}",
  "",
].join("\n");
export const INFRA_MAIN_V2 = INFRA_MAIN_V1 + [
  "",
  'module "cell_us1" {',
  '  source = "./modules/cell"',
  '  region = "us-east-2"',
  "  size   = 3",
  "}",
  "",
].join("\n");
