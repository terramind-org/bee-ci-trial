/**
 * "Copy as": every UI action rendered as the exact equivalent for agents.
 *   CLI   `bee …` (SPEC §12.4, PRODUCT §4.7)
 *   HTTP  `POST /bee.v1.<Service>/<Method>` with the proto3-JSON body (Connect)
 *   MCP   `tools/call` with `<service>_<method>` and the same arguments
 * The request object is the single source: the three forms can never drift apart. HTTP and
 * MCP name the real bee.v1 RPC the mapping layer sends for the UI operation.
 */
import { pathText } from "./api/ids";
import type { ServiceName } from "./api/methods";
import { wireOf } from "./api/ops";

export interface Op {
  service: ServiceName;
  method: string;
  request: unknown;
  /** Short human description, e.g. "Land this change". */
  label?: string;
}

type Req = Record<string, unknown>;

function q(v: unknown): string {
  const s = String(v ?? "");
  return /^[\w@%+=:,./~^-]+$/.test(s) ? s : "'" + s.replace(/'/g, "'\\''") + "'";
}

function repoFlag(r: Req): string {
  const repo = r.repo as { fullName?: string; id?: string } | undefined;
  return repo?.fullName ? " --repo " + q(repo.fullName) : repo?.id ? " --repo " + q(repo.id) : "";
}

function paths(v: unknown): string[] {
  return Array.isArray(v) ? v.map((p) => pathText(String(p))) : [];
}

function expr(v: unknown): string {
  return String((v as { expr?: string } | undefined)?.expr ?? "");
}

function opt(flag: string, v: unknown): string {
  return v === undefined || v === "" || v === null || v === 0 || v === false ? "" : " " + flag + (v === true ? "" : " " + q(v));
}

const VERDICT: Record<string, string> = { VERDICT_APPROVE: "--approve", VERDICT_REQUEST_CHANGES: "--request-changes", VERDICT_COMMENT: "--comment" };

/** CLI forms for the operations the UI performs. Anything else falls back to `bee api`. */
const CLI: Record<string, (r: Req) => string> = {
  "ChangeService/ListChanges": (r) => "bee cr list" + repoFlag(r) + opt("--state", String(r.state ?? "").replace("CHANGE_STATE_", "").toLowerCase()) + " --json",
  "ChangeService/GetChange": (r) => "bee cr show " + q(r.id) + repoFlag(r) + " --json",
  "ChangeService/OpenChange": (r) => "bee cr open" + repoFlag(r) + " --title " + q(r.title) + opt("--body", r.body) + opt("--onto", r.targetRef) + " --json",
  "ChangeService/CloseChange": (r) => "bee cr close " + q(r.id) + repoFlag(r) + opt("--reason", r.reason) + " --json",
  "ChangeService/EvaluateLanding": (r) => "bee cr can-land " + q(r.changeRequestId) + repoFlag(r) + " --json",
  "ChangeService/GetChangeDiff": (r) => "bee cr diff " + q(r.changeRequestId) + repoFlag(r) + opt("--rev", r.revision) + opt("--since", r.sinceRevision) + " --json",
  "ChangeService/SubmitReview": (r) => "bee review " + q(r.changeRequestId) + repoFlag(r) + " --rev " + q(r.revision) + " " + (VERDICT[String(r.verdict)] ?? "--comment") + opt("--body", r.body) + " --json",
  "ChangeService/ListReviews": (r) => "bee review list " + q(r.changeRequestId) + repoFlag(r) + " --json",
  "ChangeService/ListThreads": (r) => "bee review threads " + q(r.changeRequestId) + repoFlag(r) + " --json",
  "ChangeService/CreateThread": (r) =>
    "bee review comment " + q(r.changeRequestId) + repoFlag(r) + " --rev " + q(r.revision) + " --path " + q(pathText(String(r.path ?? ""))) + " --lines " + q(r.startLine + "-" + (r.endLine || r.startLine)) + " --body " + q(r.body) + opt("--suggest", r.suggestion) + " --json",
  "ChangeService/ReplyThread": (r) => "bee review reply " + q(r.threadId) + repoFlag(r) + " --body " + q(r.body) + " --json",
  "ChangeService/ResolveThread": (r) => "bee review " + (r.resolved ? "resolve " : "reopen ") + q(r.threadId) + repoFlag(r) + " --json",
  "ChangeService/ApplySuggestions": (r) => "bee review apply " + q(r.changeRequestId) + repoFlag(r) + " --threads " + q((r.threadIds as string[] | undefined)?.join(",")) + " --json",
  "ChangeService/ListQueue": (r) => "bee queue list" + repoFlag(r) + opt("--ref", r.ref) + " --json",
  "ChangeService/Enqueue": (r) => "bee cr land " + q(r.changeRequestId) + repoFlag(r) + " --queue --json",
  "WriteService/Land": (r) => "bee land " + (r.changeRequestId ? q(r.changeRequestId) : q(r.ref)) + repoFlag(r) + " --json",
  "CheckService/ListChecks": (r) => "bee check list" + repoFlag(r) + " --json",
  "LeaseService/ListLeases": (r) => "bee lease list" + repoFlag(r) + (paths(r.paths).length ? " " + paths(r.paths).map(q).join(" ") : "") + " --json",
  "LeaseService/AcquireLease": (r) =>
    "bee lease acquire " + paths(r.paths).map(q).join(" ") + repoFlag(r) + (r.mode === "LEASE_MODE_EXCLUSIVE" ? " --exclusive" : "") + opt("--ttl", r.ttlSeconds ? r.ttlSeconds + "s" : "") + opt("--note", r.note) + " --json",
  "LeaseService/ReleaseLease": (r) => "bee lease release " + q(r.id) + repoFlag(r) + " --json",
  "EventService/Subscribe": (r) => "bee events" + repoFlag(r) + opt("--from", r.fromSeq) + ((r.kinds as string[] | undefined)?.length ? " --kinds " + q((r.kinds as string[]).join(",")) : "") + " --follow --json",
  "RepoService/ListRepos": (r) => "bee repo list --org " + q(r.org) + " --json",
  "RepoService/GetContext": (r) => "bee context" + repoFlag(r) + " " + paths(r.paths).map(q).join(" ") + " --json",
  "RefService/ListRefs": (r) => "bee ref list" + repoFlag(r) + " --json",
  "QueryService/ListTree": (r) => "bee ls " + q(pathText(String(r.path ?? "")) || ".") + repoFlag(r) + opt("--at", expr(r.at)) + " --json",
  "QueryService/ReadPath": (r) => "bee cat " + q(pathText(String(r.path ?? ""))) + repoFlag(r) + opt("--at", expr(r.at)),
  "QueryService/Log": (r) => "bee log" + (r.path ? " " + q(pathText(String(r.path))) : "") + repoFlag(r) + opt("--from", expr(r.from)) + " --json",
  "QueryService/Blame": (r) => "bee blame " + q(pathText(String(r.path ?? ""))) + repoFlag(r) + opt("--at", expr(r.at)) + " --json",
  "QueryService/Diff": (r) => "bee diff " + q(expr(r.from)) + " " + q(expr(r.to)) + repoFlag(r) + (paths(r.paths).length ? " -- " + paths(r.paths).map(q).join(" ") : "") + " --json",
  "AuthService/WhoAmI": () => "bee whoami --json",
  "AuthService/Login": (r) => (r.githubStart || r.githubDeviceCode ? "bee login --github" : "bee login"),
  "AuthService/MintToken": (r) => {
    const s = (r.scope ?? {}) as Req;
    const budget = (s.budget ?? {}) as Req;
    return (
      "bee token mint" +
      opt("--for", r.principalId) +
      ((s.repos as string[] | undefined)?.length ? " --repos " + q((s.repos as string[]).join(",")) : "") +
      (paths(s.writePaths).length ? " --write " + q(paths(s.writePaths).join(",")) : "") +
      (paths(s.readPaths).length ? " --read " + q(paths(s.readPaths).join(",")) : "") +
      ((s.refs as string[] | undefined)?.length ? " --refs " + q((s.refs as string[]).join(",")) : "") +
      ((s.actions as string[] | undefined)?.length ? " --actions " + q((s.actions as string[]).join(",")) : "") +
      opt("--max-changesets", Number(budget.maxChangesets) || "") +
      opt("--max-landed", Number(budget.maxLanded) || "") +
      opt("--expires", s.expiresAt) +
      opt("--session", s.session) +
      " --json"
    );
  },
  "AuthService/Revoke": (r) => "bee token revoke " + q(r.tokenId) + " --json",
  "AgentService/ListAgents": (r) => "bee agent list --org " + q(r.org) + " --json",
  "AgentService/GetAgent": (r) => "bee agent show " + q(r.principalId) + " --json",
  "AgentService/ListTokens": (r) => "bee token list --for " + q(r.principalId) + " --json",
  "AgentService/ListProvenance": (r) => "bee agent provenance " + q(r.principalId) + " --json",
  "WorkflowService/ListRuns": (r) => "bee run list" + repoFlag(r) + opt("--status", r.status) + " --json",
  "WorkflowService/GetRun": (r) => "bee run show " + q(r.id) + repoFlag(r) + " --json",
  "WorkflowService/Rerun": (r) => "bee run rerun " + q(r.id) + repoFlag(r) + (r.failedOnly ? " --failed" : "") + " --json",
  "WorkflowService/StreamLogs": (r) => "bee run logs " + q(r.runId) + repoFlag(r) + " --job " + q(r.jobId) + " --follow",
  "IssueService/ListIssues": (r) => "bee issue list" + repoFlag(r) + opt("--state", r.state) + opt("--label", r.label) + " --json",
  "IssueService/GetIssue": (r) => "bee issue show " + q(r.number) + repoFlag(r) + " --json",
  "IssueService/AssignIssue": (r) => "bee issue assign " + q(r.number) + " " + ((r.assigneeIds as string[] | undefined) ?? []).map(q).join(" ") + repoFlag(r) + " --json",
  "IssueService/SetIssueState": (r) => "bee issue " + (r.state === "closed" ? "close " : "reopen ") + q(r.number) + repoFlag(r) + " --json",
  "IssueService/CommentIssue": (r) => "bee issue comment " + q(r.number) + repoFlag(r) + " --body " + q(r.body) + " --json",
  "ProjectService/GetProject": (r) => "bee project show" + repoFlag(r) + " --json",
  "ProjectService/MoveItem": (r) => "bee project move " + q(r.issueNumber) + " --to " + q(r.toColumnId) + repoFlag(r) + " --json",
  "ReleaseService/ListReleases": (r) => "bee release list" + repoFlag(r) + " --json",
  "ReleaseService/GenerateNotes": (r) => "bee release notes " + q(r.tag) + repoFlag(r) + opt("--since", r.previousTag),
  "ReleaseService/CreateRelease": (r) => "bee release create " + q(r.tag) + repoFlag(r) + opt("--name", r.name) + (r.prerelease ? " --prerelease" : "") + " --notes-file notes.md --json",
  "SecurityService/ListAlerts": (r) => "bee security alerts" + repoFlag(r) + opt("--state", r.state) + " --json",
  "SecurityService/DismissAlert": (r) => "bee security dismiss " + q(r.id) + repoFlag(r) + " --reason " + q(r.reason) + " --json",
  "PolicyService/GetPolicy": (r) => "bee policy show" + repoFlag(r) + " --json",
  "PolicyService/EvaluatePolicy": (r) => "bee policy evaluate .bee/policy.toml" + repoFlag(r) + opt("--sample", r.sample) + " --json",
  "AuditService/ListAudit": (r) => "bee audit --org " + q(r.org) + opt("--actor", r.actorId) + opt("--action", r.action) + opt("--repo", r.repo) + (r.deniedOnly ? " --denied" : "") + " --json",
  "ImportService/StartImport": (r) => "bee import github " + q(r.source) + (r.org ? " --org" : "") + opt("--into", r.targetOrg) + (r.phase === "IMPORT_PHASE_DRY_RUN" ? " --dry-run" : "") + " --json",
  "ImportService/GetImport": (r) => "bee import status " + q(r.id) + " --json",
  "ImportService/ListImports": (r) => "bee import list --org " + q(r.targetOrg) + " --json",
  "ImportService/GetFidelityReport": (r) => "bee import report " + q(r.id) + " --json",
  "ImportService/SetImportPhase": (r) => "bee import phase " + q(r.id) + " " + String(r.phase ?? "").replace("IMPORT_PHASE_", "").toLowerCase() + " --json",
};

/** CamelCase to kebab-case the way the CLI names verbs and flags (`WhoAmI` -> `who-am-i`, `changeRequestId` -> `change-request-id`). */
export function kebab(name: string): string {
  let out = "";
  for (let i = 0; i < name.length; i++) {
    const c = name.charAt(i);
    if (c === "_") {
      out += "-";
      continue;
    }
    if (i > 0 && c >= "A" && c <= "Z") {
      const prev = name.charAt(i - 1);
      const nextLower = /[a-z]/.test(name.charAt(i + 1));
      if (/[a-z0-9]/.test(prev) || (/[A-Z]/.test(prev) && nextLower)) out += "-";
    }
    out += c.toLowerCase();
  }
  return out;
}

/** The value of one request field as a CLI flag (SPEC §12.4: scalars as-is, repeats repeat the flag, messages as JSON or the `org/name` repo shorthand). */
function flag(key: string, v: unknown): string {
  const name = " --" + kebab(key);
  if (v === undefined || v === null || v === "") return "";
  if (Array.isArray(v)) return v.map((x) => flag(key, x)).join("");
  if (typeof v === "boolean") return v ? name : name + "=false";
  if (typeof v === "object") {
    const o = v as Record<string, unknown>;
    const keys = Object.keys(o);
    if (keys.length === 1 && (keys[0] === "fullName" || keys[0] === "id") && typeof o[keys[0] as string] === "string") return name + " " + q(o[keys[0] as string]);
    return name + " " + q(JSON.stringify(v));
  }
  return name + " " + q(v);
}

/** `bee <noun> <verb> --field value … --json` for the RPC behind a UI operation: noun and verb are the service and method in kebab-case. */
function catalogCli(op: Op): string {
  const w = wireOf(op.service, op.method, op.request ?? {});
  const body = (w.request ?? {}) as Req;
  return "bee " + kebab(w.service.replace(/Service$/, "")) + " " + kebab(w.method) + Object.keys(body).map((k) => flag(k, body[k])).join("") + (w.streaming ? " --follow" : "") + " --json";
}

/**
 * UI operations whose CLI form is the generated `bee <noun> <verb>` command of the RPC they
 * send (schemas/v1/cli/<noun>.<verb>.schema.json names each one): community, discovery, posts,
 * briefs, cloud and admin.
 */
export const CATALOG_CLI: ReadonlySet<string> = new Set([
  "OrgService/CreateOrg",
  "RepoService/ListMyRepos",
  "RepoService/GetRepoHome",
  "RepoService/CreateRepo",
  "ObjectService/GetServerInfo",
  "InsightsService/GetLanguages",
  "SearchService/SearchRepos",
  "SearchService/ListTrendingRepos",
  "SearchService/ListTrendingDevelopers",
  "SearchService/ListLeaderboard",
  "SearchService/GetTopic",
  "SearchService/ListFeaturedTopics",
  "SearchService/ListPublicActivity",
  "UserService/GetProfile",
  "UserService/StarRepo",
  "UserService/UnstarRepo",
  "UserService/ListStarred",
  "UserService/FollowUser",
  "UserService/UnfollowUser",
  "UserService/ListFollowers",
  "UserService/ListFollowing",
  "UserService/GetContributionGraph",
  "UserService/ListAchievements",
  "UserService/ListHomeFeed",
  "PostService/SetChangePost",
  "PostService/ClearChangePost",
  "PostService/GetChangePost",
  "PostService/BeginPostMediaUpload",
  "PostService/FinalizePostMedia",
  "PostService/ApprovePost",
  "PostService/WithdrawPost",
  "PostService/GetPost",
  "PostService/ListRepoPosts",
  "PostService/LikePost",
  "PostService/UnlikePost",
  "PostService/RepostPost",
  "PostService/UndoRepost",
  "PostService/ListFollowingFeed",
  "PostService/ListPostAdoptions",
  "UserService/FollowRepo",
  "UserService/UnfollowRepo",
  "UserService/IsFollowingRepo",
  "UserService/ListFollowedRepos",
  "UserService/ListRepoFollowers",
  "AgentService/GetAgentFeedTrigger",
  "AgentService/SetAgentFeedTrigger",
  "AgentService/ListAgentFeedTriggers",
  "AgentService/ListAdoptRuns",
  "IssueService/CreateIssue",
  "BriefService/SetChangeBrief",
  "BriefService/GetBriefChecklist",
  "BriefService/AddEvidence",
  "BriefService/BeginEvidenceUpload",
  "BriefService/FinalizeEvidence",
  "BriefService/GetEvidence",
  "BriefService/ListEvidence",
  "BriefService/UpdateEvidence",
  "BriefService/DeleteEvidence",
  "ChangeService/ListComments",
  "ChangeService/CommentChange",
  "AgentService/GetSessionsForChange",
  "BillingService/ListBillingPlans",
  "BillingService/GetAccountPlan",
  "BillingService/ListSeats",
  "BillingService/QueryUsage",
  "BillingService/ChangePlan",
  "OpsService/GetOpsHealth",
  "OpsService/GetClusterStatus",
  "OpsService/ValidateOpsConfig",
  "OpsService/GetEffectiveOpsConfig",
  "EnterpriseService/GetEnterprise",
  "EnterpriseService/UpdateEnterprise",
  "EnterpriseService/ListEnterpriseOrgs",
  "EnterpriseService/ListEnterpriseOwners",
  "EnterpriseService/GetSamlConfig",
  "EnterpriseService/UpdateSamlConfig",
  "EnterpriseService/TestSamlConfig",
  "EnterpriseService/GetScimConfig",
  "EnterpriseService/UpdateScimConfig",
  "EnterpriseService/CreateScimToken",
  "EnterpriseService/ListVerifiedDomains",
  "EnterpriseService/AddVerifiedDomain",
  "EnterpriseService/CheckVerifiedDomain",
  "EnterpriseService/DeleteVerifiedDomain",
  "EnterpriseService/GetTwoFactorRequirement",
  "EnterpriseService/UpdateTwoFactorRequirement",
  "EnterpriseService/ListIpAllowList",
  "EnterpriseService/CreateIpAllowListEntry",
  "EnterpriseService/DeleteIpAllowListEntry",
  "EnterpriseService/GetEnterprisePolicies",
  "EnterpriseService/UpdateEnterprisePolicies",
]);

export function toCli(op: Op): string {
  const key = op.service + "/" + op.method;
  if (CATALOG_CLI.has(key)) return catalogCli(op);
  const f = CLI[key];
  if (f) return f((op.request ?? {}) as Req);
  const w = wireOf(op.service, op.method, op.request ?? {});
  return "bee api bee.v1." + w.service + "/" + w.method + " --data " + q(JSON.stringify(w.request));
}

/** MCP tool names follow the RPC: `<service>_<method>` in snake case (e.g. change_get_change). */
export function mcpToolName(service: string, method: string): string {
  const key = service.replace(/Service$/, "");
  return key.charAt(0).toLowerCase() + key.slice(1).replace(/([a-z0-9])([A-Z])/g, "$1_$2").toLowerCase() + "_" + method.replace(/([a-z0-9])([A-Z])/g, "$1_$2").toLowerCase();
}

export function toMcp(op: Op): string {
  const w = wireOf(op.service, op.method, op.request ?? {});
  return JSON.stringify({ jsonrpc: "2.0", id: 1, method: "tools/call", params: { name: mcpToolName(w.service, w.method), arguments: w.request ?? {} } }, null, 2);
}

/**
 * curl for unary calls; a Connect envelope via printf for server-streaming ones. The HTTP and
 * MCP forms show the real bee.v1 RPC behind the UI operation (`lib/api/ops`), with its
 * proto3-JSON request; a screen that composes several calls shows the primary one.
 */
export function toCurl(op: Op, base = "$BEE_API_URL"): string {
  const w = wireOf(op.service, op.method, op.request ?? {});
  const body = JSON.stringify(w.request ?? {});
  const url = '"' + base + "/bee.v1." + w.service + "/" + w.method + '"';
  const auth = '  -H "Authorization: Bearer $BEE_TOKEN" \\';
  if (w.streaming) {
    const len = new TextEncoder().encode(body).length;
    const hdr = "\\x00" + [24, 16, 8, 0].map((s) => "\\x" + ((len >> s) & 255).toString(16).padStart(2, "0")).join("");
    return [
      "# server-streaming: the request is one Connect envelope (flag byte + 4-byte length + JSON)",
      "printf '" + hdr + "%s' " + q(body) + " | \\",
      "curl -sSN -X POST " + url + " \\",
      auth,
      '  -H "Content-Type: application/connect+json" \\',
      '  -H "Connect-Protocol-Version: 1" \\',
      "  --data-binary @-",
    ].join("\n");
  }
  return ["curl -sS -X POST " + url + " \\", auth, '  -H "Content-Type: application/json" \\', "  -d " + q(body)].join("\n");
}

export function httpLine(op: Op): string {
  const w = wireOf(op.service, op.method, op.request ?? {});
  return "POST /bee.v1." + w.service + "/" + w.method;
}
