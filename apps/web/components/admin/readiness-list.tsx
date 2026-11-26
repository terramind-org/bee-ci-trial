import type { OpsConfigError } from "@/lib/api/types";
import { Badge } from "../ui";

export type CheckStatus = "pass" | "warn" | "fail";

export interface ReadinessCheck {
  id: string;
  keyPath: string;
  title: string;
  /** What the check requires (shown when it passes). */
  requires: string;
  /** What the server said when it failed. */
  message: string;
  status: CheckStatus;
  fix: { label: string; href: string };
}

/** The self-hosting guide, linked by section. */
export const SELF_HOSTING_DOC = "docs/self-hosting.md";
const DOCS_BASE = "https://github.com/terramind-org/bee/blob/main/docs/self-hosting.md";
export const docHref = (anchor: string) => DOCS_BASE + "#" + anchor;

interface Item {
  id: string;
  keyPath: string;
  title: string;
  requires: string;
  anchor: string;
  section: string;
  edition?: "cloud" | "enterprise";
}

/** The operational checklist, in the server's order (`docs/self-hosting.md`, "Checking a production configuration"). */
const CATALOG: Item[] = [
  { id: "edition_login", keyPath: "login.github_client_id", title: "Sign-in method", requires: "A cloud instance needs a way for people to sign in (GitHub or OIDC).", anchor: "edition", section: "Edition", edition: "cloud" },
  { id: "edition_sso", keyPath: "login.oidc_issuer", title: "Single sign-on or an operator", requires: "A self-hosted instance needs OIDC single sign-on or at least one operator.", anchor: "first-admin-and-sso", section: "First admin and SSO", edition: "enterprise" },
  { id: "metadata_postgres", keyPath: "metadata.backend", title: "Metadata in Postgres", requires: "metadata.backend is postgres, so more than one process can serve.", anchor: "production-stack-step-by-step", section: "Production stack" },
  { id: "metadata_replica", keyPath: "metadata.postgres_replica_url", title: "Metadata read replica", requires: "A replica serves reads while the primary is down.", anchor: "production-stack-step-by-step", section: "Production stack" },
  { id: "stale_reads", keyPath: "metadata.stale_read_max_ms", title: "Stale reads allowed", requires: "Ref reads continue, up to a bound, during a primary outage.", anchor: "configuration", section: "Configuration" },
  { id: "blobs_s3", keyPath: "blobs.backend", title: "Blobs in S3", requires: "blobs.backend is s3, so blobs are not on one machine.", anchor: "production-stack-step-by-step", section: "Production stack" },
  { id: "tls", keyPath: "tls.cert_file", title: "TLS", requires: "A certificate, or an https:// public URL behind a trusted proxy.", anchor: "tls", section: "TLS" },
  { id: "signing_key", keyPath: "auth.signing_key_hex", title: "Shared signing key", requires: "Every process verifies tokens with the same key.", anchor: "configuration", section: "Configuration" },
  { id: "url_key", keyPath: "blobs.url_key_hex", title: "Shared URL key", requires: "Signed blob URLs verify on every process.", anchor: "configuration", section: "Configuration" },
  { id: "operators", keyPath: "ops.operators", title: "Site operators", requires: "At least one principal runs the server-wide operator RPCs.", anchor: "first-admin-and-sso", section: "First admin and SSO" },
  { id: "rate_limits", keyPath: "admission.per_principal_rps", title: "Rate limits", requires: "Per-principal and per-root rate limits are above zero.", anchor: "configuration", section: "Configuration" },
  { id: "import_roots", keyPath: "import.allowed_roots", title: "Import roots", requires: "Imports cannot read from the filesystem root.", anchor: "configuration", section: "Configuration" },
  { id: "backups", keyPath: "backup.target", title: "Off-machine backups", requires: "Backups go to S3 or a directory outside the data directory.", anchor: "backups", section: "Backups" },
  { id: "logging", keyPath: "logging.level", title: "Log level", requires: "Production does not log at debug.", anchor: "observability", section: "Observability" },
];

const PREFIX = "production:";
const GENERIC = { anchor: "checking-a-production-configuration", section: "Checking a production configuration" };

/** Whether a response error is a checklist item rather than a problem parsing the document. */
export const isChecklistItem = (e: OpsConfigError) => e.code.startsWith(PREFIX);

/**
 * The full checklist for an answer from ValidateOpsConfig (component "production"). The server
 * reports only what failed; every catalog item it did not report passed. A document that does
 * not parse has no checklist at all, so the caller shows its errors instead.
 */
export function checksFrom(res: { errors: OpsConfigError[]; warnings: OpsConfigError[] }, edition: "cloud" | "enterprise"): ReadinessCheck[] {
  const failed = new Map<string, { status: CheckStatus; e: OpsConfigError }>();
  for (const e of res.errors) if (isChecklistItem(e)) failed.set(e.code.slice(PREFIX.length), { status: "fail", e });
  for (const e of res.warnings) if (isChecklistItem(e)) failed.set(e.code.slice(PREFIX.length), { status: "warn", e });
  const known = new Set(CATALOG.map((c) => c.id));
  const out: ReadinessCheck[] = CATALOG.filter((c) => !c.edition || c.edition === edition).map((c) => {
    const hit = failed.get(c.id);
    return { id: c.id, keyPath: c.keyPath, title: c.title, requires: c.requires, message: hit?.e.message ?? "", status: hit?.status ?? "pass", fix: { label: SELF_HOSTING_DOC + " → " + c.section, href: docHref(c.anchor) } };
  });
  // An item this build does not know yet is still shown, with the server's own words.
  for (const [id, hit] of failed) {
    if (known.has(id)) continue;
    out.push({ id, keyPath: hit.e.keyPath, title: id.replaceAll("_", " "), requires: hit.e.message, message: hit.e.message, status: hit.status, fix: { label: SELF_HOSTING_DOC + " → " + GENERIC.section, href: docHref(GENERIC.anchor) } });
  }
  return out;
}

const BADGE: Record<CheckStatus, { tone: "ok" | "warn" | "bad"; text: string }> = { pass: { tone: "ok", text: "Pass" }, warn: { tone: "warn", text: "Warning" }, fail: { tone: "bad", text: "Fail" } };

/** One row per check: pass, warn or fail, the message, and where the fix is described. */
export function ReadinessList({ checks }: { checks: ReadinessCheck[] }) {
  return (
    <ul aria-label="Readiness checks" className="divide-y divide-line">
      {checks.map((c) => (
        <li key={c.id} data-check={c.id} data-status={c.status} className="flex flex-col gap-1 px-3.5 py-3 md:flex-row md:items-start md:gap-4">
          <div className="w-24 shrink-0">
            <Badge tone={BADGE[c.status].tone}>{BADGE[c.status].text}</Badge>
          </div>
          <div className="min-w-0 flex-1">
            <p className="text-sm font-medium">
              {c.title} <span className="mono ml-1 text-xs font-normal text-ink-3">{c.keyPath}</span>
            </p>
            <p className="text-sm text-ink-2 [overflow-wrap:anywhere]">{c.status === "pass" ? c.requires : c.message}</p>
            {c.status !== "pass" ? (
              <p className="mt-1 text-sm">
                <span className="text-ink-3">Fix: </span>
                <a href={c.fix.href} className="link" target="_blank" rel="noreferrer">
                  {c.fix.label}
                </a>
              </p>
            ) : null}
          </div>
        </li>
      ))}
    </ul>
  );
}

/** "2 failed, 1 warning, 10 passed". */
export function summarize(checks: ReadinessCheck[]): string {
  const n = (s: CheckStatus) => checks.filter((c) => c.status === s).length;
  return `${n("fail")} failed, ${n("warn")} ${n("warn") === 1 ? "warning" : "warnings"}, ${n("pass")} passed`;
}
