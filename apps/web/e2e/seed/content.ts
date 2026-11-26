/** File contents the seed commits: a small invoicing service with docs, CI and a policy. */

export const README = `# playground

Invoicing service used to demo Beehive: agents and people land changes on one branch.

## Layout

- \`src/billing\`: invoices and tax lines
- \`src/auth\`: sessions (sensitive: two human approvals to land)
- \`docs\`: operator docs, agents may land these alone

## Running

\`\`\`sh
bun install
bun test
\`\`\`
`;

export const PACKAGE_JSON = (zod: string) => `{
  "name": "playground",
  "version": "0.1.0",
  "private": true,
  "type": "module",
  "dependencies": {
    "hono": "4.6.3",
    "zod": "${zod}"
  }
}
`;

export const INVOICE_V1 = `import { taxFor } from "./tax";

export interface Line {
  sku: string;
  quantity: number;
  unitCents: number;
  region: string;
}

export interface Invoice {
  customer: string;
  lines: Line[];
}

/** Total of one line before tax, in cents. */
export function lineTotal(line: Line): number {
  return line.quantity * line.unitCents;
}

/** Invoice total in cents, tax included. */
export function invoiceTotal(invoice: Invoice): number {
  let total = 0;
  for (const line of invoice.lines) {
    const net = lineTotal(line);
    total += net + net * taxFor(line.region);
  }
  return total;
}
`;

/** Rounds tax per line: the fix for the rounding drift. */
export const INVOICE_FIX_R1 = INVOICE_V1.replace("    total += net + net * taxFor(line.region);", "    total += net + Math.round(net * taxFor(line.region));");

export const INVOICE_FIX_R2 = INVOICE_FIX_R1.replace(
  "/** Invoice total in cents, tax included. */",
  "/** Invoice total in cents, tax included. Tax is rounded per line, as on the printed invoice. */",
);

export const TAX = `const RATES: Record<string, number> = {
  "EU-DE": 0.19,
  "EU-FR": 0.2,
  "US-CA": 0.0725,
  "US-NY": 0.08875,
};

/** Tax rate for a region; unknown regions are not taxed. */
export function taxFor(region: string): number {
  return RATES[region] ?? 0;
}
`;

export const TAX_V2 = TAX.replace('  "US-NY": 0.08875,\n', '  "US-NY": 0.08875,\n  "US-TX": 0.0625,\n');

export const SESSION_V1 = `import { randomUUID } from "node:crypto";

export interface Session {
  id: string;
  userId: string;
  role: "member" | "admin";
}

const sessions = new Map<string, Session>();

export function createSession(userId: string, role: Session["role"]): Session {
  const s = { id: randomUUID(), userId, role };
  sessions.set(s.id, s);
  return s;
}

export function changeRole(id: string, role: Session["role"]): Session | undefined {
  const s = sessions.get(id);
  if (!s) return undefined;
  s.role = role;
  return s;
}
`;

export const SESSION_ROTATE = SESSION_V1.replace(
  `  const s = sessions.get(id);
  if (!s) return undefined;
  s.role = role;
  return s;`,
  `  const s = sessions.get(id);
  if (!s) return undefined;
  // A role change rotates the session id so a stolen id loses its new rights.
  sessions.delete(id);
  const next = { ...s, id: randomUUID(), role };
  sessions.set(next.id, next);
  return next;`,
);

export const HEALTH = `export function health(): { ok: true; version: string } {
  return { ok: true, version: "0.1.0" };
}
`;

export const WEBHOOKS_V1 = `# Webhooks

Beehive posts JSON to your endpoint for each event you subscribe to.

## Delivery

Each delivery carries an \`X-Bee-Signature\` header (HMAC-SHA256 of the body).
`;

export const WEBHOOKS_V2 =
  WEBHOOKS_V1 +
  `
## Retries

Failed deliveries are retried with exponential backoff: 1, 2, 4, 8 and 16 minutes, then the
delivery is marked failed and shown in the hook's delivery log.
`;

export const CI = `name: ci
on:
  push:
    branches: [main]
  pull_request:
jobs:
  test:
    runs-on: [self-hosted, seed]
    steps:
      - name: Install
        run: bun install
      - name: Unit tests
        run: bun test
  lint:
    runs-on: [self-hosted, seed]
    steps:
      - name: Lint
        run: bun run lint
`;

/** Agents land docs alone; sensitive paths need two human approvals; logic needs one. */
export const POLICY = `version = 1

[members]
admin = ["@humans"]
write = ["@humans", "@agents"]
read = ["@everyone"]

[[access]]
path = "/"
read = ["@everyone"]
write = ["@humans", "@agents"]

[[land]]
ref = "main"
agents_may_land_without_human = ["docs", "test_only", "comment_only", "dependency"]

[[land.approvals]]
classes = ["sensitive"]
count = 2
include_agents = false

[[land.approvals]]
classes = ["logic"]
count = 1
include_agents = false

[paths]
sensitive = ["/src/auth", "/.beehive"]
`;

export const CACHE = `import { invoiceTotal, type Invoice } from "./invoice";

const totals = new Map<string, number>();

export function cachedTotal(invoice: Invoice): number {
  const hit = totals.get(invoice.customer);
  if (hit !== undefined) return hit;
  const total = invoiceTotal(invoice);
  totals.set(invoice.customer, total);
  return total;
}

export function invalidate(customer: string): void {
  totals.delete(customer);
}
`;

export const MONEY = `import Decimal from "decimal.js";

export const cents = (amount: string) => new Decimal(amount).mul(100).toNumber();
`;

/** A fake credential committed by mistake (secret scanning should flag it). */
export const LEAKED_CONFIG = `# local settings, do not commit
region = "us-east-1"
aws_access_key_id = "AKIAQX4JQ7PZRKV3M2NA"
aws_secret_access_key = "vQ8sJm3Tn6Wb2Lr9Xc4Zp7Ye1Hd5Gk0Fa8Uo3Ri"
`;

/** The sessions list of the settings page, as first written (r1): every row reads the same. */
export const SESSIONS_TSX_R1 = `export interface SessionRow {
  id: string;
  device: string;
  current: boolean;
}

export function SessionsList({ rows }: { rows: SessionRow[] }) {
  return (
    <ul className="sessions">
      {rows.map((row) => (
        <li key={row.id}>{row.device}</li>
      ))}
    </ul>
  );
}
`;

/** r2: the current session is marked and the others can be signed out. */
export const SESSIONS_TSX_R2 = `export interface SessionRow {
  id: string;
  device: string;
  current: boolean;
}

export function SessionsList({ rows, onSignOut }: { rows: SessionRow[]; onSignOut: (id: string) => void }) {
  return (
    <ul className="sessions">
      {rows.map((row) => (
        <li key={row.id} aria-current={row.current ? "true" : undefined}>
          {row.device}
          {row.current ? <span className="badge">This device</span> : <button onClick={() => onSignOut(row.id)}>Sign out</button>}
        </li>
      ))}
    </ul>
  );
}
`;
