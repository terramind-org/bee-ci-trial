/** OpsService for the fixture: health, cluster status and a small config validator. */
import { OpsService, type HealthComponentJson, type OpsCellJson, type OpsConfigErrorJson } from "../../gen/bee/v1/ops_pb";
import { BeeError } from "../transport";
import { authorizeOperator } from "./admin-state";
import { defineService } from "./router";
import { digestHex } from "./seed";
import type { FixtureStore } from "./store";

const VERSION = "0.9.0-dev";

interface CellDef {
  name: string;
  region: string;
  node: string;
  degraded: boolean;
}

const CELLS: CellDef[] = [
  { name: "eu-1", region: "eu-west", node: "node-a1", degraded: false },
  { name: "us-1", region: "us-east", node: "node-b1", degraded: true },
];

function components(s: FixtureStore, cell: CellDef): HealthComponentJson[] {
  const at = (secondsAgo: number) => s.iso(s.now() - secondsAgo * 1000);
  const ok = (name: string, detail: string, critical: boolean, secondsAgo: number): HealthComponentJson => ({
    name,
    liveness: "HEALTH_STATUS_OK",
    readiness: "HEALTH_STATUS_OK",
    reason: "",
    detail,
    criticalPath: critical,
    checkedAt: at(secondsAgo),
  });
  if (cell.degraded)
    return [
      {
        name: "metadata_store",
        liveness: "HEALTH_STATUS_OK",
        readiness: "HEALTH_STATUS_DEGRADED",
        reason: "primary_unreachable",
        detail: "serving refs from the replica, 1.8s behind the primary",
        criticalPath: true,
        checkedAt: at(3),
      },
      ok("blob_store", "object store responding in 11 ms", true, 3),
      ok("event_log", "tailing from the replica", false, 3),
      ok("search_index", "caught up", false, 6),
      ok("workflow_runners", "2 of 2 runners registered", false, 5),
      ok("git_gateway", "accepting pushes and fetches", true, 3),
      ok("forge", "serving", false, 3),
    ];
  return [
    ok("metadata_store", "primary responding in 3 ms", true, 2),
    ok("blob_store", "object store responding in 9 ms", true, 2),
    ok("event_log", "head at the latest segment", false, 2),
    {
      name: "search_index",
      liveness: "HEALTH_STATUS_OK",
      readiness: "HEALTH_STATUS_DEGRADED",
      reason: "index_lagging",
      detail: "indexing lag 38s behind the event log",
      criticalPath: false,
      checkedAt: at(4),
    },
    ok("workflow_runners", "6 of 6 runners registered", false, 5),
    ok("git_gateway", "accepting pushes and fetches", true, 2),
    ok("forge", "serving", false, 2),
  ];
}

function cellJson(s: FixtureStore, c: CellDef): OpsCellJson {
  const eu = c.name === "eu-1";
  return {
    name: c.name,
    region: c.region,
    version: VERSION,
    configVersion: "cfg-" + digestHex("config:" + c.name, 4),
    schemaVersion: "42",
    status: eu ? "HEALTH_STATUS_OK" : "HEALTH_STATUS_DEGRADED",
    metadataPrimaryUp: eu,
    servingStaleReads: !eu,
    staleLagMs: eu ? "0" : "1800",
    nodes: eu ? 3 : 2,
    orgs: 1,
    redactionEpoch: eu ? "17" : "16",
    policyEpoch: "29",
  };
}

// ---------------------------------------------------------------- config validation

const KNOWN_TABLES = new Set(["server", "limits", "storage", "daemon", "git_gateway", "auth", "logging", "metrics", "cell", "cluster", "search", "workflows", "billing", "email"]);
const COMPONENTS = new Set(["server", "daemon", "git_gateway", "production"]);

/** Removes a trailing comment, leaving `#` inside a quoted string alone. */
function stripComment(line: string): string {
  let quote = "";
  for (let i = 0; i < line.length; i++) {
    const c = line.charAt(i);
    if (quote) {
      if (c === "\\" && quote === '"') i++;
      else if (c === quote) quote = "";
    } else if (c === '"' || c === "'") quote = c;
    else if (c === "#") return line.slice(0, i);
  }
  return line;
}

function bracketsBalanced(v: string): boolean {
  let depth = 0;
  let quote = "";
  for (let i = 0; i < v.length; i++) {
    const c = v.charAt(i);
    if (quote) {
      if (c === "\\" && quote === '"') i++;
      else if (c === quote) quote = "";
    } else if (c === '"' || c === "'") quote = c;
    else if (c === "[") depth++;
    else if (c === "]") depth--;
  }
  return depth <= 0;
}

/** A quoted string value ends with its opening quote that is not escaped. */
function stringTerminated(v: string): boolean {
  const q = v.charAt(0);
  if (v.length < 2) return false;
  let i = 1;
  while (i < v.length) {
    const c = v.charAt(i);
    if (c === "\\" && q === '"') i += 2;
    else if (c === q) return v.slice(i + 1).trim() === "";
    else i++;
  }
  return false;
}

interface Validation {
  errors: OpsConfigErrorJson[];
  warnings: OpsConfigErrorJson[];
}

export function validateToml(toml: string): Validation {
  const errors: OpsConfigErrorJson[] = [];
  const warnings: OpsConfigErrorJson[] = [];
  const lines = toml.replace(/\r\n/g, "\n").split("\n");
  const seen = new Set<string>();
  let table = "";
  for (let i = 0; i < lines.length; i++) {
    const lineNo = i + 1;
    const raw = lines[i] ?? "";
    const text = stripComment(raw).trim();
    if (!text) continue;
    const col = raw.indexOf(text.charAt(0)) + 1;
    const syntax = (message: string, column = col, keyPath = table) => errors.push({ keyPath, line: lineNo, column, message, code: "toml_syntax" });

    if (text.startsWith("[")) {
      const array = text.startsWith("[[");
      const close = array ? "]]" : "]";
      if (!text.endsWith(close)) {
        syntax("unterminated table header: expected " + close, raw.trimEnd().length + 1, "");
        continue;
      }
      const name = text.slice(array ? 2 : 1, text.length - close.length).trim();
      if (!/^[A-Za-z0-9_-]+(\.[A-Za-z0-9_-]+)*$/.test(name)) {
        syntax("invalid table name: " + (name || "(empty)"), col, "");
        continue;
      }
      table = name;
      const top = name.split(".")[0] ?? name;
      if (!KNOWN_TABLES.has(top)) warnings.push({ keyPath: name, line: lineNo, column: col, message: `unknown table [${top}]: it is ignored`, code: "unknown_table" });
      continue;
    }

    const eq = text.indexOf("=");
    if (eq < 0) {
      syntax("expected `key = value`");
      continue;
    }
    const key = text.slice(0, eq).trim();
    let value = text.slice(eq + 1).trim();
    if (!/^[A-Za-z0-9_.-]+$/.test(key) && !/^"[^"]+"$/.test(key)) {
      syntax("invalid key: " + (key || "(empty)"));
      continue;
    }
    if (!value) {
      syntax("missing value for " + key, raw.trimEnd().length + 1, table ? table + "." + key : key);
      continue;
    }
    // A multi-line array continues until its brackets balance.
    const startLine = lineNo;
    if (value.startsWith("[") && !bracketsBalanced(value)) {
      while (i + 1 < lines.length && !bracketsBalanced(value)) {
        i++;
        value += " " + stripComment(lines[i] ?? "").trim();
      }
      if (!bracketsBalanced(value)) {
        errors.push({ keyPath: table ? table + "." + key : key, line: startLine, column: col, message: "unterminated array", code: "toml_syntax" });
        continue;
      }
    }
    const path = table ? table + "." + key : key;
    if ((value.startsWith('"') || value.startsWith("'")) && !stringTerminated(value)) {
      errors.push({ keyPath: path, line: startLine, column: raw.indexOf(value.charAt(0)) + 1, message: "unterminated string", code: "toml_syntax" });
      continue;
    }
    if (seen.has(path)) {
      errors.push({ keyPath: path, line: startLine, column: col, message: "duplicate key " + path, code: "toml_syntax" });
      continue;
    }
    seen.add(path);
    if (path === "server.edition" && !/^"(cloud|enterprise)"$/.test(value))
      errors.push({ keyPath: path, line: startLine, column: raw.indexOf(value.charAt(0)) + 1, message: 'server.edition must be "cloud" or "enterprise"', code: "invalid_config" });
  }
  return { errors, warnings };
}


// ---------------------------------------------------------------- effective config and the production checklist

/** The config the fixture "server" runs with, as GetEffectiveOpsConfig returns it (secrets redacted). */
const EFFECTIVE_CONFIG = `[server]
edition = "enterprise"
public_url = "https://bee.acme.example"
trust_forwarded_for = true
data_dir = "/var/lib/bee"

[metadata]
backend = "postgres"
stale_read_max_ms = 5000

[blobs]
backend = "s3"
url_key_hex = "<redacted>"

[auth]
signing_key_hex = "<redacted>"

[login]
oidc_issuer = "https://id.acme.example"

[ops]
operators = ["mara"]

[admission]
per_principal_rps = 50
per_root_rps = 100

[import]
allowed_roots = ["/srv/import"]

[backup]
target = "dir:/mnt/backups/bee"

[logging]
level = "debug"
`;

/** `table.key` to the raw value, for the handful of keys the checklist reads. */
function keyValues(toml: string): Map<string, string> {
  const out = new Map<string, string>();
  let table = "";
  for (const raw of toml.split(/\r?\n/)) {
    const line = stripComment(raw).trim();
    const header = /^\[([A-Za-z0-9_.-]+)\]$/.exec(line);
    if (header) {
      table = header[1] ?? "";
      continue;
    }
    const eq = line.indexOf("=");
    if (eq > 0) out.set((table ? table + "." : "") + line.slice(0, eq).trim(), line.slice(eq + 1).trim());
  }
  return out;
}

interface Item {
  id: string;
  keyPath: string;
  severity: "error" | "warning";
  ok: boolean;
  message: string;
}

/** The operational checklist (PRODUCT §4.22) for a document; failed items come back as errors or warnings. */
function productionChecklist(toml: string): Item[] {
  const kv = keyValues(toml);
  const str = (k: string) => (kv.get(k) ?? "").replace(/^["']|["']$/g, "");
  const list = (k: string) => (kv.get(k) ?? "[]").replace(/[\[\]\s]/g, "").length > 0;
  const num = (k: string, fallback: number) => (kv.has(k) ? Number(kv.get(k)) : fallback);
  const edition = str("server.edition") || "enterprise";
  const publicUrl = str("server.public_url");
  const dataDir = str("server.data_dir") || ".";
  const backup = str("backup.target");
  const s3 = str("blobs.backend") === "s3";
  const tls = str("tls.cert_file") !== "" || (publicUrl.startsWith("https://") && str("server.trust_forwarded_for") === "true");
  const operators = list("ops.operators") || list("forge.site_admins");
  const item = (id: string, keyPath: string, severity: Item["severity"], ok: boolean, message: string): Item => ({ id, keyPath, severity, ok, message });
  return [
    edition === "cloud"
      ? item("edition_login", "login.github_client_id", "error", str("login.github_client_id") !== "" || str("login.oidc_issuer") !== "", "the cloud edition needs a way for people to sign in: set login.github_client_id or login.oidc_issuer")
      : item("edition_sso", "login.oidc_issuer", "error", str("login.oidc_issuer") !== "" || operators, "the enterprise edition needs single sign-on: set login.oidc_issuer, or name an operator in ops.operators"),
    item("metadata_postgres", "metadata.backend", "error", str("metadata.backend") === "postgres", "metadata.backend must be postgres: SQLite serves one process on one machine"),
    item("metadata_replica", "metadata.postgres_replica_url", "warning", str("metadata.postgres_replica_url") !== "", "set metadata.postgres_replica_url so reads outlive the primary"),
    item("stale_reads", "metadata.stale_read_max_ms", "warning", num("metadata.stale_read_max_ms", 0) > 0, "metadata.stale_read_max_ms is 0: ref reads fail during a primary outage"),
    item("blobs_s3", "blobs.backend", "error", s3, "blobs.backend must be s3: blobs on disk sit on one machine"),
    item("tls", "tls.cert_file", "error", tls, "set tls.cert_file, or an https:// server.public_url with server.trust_forwarded_for"),
    item("signing_key", "auth.signing_key_hex", "error", str("auth.signing_key_hex") !== "", "auth.signing_key_hex is empty: each process would generate its own key"),
    item("url_key", "blobs.url_key_hex", "error", s3 || str("blobs.url_key_hex") !== "", "local signed URLs need a shared blobs.url_key_hex"),
    item("operators", "ops.operators", "error", operators, "name at least one site operator in ops.operators or forge.site_admins"),
    item("rate_limits", "admission.per_principal_rps", "error", num("admission.per_principal_rps", 1) > 0 && num("admission.per_root_rps", 1) > 0, "admission.per_principal_rps and per_root_rps must not be 0"),
    item("import_roots", "import.allowed_roots", "error", !(kv.get("import.allowed_roots") ?? "").includes('"/"'), "import.allowed_roots must not contain /"),
    item("backups", "backup.target", "error", backup === "s3" || (backup.startsWith("dir:") && !backup.slice(4).startsWith(dataDir)), "backup.target must be s3 or a directory outside data_dir"),
    item("logging", "logging.level", "warning", str("logging.level") !== "debug", "logging.level is debug: production should log at info"),
  ];
}

/** The server-wide operator RPCs answer only the instance operator; org owners are not operators. */
const operator = (s: FixtureStore, ctx: Parameters<typeof authorizeOperator>[1], action: string) => authorizeOperator(s, ctx, action, "ops");

export const ops = defineService(OpsService, {
  getOpsHealth: (s, req, ctx) => {
    operator(s, ctx, "ops.health");
    const cell = CELLS.find((c) => c.name === (req.cell || "eu-1"));
    if (!cell) throw new BeeError("not_found", { reason: "CELL_NOT_FOUND", message: "no cell " + req.cell });
    const parts = components(s, cell);
    const worst = (f: (c: HealthComponentJson) => string | undefined) => (parts.some((c) => f(c) === "HEALTH_STATUS_DOWN") ? "HEALTH_STATUS_DOWN" : parts.some((c) => f(c) === "HEALTH_STATUS_DEGRADED") ? "HEALTH_STATUS_DEGRADED" : "HEALTH_STATUS_OK");
    return {
      liveness: worst((c) => c.liveness),
      readiness: worst((c) => c.readiness),
      components: req.includeComponents ? parts : [],
      node: cell.node,
      cell: cell.name,
      version: VERSION,
    };
  },

  getOpsClusterStatus: (s, req, ctx) => {
    operator(s, ctx, "ops.cluster");
    const only = new Set(req.cells ?? []);
    return {
      cells: CELLS.filter((c) => !only.size || only.has(c.name)).map((c) => cellJson(s, c)),
      directoryUp: true,
      directoryVersion: "dir-" + digestHex("directory", 4),
      observedAt: s.iso(s.now()),
    };
  },

  getEffectiveOpsConfig: (s, _req, ctx) => {
    operator(s, ctx, "ops.config.read");
    return { toml: EFFECTIVE_CONFIG, configVersion: "cfg-" + digestHex("config:eu-1", 4), appliedAt: s.iso(s.now() - 26 * 3600_000) };
  },

  validateOpsConfig: (s, req, ctx) => {
    operator(s, ctx, "ops.config.validate");
    if (req.component && !COMPONENTS.has(req.component)) throw new BeeError("invalid_argument", { reason: "UNKNOWN_COMPONENT", message: `component must be one of: ${[...COMPONENTS].join(", ")}` });
    const toml = req.toml ?? "";
    const { errors, warnings } = validateToml(toml);
    if (req.component === "production" && errors.length === 0) {
      const failed = productionChecklist(toml).filter((c) => !c.ok);
      const asError = (c: Item): OpsConfigErrorJson => ({ keyPath: c.keyPath, line: 0, column: 0, message: c.message, code: "production:" + c.id });
      const blocking = failed.filter((c) => c.severity === "error").map(asError);
      return { valid: blocking.length === 0, errors: blocking, warnings: [...warnings, ...failed.filter((c) => c.severity === "warning").map(asError)], normalizedToml: toml.replace(/\r\n/g, "\n").trim() + "\n" };
    }
    const valid = errors.length === 0;
    return { valid, errors, warnings, normalizedToml: valid ? toml.replace(/\r\n/g, "\n").trim() + "\n" : "" };
  },
});
