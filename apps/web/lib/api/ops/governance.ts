/** Releases, security alerts, policy and the audit log. */
import { AuditService, type AuditEntryJson } from "../../gen/bee/v1/audit_pb";
import type { SubjectRefJson } from "../../gen/bee/v1/common_pb";
import { RepoService } from "../../gen/bee/v1/forge_pb";
import { PolicyService } from "../../gen/bee/v1/policy_pb";
import { ReleaseService, type ReleaseJson } from "../../gen/bee/v1/releases_pb";
import {
  SecurityService,
  type CodeScanningAlertJson,
  type DependencyAlertJson,
  type SecretScanningAlertJson,
  type SecretScanningLocationJson,
  type SecuritySeverityJson,
} from "../../gen/bee/v1/security_pb";
import { QueryService } from "../../gen/bee/v1/vcs_pb";
import { base64ToBytes, hex, objectIdText, pathBytes, pathText } from "../ids";
import type { Rpc } from "../rpc";
import { BeeError, type CallOptions } from "../transport";
import type * as T from "../types";
import { namer, principals } from "./agents";
import { list, mapLimit, num, pageOf, pass, str, wire, type PartialOps } from "./shared";

// ---------------------------------------------------------------- releases

export function releaseView(r: ReleaseJson): T.Release {
  return {
    id: str(r.id),
    repoId: str(r.repo?.id),
    tag: str(r.tagName),
    name: str(r.name),
    notes: str(r.body),
    target: r.target as T.ObjectId | undefined,
    author: r.author as T.Principal | undefined,
    prerelease: !!r.prerelease,
    immutable: !!r.immutable,
    assets: list(r.assets).map((a) => ({ name: str(a.name), size: str(a.size) || "0", downloads: num(a.downloadCount) })),
    createdAt: r.createdAt,
  };
}

// ---------------------------------------------------------------- security

const SEVERITY: Record<string, T.SecurityAlert["severity"]> = {
  SECURITY_SEVERITY_CRITICAL: "critical",
  SECURITY_SEVERITY_HIGH: "high",
  SECURITY_SEVERITY_MEDIUM: "medium",
};
const sev = (s: SecuritySeverityJson | undefined): T.SecurityAlert["severity"] => SEVERITY[str(s)] ?? "low";
const ORDER = { critical: 0, high: 1, medium: 2, low: 3 };

export function dependencyAlertView(a: DependencyAlertJson): T.SecurityAlert {
  const d = a.dependency ?? {};
  return {
    id: "dependency:" + str(a.number),
    repoId: str(a.repo?.id),
    kind: "dependency",
    severity: sev(a.severity ?? a.advisory?.severity),
    state: a.state === "DEPENDENCY_ALERT_STATE_FIXED" ? "fixed" : a.state === "DEPENDENCY_ALERT_STATE_OPEN" ? "open" : "dismissed",
    title: str(a.advisory?.summary),
    detail: str(a.advisory?.description),
    location: [str(d.manifestPath), [str(d.name), str(d.version)].filter(Boolean).join(" ")].filter(Boolean).join(" · "),
    advisory: str(a.advisory?.ghsaId) || str(a.advisory?.cveId),
    fixChangeRequestId: str(a.updateChangeRequest),
    dismissedReason: str(a.dismissedComment),
    createdAt: a.createdAt,
  };
}

export function secretAlertView(a: SecretScanningAlertJson, locations: SecretScanningLocationJson[]): T.SecurityAlert {
  const blob = locations.find((l) => l.blob)?.blob;
  const where = blob ? pathText(str(blob.path)) : "";
  return {
    id: "secret:" + str(a.number),
    repoId: str(a.repo?.id),
    kind: "secret",
    severity: sev(a.severity),
    state: a.state === "SECRET_SCANNING_ALERT_STATE_OPEN" ? "open" : a.resolution === "SECRET_SCANNING_RESOLUTION_REVOKED" ? "fixed" : "dismissed",
    title: str(a.secretTypeDisplayName) + (where ? " in " + where : ""),
    detail: [
      a.secretPreview ? "Matched " + a.secretPreview + "." : "",
      a.validity === "SECRET_SCANNING_VALIDITY_ACTIVE" ? "The provider reports the secret as active." : a.validity === "SECRET_SCANNING_VALIDITY_INACTIVE" ? "The provider reports the secret as inactive." : "",
      a.pushProtectionBypassed ? "Push protection was bypassed." : "",
    ]
      .filter(Boolean)
      .join(" "),
    location: where ? where + (blob?.line ? ":" + blob.line : "") : "",
    advisory: "secret:" + str(a.secretType),
    fixChangeRequestId: "",
    dismissedReason: a.resolution === "SECRET_SCANNING_RESOLUTION_REVOKED" ? "" : str(a.resolutionComment),
    createdAt: a.createdAt,
  };
}

export function codeAlertView(a: CodeScanningAlertJson): T.SecurityAlert {
  const loc = a.mostRecentInstance?.location;
  const path = loc ? pathText(str(loc.path)) : "";
  return {
    id: "code_scanning:" + str(a.number),
    repoId: str(a.repo?.id),
    kind: "code_scanning",
    severity: sev(a.rule?.securitySeverity),
    state: a.state === "CODE_SCANNING_ALERT_STATE_FIXED" ? "fixed" : a.state === "CODE_SCANNING_ALERT_STATE_DISMISSED" ? "dismissed" : "open",
    title: str(a.rule?.name),
    detail: str(a.mostRecentInstance?.message) || str(a.rule?.description),
    location: path ? path + (loc?.startLine ? ":" + loc.startLine : "") : "",
    advisory: list(a.rule?.tags).find((t) => /^CWE-\d+$/.test(t)) ?? str(a.rule?.id),
    fixChangeRequestId: str(a.fixChangeRequest),
    dismissedReason: str(a.dismissedComment),
    createdAt: a.createdAt,
  };
}

async function secretView(rpc: Rpc, repo: T.RepoRef, a: SecretScanningAlertJson, opts?: CallOptions): Promise<T.SecurityAlert> {
  const locs = await rpc.call(SecurityService.method.listSecretScanningLocations, { repo, number: str(a.number), page: { pageSize: 10 } }, opts);
  return secretAlertView(a, list(locs.locations));
}

async function listAlerts(rpc: Rpc, r: T.ListAlertsRequest, opts?: CallOptions): Promise<T.SecurityAlert[]> {
  const s = r.state || "";
  const dep = s === "open" ? ["DEPENDENCY_ALERT_STATE_OPEN" as const] : s === "fixed" ? ["DEPENDENCY_ALERT_STATE_FIXED" as const] : s === "dismissed" ? ["DEPENDENCY_ALERT_STATE_DISMISSED" as const, "DEPENDENCY_ALERT_STATE_AUTO_DISMISSED" as const] : [];
  const code = s === "open" ? ["CODE_SCANNING_ALERT_STATE_OPEN" as const] : s === "fixed" ? ["CODE_SCANNING_ALERT_STATE_FIXED" as const] : s === "dismissed" ? ["CODE_SCANNING_ALERT_STATE_DISMISSED" as const] : [];
  const secret = s === "open" ? "SECRET_SCANNING_ALERT_STATE_OPEN" : s ? "SECRET_SCANNING_ALERT_STATE_RESOLVED" : undefined;
  const page = { pageSize: 200 };
  const [d, sc, c] = await Promise.all([
    rpc.call(SecurityService.method.listDependencyAlerts, { repo: r.repo, states: dep, page }, opts),
    rpc.call(SecurityService.method.listSecretScanningAlerts, { repo: r.repo, state: secret, page }, opts),
    rpc.call(SecurityService.method.listCodeScanningAlerts, { repo: r.repo, states: code, page }, opts),
  ]);
  const secrets = await mapLimit(list(sc.alerts), 8, (a) => secretView(rpc, r.repo, a, opts));
  const all = [...list(d.alerts).map(dependencyAlertView), ...secrets, ...list(c.alerts).map(codeAlertView)];
  return all.filter((a) => !s || a.state === s).sort((a, b) => ORDER[a.severity] - ORDER[b.severity]);
}

function parseAlertId(id: string): { kind: T.SecurityAlert["kind"]; number: string } {
  const m = /^(dependency|secret|code_scanning):(\d+)$/.exec(id);
  if (!m) throw new BeeError("invalid_argument", { reason: "INVALID_ARGUMENT", message: "alert ids look like dependency:3, secret:1 or code_scanning:7" });
  return { kind: m[1] as T.SecurityAlert["kind"], number: m[2] as string };
}

function dismissCall(r: T.DismissAlertRequest) {
  const { kind, number } = parseAlertId(r.id);
  const base = { repo: r.repo, number };
  // The form takes free text; it becomes the comment. The enum reason is the closest generic one.
  if (kind === "dependency")
    return { kind, method: SecurityService.method.updateDependencyAlert, req: { ...base, state: "DEPENDENCY_ALERT_STATE_DISMISSED" as const, dismissedReason: "DEPENDENCY_ALERT_DISMISS_REASON_TOLERABLE_RISK" as const, dismissedComment: r.reason } };
  if (kind === "secret")
    return { kind, method: SecurityService.method.updateSecretScanningAlert, req: { ...base, state: "SECRET_SCANNING_ALERT_STATE_RESOLVED" as const, resolution: "SECRET_SCANNING_RESOLUTION_WONT_FIX" as const, resolutionComment: r.reason } };
  return { kind, method: SecurityService.method.updateCodeScanningAlert, req: { ...base, state: "CODE_SCANNING_ALERT_STATE_DISMISSED" as const, dismissedReason: "CODE_SCANNING_DISMISS_REASON_WONT_FIX" as const, dismissedComment: r.reason } };
}

async function dismiss(rpc: Rpc, r: T.DismissAlertRequest, opts?: CallOptions): Promise<T.SecurityAlert> {
  const { kind, number } = parseAlertId(r.id);
  const base = { repo: r.repo, number };
  if (kind === "dependency")
    return dependencyAlertView(
      await rpc.call(SecurityService.method.updateDependencyAlert, { ...base, state: "DEPENDENCY_ALERT_STATE_DISMISSED", dismissedReason: "DEPENDENCY_ALERT_DISMISS_REASON_TOLERABLE_RISK", dismissedComment: r.reason }, opts),
    );
  if (kind === "secret")
    return secretView(
      rpc,
      r.repo,
      await rpc.call(SecurityService.method.updateSecretScanningAlert, { ...base, state: "SECRET_SCANNING_ALERT_STATE_RESOLVED", resolution: "SECRET_SCANNING_RESOLUTION_WONT_FIX", resolutionComment: r.reason }, opts),
      opts,
    );
  return codeAlertView(
    await rpc.call(SecurityService.method.updateCodeScanningAlert, { ...base, state: "CODE_SCANNING_ALERT_STATE_DISMISSED", dismissedReason: "CODE_SCANNING_DISMISS_REASON_WONT_FIX", dismissedComment: r.reason }, opts),
  );
}

// ---------------------------------------------------------------- policy

async function readText(rpc: Rpc, repo: T.RepoRef, at: string, path: string, opts?: CallOptions): Promise<string> {
  const parts: Uint8Array[] = [];
  for await (const c of rpc.stream(QueryService.method.readPath, { repo, at: { expr: at }, path: pathBytes(path) }, opts)) parts.push(base64ToBytes(str(c.data)));
  const all = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
  let o = 0;
  for (const p of parts) {
    all.set(p, o);
    o += p.length;
  }
  return new TextDecoder().decode(all);
}

async function getPolicy(rpc: Rpc, r: T.GetPolicyRequest, opts?: CallOptions): Promise<T.Policy> {
  const doc = await rpc.call(PolicyService.method.getEffectivePolicy, { repo: r.repo }, opts);
  const src = list(doc.sources).find((s) => s.kind === "POLICY_SOURCE_KIND_REPO_FILE");
  const path = str(src?.path) || ".beehive/policy.toml";
  const at = src?.changeset as T.ObjectId | undefined;
  const source = at ? await readText(rpc, r.repo, objectIdText(at), path, opts) : "";
  return { path, source, normalizedJson: str(doc.normalizedJson), epoch: str(doc.epoch), atChangeset: at };
}

async function evaluatePolicy(rpc: Rpc, r: T.EvaluatePolicyRequest, opts?: CallOptions): Promise<T.PolicyEvaluation> {
  const v = await rpc.call(PolicyService.method.validatePolicy, { repo: r.repo, toml: r.source, replaySample: r.sample }, opts);
  const errors = list(v.issues)
    .filter((i) => i.severity !== "POLICY_ISSUE_SEVERITY_WARNING")
    .map((i) => ({ line: num(i.line), message: str(i.message) }));
  if (!v.valid) return { valid: false, errors, rules: [], decisions: [] };
  return {
    valid: true,
    errors,
    normalizedJson: str(v.normalizedJson),
    rules: list(v.ruleSummaries),
    decisions: list(v.replay).map((d) => ({
      changeRequestId: str(d.changeRequestId),
      title: str(d.title),
      diffClass: str(d.diffClass),
      current: !!d.currentAllowed,
      proposed: !!d.proposedAllowed,
      missing: list(d.missing),
    })),
  };
}

// ---------------------------------------------------------------- audit

function subjectText(s: SubjectRefJson | undefined): string {
  if (!s?.id) return "";
  const kind = str(s.kind).replace("SUBJECT_KIND_", "");
  const label = kind === "CHANGE_REQUEST" ? "pull request" : kind === "CHANGESET" ? "commit" : kind.toLowerCase().replaceAll("_", " ");
  return [label, s.id].filter(Boolean).join(" ");
}

function delegationOf(e: AuditEntryJson): string[] {
  const actor = e.actor?.id;
  const chain = list(e.delegationChain).filter((id) => id !== actor);
  const root = e.onBehalfOf;
  return root && root !== actor && !chain.includes(root) ? [...chain, root] : chain;
}

async function listAudit(rpc: Rpc, r: T.ListAuditRequest, opts?: CallOptions): Promise<T.ListAuditResponse> {
  const [q, verify] = await Promise.all([
    rpc.call(AuditService.method.queryAuditLog, { org: r.org, filter: auditFilter(r), page: r.page }, opts),
    rpc.call(AuditService.method.verifyAuditChain, { org: r.org }, opts),
  ]);
  // Record's result filter works on older servers and on ones that only support denied_only.
  const entries = list(q.entries).filter((e) => (!r.deniedOnly || !allowed(e)) && (!r.allowedOnly || allowed(e)));
  const ids = (e: AuditEntryJson) => [...delegationOf(e), str(e.actor?.id)].filter((id) => /^[0-9A-HJKMNP-TV-Z]{26}$/.test(id));
  const repoIds = [...new Set(entries.flatMap((e) => (e.repo?.id && !e.repo.fullName ? [e.repo.id] : [])))];
  const [names, repoNames] = await Promise.all([principals(rpc, entries.flatMap(ids), opts), reposById(rpc, repoIds, opts)]);
  return {
    entries: entries.map((e): T.AuditEntry => {
      const delegation = delegationOf(e);
      const root = names.get(e.onBehalfOf || delegation.at(-1) || "");
      return {
        seq: str(e.seq),
        time: e.time,
        actor: e.actor?.id && !e.actor.display ? (names.get(e.actor.id) ?? (e.actor as T.Principal)) : (e.actor as T.Principal | undefined),
        onBehalfOf: delegation.map((id) => names.get(id)?.display || id),
        answerable: root?.kind === "PRINCIPAL_KIND_HUMAN" && root.id !== e.actor?.id ? root : undefined,
        action: str(e.action),
        target: str(e.targetDisplay) || subjectText(e.target),
        repo: str(e.repo?.fullName) || repoNames.get(str(e.repo?.id)) || str(e.repo?.id),
        allowed: allowed(e),
        rule: str(e.policyRule),
        source: [str(e.userAgent), str(e.ip)].filter(Boolean).join(" · "),
        hash: hex(base64ToBytes(str(e.hash))),
        prevHash: hex(base64ToBytes(str(e.prevHash))),
      };
    }),
    chainVerified: !!verify.ok,
    verifiedToSeq: str(verify.verifiedToSeq) || "0",
    firstBrokenSeq: str(verify.firstBrokenSeq) || "0",
    verificationFailure: str(verify.failure),
    page: pageOf(q.page),
  };
}

/** Repo ids to full names (audit entries name repos by id). Unknown ids are left out. */
async function reposById(rpc: Rpc, ids: string[], opts?: CallOptions): Promise<Map<string, string>> {
  const names = await mapLimit(ids, 8, async (id) => {
    try {
      return str((await rpc.call(RepoService.method.getRepo, { repo: { id } }, opts)).fullName);
    } catch {
      return "";
    }
  });
  return new Map(ids.flatMap((id, i) => (names[i] ? [[id, names[i] as string]] : [])));
}

const allowed = (e: AuditEntryJson) => !e.result || e.result === "ok";

function auditFilter(r: T.ListAuditRequest) {
  return { actorId: r.actorId || undefined, actionPrefixes: r.action ? [r.action] : [], repo: r.repo ? { fullName: r.repo } : undefined, deniedOnly: !!r.deniedOnly };
}

// ---------------------------------------------------------------- ops

export const governanceOps = {
  ReleaseService: {
    ListReleases: {
      wire: (r) => wire(ReleaseService.method.listReleases, { repo: r.repo, includePrereleases: true }),
      run: async (rpc, r, opts) => {
        const releases = list((await rpc.call(ReleaseService.method.listReleases, { repo: r.repo, includePrereleases: true, page: { pageSize: 100 } }, opts)).releases).map(releaseView);
        const name = await namer(rpc, releases.map((x) => x.author), opts);
        return { releases: releases.map((x) => ({ ...x, author: name(x.author) })) };
      },
    },
    GenerateNotes: {
      wire: (r) => wire(ReleaseService.method.generateReleaseNotes, { repo: r.repo, tagName: r.tag, previousTagName: r.previousTag }),
      run: async (rpc, r, opts) => ({ notes: str((await rpc.call(ReleaseService.method.generateReleaseNotes, { repo: r.repo, tagName: r.tag, previousTagName: r.previousTag }, opts)).body) }),
    },
    CreateRelease: {
      wire: (r) => wire(ReleaseService.method.createRelease, createReleaseReq(r)),
      run: async (rpc, r, opts) => {
        const created = await rpc.call(ReleaseService.method.createRelease, createReleaseReq(r), opts);
        // An immutable release is created as a draft, then published (PRODUCT §4.16).
        if (!created.draft) return releaseView(created);
        return releaseView(await rpc.call(ReleaseService.method.publishRelease, { repo: r.repo, releaseId: str(created.id) }, opts));
      },
    },
    GetSourceArchiveUrl: pass(ReleaseService.method.getSourceArchiveUrl),
  },
  SecurityService: {
    ListAlerts: {
      wire: (r) => wire(SecurityService.method.listDependencyAlerts, { repo: r.repo }),
      run: async (rpc, r, opts) => ({ alerts: await listAlerts(rpc, r, opts) }),
    },
    DismissAlert: {
      wire: (r) => {
        const c = dismissCall(r);
        return wire(c.method, c.req);
      },
      run: (rpc, r, opts) => dismiss(rpc, r, opts),
    },
  },
  PolicyService: {
    GetPolicy: {
      wire: (r) => wire(PolicyService.method.getEffectivePolicy, { repo: r.repo }),
      run: (rpc, r, opts) => getPolicy(rpc, r, opts),
    },
    EvaluatePolicy: {
      wire: (r) => wire(PolicyService.method.validatePolicy, { repo: r.repo, toml: r.source, replaySample: r.sample }),
      run: (rpc, r, opts) => evaluatePolicy(rpc, r, opts),
    },
  },
  AuditService: {
    ListAudit: {
      wire: (r) => wire(AuditService.method.queryAuditLog, { org: r.org, filter: auditFilter(r), page: r.page }),
      run: (rpc, r, opts) => listAudit(rpc, r, opts),
    },
    VerifyAuditChain: pass(AuditService.method.verifyAuditChain),
  },
} satisfies PartialOps<"ReleaseService" | "SecurityService" | "PolicyService" | "AuditService">;

function createReleaseReq(r: T.CreateReleaseRequest) {
  // Published stable releases are immutable (PRODUCT §4.16); pre-releases stay editable.
  return { repo: r.repo, tagName: r.tag, name: r.name, body: r.notes, prerelease: r.prerelease, immutable: !r.prerelease, draft: !r.prerelease };
}
