/**
 * The demo world's static facts: who exists, which repos, the history of main, and the
 * change requests in flight. `store.ts` turns these into live state.
 */
import * as C from "./content";
import type { PrincipalKind } from "../types";
import { POST_CRS } from "./posts-world";
import { ACME_TOPICS, COMMUNITY_CRS, COMMUNITY_HISTORY, COMMUNITY_PRINCIPALS, COMMUNITY_REPOS } from "./community-world";
import { POLICY_PATH } from "./policy";

export interface PrincipalSeed {
  handle: string;
  kind: PrincipalKind;
  display: string;
  owner?: string;
  runtime?: string;
  purpose?: string;
  model?: string;
  github?: string;
  role?: "ROLE_OWNER" | "ROLE_ADMIN" | "ROLE_MEMBER";
  /** The org a human belongs to; acme when omitted. */
  org?: string;
}

const ACME_PRINCIPALS: PrincipalSeed[] = [
  { handle: "mara", kind: "PRINCIPAL_KIND_HUMAN", display: "Mara Okafor", github: "4417203", role: "ROLE_OWNER" },
  { handle: "dev", kind: "PRINCIPAL_KIND_HUMAN", display: "Dev Patel", github: "981552", role: "ROLE_ADMIN" },
  { handle: "lin", kind: "PRINCIPAL_KIND_HUMAN", display: "Lin Zhou", github: "2210984", role: "ROLE_MEMBER" },
  { handle: "sam", kind: "PRINCIPAL_KIND_HUMAN", display: "Sam Reyes", github: "7730021", role: "ROLE_MEMBER" },
  {
    handle: "triage",
    kind: "PRINCIPAL_KIND_AGENT",
    display: "triage",
    owner: "mara",
    runtime: "claude-code",
    model: "claude-opus-5",
    purpose: "Fixes bugs filed against acme/api; holds the src/auth lease while it works.",
  },
  {
    handle: "billing",
    kind: "PRINCIPAL_KIND_AGENT",
    display: "billing-fixer",
    owner: "dev",
    runtime: "codex",
    model: "gpt-5-codex",
    purpose: "Small, test-first fixes in src/billing.",
  },
  {
    handle: "docs",
    kind: "PRINCIPAL_KIND_AGENT",
    display: "docs-writer",
    owner: "lin",
    runtime: "claude-code",
    model: "claude-sonnet-5",
    purpose: "Keeps docs/ in step with the code; lands docs-only changes on its own.",
  },
  {
    handle: "deps",
    kind: "PRINCIPAL_KIND_AGENT",
    display: "deps-bot",
    owner: "dev",
    runtime: "codex",
    model: "gpt-5-codex-mini",
    purpose: "Dependency and security updates.",
  },
  {
    handle: "watcher",
    kind: "PRINCIPAL_KIND_AGENT",
    display: "latency-watcher",
    owner: "mara",
    runtime: "claude-code",
    model: "claude-haiku-5",
    purpose: "Component watcher for src/routes: reads latency telemetry, files issues with evidence.",
  },
  {
    handle: "reviewer",
    kind: "PRINCIPAL_KIND_AGENT",
    display: "first-pass",
    owner: "mara",
    runtime: "claude-code",
    model: "claude-sonnet-5",
    purpose: "First-pass review on every pull request. Its approvals never count toward rules.",
  },
  { handle: "nightly-import", kind: "PRINCIPAL_KIND_AGENT", display: "nightly-import", owner: "ci" },
  { handle: "ci", kind: "PRINCIPAL_KIND_SERVICE", display: "bee-ci" },
  { handle: "importer", kind: "PRINCIPAL_KIND_SERVICE", display: "github-importer" },
];

export const PRINCIPALS: PrincipalSeed[] = [...ACME_PRINCIPALS, ...COMMUNITY_PRINCIPALS];

export interface RepoSeed {
  /** Short name; the store keys repos of orgs other than acme by "owner/name". */
  name: string;
  /** Owning org; acme when omitted. */
  owner?: string;
  description: string;
  topics?: string[];
  /** SPDX id of the license, "" when there is none. */
  license?: string;
  homepage?: string;
  visibility: "VISIBILITY_PRIVATE" | "VISIBILITY_INTERNAL" | "VISIBILITY_PUBLIC";
  restricted: string[];
}

const ACME_REPOS: RepoSeed[] = [
  { name: "api", description: "Billing and session API for the fleet console", visibility: "VISIBILITY_PRIVATE", restricted: ["ops/secrets.enc.yaml"] },
  { name: "web", description: "Fleet console (Next.js)", visibility: "VISIBILITY_INTERNAL", restricted: [] },
  { name: "infra", description: "Terraform for Acme cells", visibility: "VISIBILITY_PRIVATE", restricted: [] },
];

export const REPOS: RepoSeed[] = [...ACME_REPOS.map((r) => ({ ...r, topics: ACME_TOPICS[r.name] ?? [] })), ...COMMUNITY_REPOS];

export interface CommitSeed {
  repo: string;
  author: string;
  ago: number; // minutes before the anchor
  message: string;
  files: Record<string, string | null>;
  session?: string;
  costCents?: number;
}

/** History of main, oldest first, per repo. */
const ACME_HISTORY: CommitSeed[] = [
  {
    repo: "api",
    author: "importer",
    ago: 43_200,
    message: "Import github:acme/api",
    files: {
      "README.md": C.README,
      "package.json": C.PACKAGE_V1,
      "bun.lock": C.LOCK_V1,
      [POLICY_PATH]: C.POLICY_V1,
      "AGENTS.md": C.AGENT_INSTRUCTIONS,
      ".github/workflows/ci.yml": C.CI_YML,
      "src/server.ts": C.SERVER,
      "src/routes/health.ts": C.HEALTH_V1,
      "src/auth/session.ts": C.SESSION_V1,
      "src/auth/tokens.ts": C.TOKENS_V1,
      "src/billing/invoice.ts": C.INVOICE,
      "src/metrics.ts": C.METRICS,
      "tests/auth.test.ts": C.AUTH_TEST_V1,
      "docs/api.md": C.DOCS_API_V1,
      "docs/webhooks.md": C.DOCS_WEBHOOKS_V1,
      "ops/secrets.enc.yaml": "sops:\n  kms: arn:aws:kms:eu-central-1:000000000000:key/redacted\n",
    },
  },
  { repo: "api", author: "dev", ago: 28_800, message: "Add invoice routes", files: { "src/routes/invoices.ts": C.INVOICES_ROUTE } },
  {
    repo: "api",
    author: "triage",
    ago: 17_280,
    message: "Expire idle sessions after 30 minutes",
    files: { "src/auth/session.ts": C.SESSION_V2 },
    session: "ses_01JA9triage7f2",
    costCents: 41,
  },
  {
    repo: "api",
    author: "docs",
    ago: 12_960,
    message: "docs: webhook delivery guarantees",
    files: { "docs/webhooks.md": C.DOCS_WEBHOOKS_V2 },
    session: "ses_01JA9docs0c31",
    costCents: 6,
  },
  {
    repo: "api",
    author: "billing",
    ago: 8_640,
    message: "Add per-line tax computation",
    files: { "src/billing/tax.ts": C.TAX_V1, "tests/billing.test.ts": C.BILLING_TEST_V1 },
    session: "ses_01JAAbill92e0",
    costCents: 58,
  },
  {
    repo: "api",
    author: "deps",
    ago: 5_760,
    message: "Bump hono from 4.5.11 to 4.6.3",
    files: { "package.json": C.PACKAGE_V2, "bun.lock": C.LOCK_V2 },
    session: "ses_01JABdeps1a77",
    costCents: 3,
  },
  { repo: "api", author: "mara", ago: 4_320, message: "Health route: report build sha", files: { "src/routes/health.ts": C.HEALTH_V2 } },
  {
    repo: "api",
    author: "triage",
    ago: 1_440,
    message: "Add health route latency histogram",
    files: { "src/routes/health.ts": C.HEALTH_V3 },
    session: "ses_01JACtriage0b9",
    costCents: 37,
  },
  {
    repo: "api",
    author: "docs",
    ago: 300,
    message: "docs: rate limit headers",
    files: { "docs/api.md": C.DOCS_API_V2 },
    session: "ses_01JADdocs77a1",
    costCents: 5,
  },
  { repo: "api", author: "mara", ago: 120, message: "Tighten token audience check", files: { "src/auth/tokens.ts": C.TOKENS_V2 } },

  { repo: "web", author: "importer", ago: 43_000, message: "Import github:acme/web", files: { "README.md": C.WEB_README, "app/page.tsx": C.WEB_PAGE_V1, "components/fleet.tsx": C.WEB_FLEET_V1 } },
  { repo: "web", author: "lin", ago: 2_000, message: "Fleet list reads connected robots", files: { "components/fleet.tsx": C.WEB_FLEET_V2, "lib/robots.ts": C.WEB_ROBOTS } },

  { repo: "infra", author: "importer", ago: 42_000, message: "Import github:acme/infra", files: { "README.md": C.INFRA_README, "main.tf": C.INFRA_MAIN_V1 } },
];

export const HISTORY: CommitSeed[] = [...ACME_HISTORY, ...COMMUNITY_HISTORY];

export interface ReviewSeed {
  by: string;
  rev: number;
  verdict: "VERDICT_APPROVE" | "VERDICT_REQUEST_CHANGES" | "VERDICT_COMMENT";
  body: string;
  ago: number;
}

export interface ThreadSeed {
  key: string;
  rev: number;
  path: string;
  start: number;
  end: number;
  resolved: boolean;
  outdated?: boolean;
  comments: { by: string; body: string; suggestion?: string; ago: number }[];
}

export interface RevisionSeed {
  ago: number;
  files: Record<string, string | null>;
  /** Base commit index on main (in HISTORY order for the repo); defaults to the tip at that time. */
  base: number;
  contentChanged: boolean;
  checks: Record<string, "CHECK_STATUS_PASSED" | "CHECK_STATUS_FAILED" | "CHECK_STATUS_RUNNING" | "CHECK_STATUS_QUEUED">;
  session?: string;
  costCents?: number;
}

export interface CrSeed {
  key: string;
  repo: string;
  author: string;
  title: string;
  body: string;
  ago: number;
  state: "CHANGE_STATE_OPEN" | "CHANGE_STATE_LANDED" | "CHANGE_STATE_CLOSED";
  revisions: RevisionSeed[];
  reviews: ReviewSeed[];
  threads: ThreadSeed[];
  dependsOn?: string;
  /** For landed CRs: the main commit index (in repo history order) it became. */
  landedAs?: number;
  queued?: "waiting" | "testing";
  ws: string;
}

const PASS = { test: "CHECK_STATUS_PASSED", lint: "CHECK_STATUS_PASSED", e2e: "CHECK_STATUS_PASSED" } as const;

const ACME_CRS: CrSeed[] = [
  {
    key: "rotate",
    repo: "api",
    author: "triage",
    title: "Rotate session ids on role change",
    body: [
      "Fixes #13 (part of #12).",
      "",
      "A session kept its id after a role downgrade, so a token issued while the user was an admin",
      "kept working with admin claims until idle expiry. `rotateOnRoleChange` issues a new id and",
      "drops the old one; the role service calls it.",
      "",
      "Local checks: `bun test` (14 passed), `bun run lint`.",
    ].join("\n"),
    ago: 2_900,
    state: "CHANGE_STATE_OPEN",
    ws: "ws/triage/rotate-sessions",
    revisions: [
      { ago: 2_900, base: 7, files: { "src/auth/session.ts": C.SESSION_CR_R1 }, contentChanged: true, checks: PASS, session: "ses_01JACtriage4d0", costCents: 52 },
      {
        ago: 1_300,
        base: 7,
        files: { "src/auth/session.ts": C.SESSION_CR_R2, "tests/auth.test.ts": C.AUTH_TEST_CR },
        contentChanged: true,
        checks: PASS,
        session: "ses_01JACtriage4d0",
        costCents: 33,
      },
      {
        ago: 90,
        base: 9,
        files: { "src/auth/session.ts": C.SESSION_CR_R2, "tests/auth.test.ts": C.AUTH_TEST_CR },
        contentChanged: false,
        checks: { test: "CHECK_STATUS_PASSED", lint: "CHECK_STATUS_PASSED", e2e: "CHECK_STATUS_RUNNING" },
        session: "ses_01JACtriage4d0",
        costCents: 2,
      },
    ],
    reviews: [
      { by: "reviewer", rev: 1, verdict: "VERDICT_COMMENT", body: "Rotation runs on every request; that will log users out of other tabs.", ago: 2_850 },
      { by: "mara", rev: 1, verdict: "VERDICT_REQUEST_CHANGES", body: "Only rotate when the role changes. Rotating per request breaks multi-tab use.", ago: 2_400 },
      { by: "dev", rev: 2, verdict: "VERDICT_APPROVE", body: "Looks right. One small typing suggestion inline.", ago: 1_100 },
    ],
    threads: [
      {
        key: "t1",
        rev: 1,
        path: "src/auth/session.ts",
        start: 25,
        end: 28,
        resolved: true,
        outdated: true,
        comments: [
          { by: "mara", body: "This rotates on every request. Rotate only when the role changes.", ago: 2_400 },
          { by: "triage", body: "Done in r2: rotation moved to `rotateOnRoleChange`, called by the role service.", ago: 1_300 },
        ],
      },
      {
        key: "t2",
        rev: 2,
        path: "src/auth/session.ts",
        start: 35,
        end: 35,
        resolved: false,
        comments: [
          {
            by: "dev",
            body: "Annotate the type so a future field on Session is not silently dropped.",
            suggestion: "  const next: Session = { ...session, id: crypto.randomUUID(), role, lastSeen: Date.now() };",
            ago: 1_100,
          },
        ],
      },
      {
        key: "t3",
        rev: 2,
        path: "src/auth/session.ts",
        start: 31,
        end: 31,
        resolved: false,
        comments: [{ by: "reviewer", body: "Consider writing an audit entry when a session rotates (tracked in #14).", ago: 1_250 }],
      },
    ],
  },
  {
    key: "tax",
    repo: "api",
    author: "billing",
    title: "Fix rounding drift in invoice tax lines",
    body: "Fixes #15. Line taxes are allocated with largest remainder so they always sum to the tax on the total. Adds a regression test with three lines that used to drift by a cent.",
    ago: 1_900,
    state: "CHANGE_STATE_OPEN",
    ws: "ws/billing/tax-rounding",
    revisions: [
      { ago: 1_900, base: 8, files: { "src/billing/tax.ts": C.TAX_CR_R1 }, contentChanged: true, checks: { test: "CHECK_STATUS_FAILED", lint: "CHECK_STATUS_PASSED", e2e: "CHECK_STATUS_PASSED" }, session: "ses_01JACbill11f3", costCents: 64 },
      { ago: 700, base: 9, files: { "src/billing/tax.ts": C.TAX_CR_R2, "tests/billing.test.ts": C.BILLING_TEST_CR }, contentChanged: true, checks: PASS, session: "ses_01JACbill11f3", costCents: 71 },
    ],
    reviews: [
      { by: "reviewer", rev: 1, verdict: "VERDICT_REQUEST_CHANGES", body: "Flooring every line undercharges; the regression test fails on DE.", ago: 1_850 },
      { by: "dev", rev: 2, verdict: "VERDICT_APPROVE", body: "Largest remainder is the right call. Ship it.", ago: 400 },
    ],
    threads: [
      {
        key: "t4",
        rev: 2,
        path: "src/billing/tax.ts",
        start: 20,
        end: 22,
        resolved: true,
        comments: [
          { by: "dev", body: "Tie-break on index so the allocation is deterministic.", ago: 600 },
          { by: "billing", body: "It does: `|| a.i - b.i`.", ago: 590 },
        ],
      },
    ],
  },
  {
    key: "webhooks",
    repo: "api",
    author: "docs",
    title: "Document the webhook retry schedule",
    body: "Closes #17. Writes down the backoff schedule the delivery worker already uses.",
    ago: 240,
    state: "CHANGE_STATE_OPEN",
    ws: "ws/docs/webhook-retries",
    queued: "testing",
    revisions: [{ ago: 240, base: 9, files: { "docs/webhooks.md": C.DOCS_WEBHOOKS_CR }, contentChanged: true, checks: PASS, session: "ses_01JADdocs90aa", costCents: 4 }],
    reviews: [{ by: "reviewer", rev: 1, verdict: "VERDICT_APPROVE", body: "Matches the worker's schedule in the delivery service.", ago: 230 }],
    threads: [],
  },
  {
    key: "zod",
    repo: "api",
    author: "deps",
    title: "Bump zod from 3.23.8 to 4.1.0",
    body: "Resolves security alert GHSA-zod-email-redos. Major version: `error.issues` shape changed; tests flag one call site.",
    ago: 600,
    state: "CHANGE_STATE_OPEN",
    ws: "ws/deps/zod-4",
    revisions: [
      { ago: 600, base: 9, files: { "package.json": C.PACKAGE_ZOD4, "bun.lock": C.LOCK_ZOD4 }, contentChanged: true, checks: { test: "CHECK_STATUS_FAILED", lint: "CHECK_STATUS_PASSED", e2e: "CHECK_STATUS_QUEUED" }, session: "ses_01JADdeps3c12", costCents: 2 },
    ],
    reviews: [],
    threads: [],
  },
  {
    key: "pdf",
    repo: "api",
    author: "billing",
    title: "Show the tax breakdown on invoice PDFs",
    body: "Stacked on the rounding fix. Adds `formatTaxLine` for the PDF renderer. Part of #16.",
    ago: 180,
    state: "CHANGE_STATE_OPEN",
    dependsOn: "tax",
    ws: "ws/billing/pdf-tax",
    revisions: [
      { ago: 180, base: 9, files: { "src/billing/tax.ts": C.TAX_PDF, "tests/billing.test.ts": C.BILLING_TEST_CR }, contentChanged: true, checks: { test: "CHECK_STATUS_RUNNING", lint: "CHECK_STATUS_PASSED", e2e: "CHECK_STATUS_QUEUED" }, session: "ses_01JADbill5501", costCents: 29 },
    ],
    reviews: [],
    threads: [],
  },
  {
    key: "histogram",
    repo: "api",
    author: "triage",
    title: "Add health route latency histogram",
    body: "Requested by latency-watcher in #18 so it can tell a slow route from a slow host.",
    ago: 1_600,
    state: "CHANGE_STATE_LANDED",
    landedAs: 7,
    ws: "ws/triage/health-histogram",
    revisions: [{ ago: 1_600, base: 6, files: { "src/routes/health.ts": C.HEALTH_V3 }, contentChanged: true, checks: PASS, session: "ses_01JACtriage0b9", costCents: 37 }],
    reviews: [{ by: "mara", rev: 1, verdict: "VERDICT_APPROVE", body: "Good.", ago: 1_500 }],
    threads: [],
  },
  {
    key: "retry-forever",
    repo: "api",
    author: "docs",
    title: "Retry webhook deliveries forever",
    body: "Proposes unbounded retries.",
    ago: 3_000,
    state: "CHANGE_STATE_CLOSED",
    ws: "ws/docs/retry-forever",
    revisions: [{ ago: 3_000, base: 8, files: { "docs/webhooks.md": C.DOCS_WEBHOOKS_V2 + "\nDeliveries are retried until they succeed.\n" }, contentChanged: true, checks: PASS }],
    reviews: [{ by: "lin", rev: 1, verdict: "VERDICT_REQUEST_CHANGES", body: "That is not what the worker does. Closing; see the retry-schedule change instead.", ago: 2_950 }],
    threads: [],
  },
  {
    key: "cache",
    repo: "api",
    author: "lin",
    title: "Cache invoice totals per customer",
    body: "Part of #20. In-memory for now; a follow-up moves it to Redis.",
    ago: 400,
    state: "CHANGE_STATE_OPEN",
    ws: "ws/lin/invoice-cache",
    revisions: [{ ago: 400, base: 9, files: { "src/billing/invoice.ts": C.CACHE_TOTALS }, contentChanged: true, checks: PASS }],
    reviews: [{ by: "reviewer", rev: 1, verdict: "VERDICT_COMMENT", body: "The cache never evicts; fine for a demo, unbounded in production.", ago: 390 }],
    threads: [
      {
        key: "t5",
        rev: 1,
        path: "src/billing/invoice.ts",
        start: 17,
        end: 17,
        resolved: false,
        comments: [{ by: "reviewer", body: "Unbounded map. Add an LRU or a TTL before this goes to production.", ago: 390 }],
      },
    ],
  },
  {
    key: "policy-migrations",
    repo: "api",
    author: "mara",
    title: "Policy: require an exclusive lease for migrations",
    body: "Two agents edited the same migration last week. Make `/migrations` exclusive like `/src/auth`.",
    ago: 60,
    state: "CHANGE_STATE_OPEN",
    ws: "ws/mara/policy-migrations",
    revisions: [{ ago: 60, base: 9, files: { [POLICY_PATH]: C.POLICY_V2 }, contentChanged: true, checks: PASS }],
    reviews: [],
    threads: [],
  },
  {
    key: "web-robots",
    repo: "web",
    author: "lin",
    title: "Poll the robots endpoint every 5 seconds",
    body: "Until the event stream lands in acme/api.",
    ago: 500,
    state: "CHANGE_STATE_OPEN",
    ws: "ws/lin/robots-poll",
    revisions: [{ ago: 500, base: 1, files: { "lib/robots.ts": C.WEB_ROBOTS.replace("return [];", "return usePoll<Robot[]>(\"/v1/robots\", 5000) ?? [];") }, contentChanged: true, checks: PASS }],
    reviews: [],
    threads: [],
  },
  {
    key: "ui",
    repo: "web",
    author: "triage",
    title: "Mark the current session in the sessions list",
    body: "The settings page listed every signed-in session the same way. Mark this device and let people sign out of the others.",
    ago: 320,
    state: "CHANGE_STATE_OPEN",
    ws: "ws/triage/sessions-list",
    revisions: [
      { ago: 320, base: 1, files: { "components/sessions.tsx": C.WEB_SESSIONS_R1 }, contentChanged: true, checks: PASS, session: "ses_01JAEtriage7a1", costCents: 41 },
      { ago: 130, base: 1, files: { "components/sessions.tsx": C.WEB_SESSIONS_R2 }, contentChanged: true, checks: PASS, session: "ses_01JAEtriage7a1", costCents: 36 },
    ],
    reviews: [],
    threads: [],
  },
  {
    key: "infra-us1",
    repo: "infra",
    author: "dev",
    title: "Add the us-east-2 cell",
    body: "Second cell for US customers.",
    ago: 800,
    state: "CHANGE_STATE_OPEN",
    ws: "ws/dev/cell-us1",
    revisions: [{ ago: 800, base: 0, files: { "main.tf": C.INFRA_MAIN_V2 }, contentChanged: true, checks: { test: "CHECK_STATUS_PASSED", lint: "CHECK_STATUS_PASSED", e2e: "CHECK_STATUS_PASSED" } }],
    reviews: [{ by: "mara", rev: 1, verdict: "VERDICT_APPROVE", body: "Plan output looks right.", ago: 700 }],
    threads: [],
  },
];

export const CRS: CrSeed[] = [...ACME_CRS, ...COMMUNITY_CRS, ...POST_CRS];

export interface LeaseSeed {
  key: string;
  repo: string;
  holder: string;
  paths: string[];
  mode: "LEASE_MODE_ADVISORY" | "LEASE_MODE_EXCLUSIVE";
  expiresIn: number; // minutes after the anchor
  note: string;
  cr?: string;
}

export const LEASES: LeaseSeed[] = [
  { key: "l1", repo: "api", holder: "triage", paths: ["src/auth"], mode: "LEASE_MODE_EXCLUSIVE", expiresIn: 85, note: "Rotating session ids (#13)", cr: "rotate" },
  { key: "l2", repo: "api", holder: "billing", paths: ["src/billing/tax.ts", "tests/billing.test.ts"], mode: "LEASE_MODE_ADVISORY", expiresIn: 40, note: "Tax rounding and PDF breakdown", cr: "tax" },
  { key: "l3", repo: "api", holder: "watcher", paths: ["src/routes/health.ts"], mode: "LEASE_MODE_ADVISORY", expiresIn: 110, note: "Watching p99 after the histogram merged" },
  { key: "l4", repo: "api", holder: "lin", paths: ["src/billing/invoice.ts"], mode: "LEASE_MODE_ADVISORY", expiresIn: 25, note: "Invoice totals cache", cr: "cache" },
  { key: "l5", repo: "web", holder: "lin", paths: ["lib/robots.ts"], mode: "LEASE_MODE_ADVISORY", expiresIn: 60, note: "Polling robots" },
];

export const DOC_TOPICS = [
  { file: "docs/errors.md", title: "error reasons", body: "Every error carries a stable `reason` such as `INVALID_INVOICE` or `SESSION_EXPIRED`." },
  { file: "docs/pagination.md", title: "pagination", body: "List endpoints return `next_page_token`; pass it back as `page_token`." },
  { file: "docs/idempotency.md", title: "idempotency keys", body: "Send `Idempotency-Key` on POST; retries with the same key return the first result." },
  { file: "docs/regions.md", title: "regions", body: "Invoices are stored in the customer's region: `eu1` or `us1`." },
];
