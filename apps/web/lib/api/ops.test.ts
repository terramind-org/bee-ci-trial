/**
 * The mapping layer targets the real wire API, and screens reach it only through the client.
 */
import { readdirSync, readFileSync, statSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import type { DescService } from "@bufbuild/protobuf";
import { describe, expect, it } from "vitest";
import * as agents from "../gen/bee/v1/agents_pb";
import * as billing from "../gen/bee/v1/billing_pb";
import * as briefs from "../gen/bee/v1/briefs_pb";
import * as enterprise from "../gen/bee/v1/enterprise_pb";
import * as insights from "../gen/bee/v1/insights_pb";
import * as ops from "../gen/bee/v1/ops_pb";
import * as users from "../gen/bee/v1/users_pb";
import * as audit from "../gen/bee/v1/audit_pb";
import * as auth from "../gen/bee/v1/auth_pb";
import * as forge from "../gen/bee/v1/forge_pb";
import * as imports from "../gen/bee/v1/import_pb";
import * as issues from "../gen/bee/v1/issues_pb";
import * as policy from "../gen/bee/v1/policy_pb";
import * as posts from "../gen/bee/v1/posts_pb";
import * as projects from "../gen/bee/v1/projects_pb";
import * as releases from "../gen/bee/v1/releases_pb";
import * as security from "../gen/bee/v1/security_pb";
import * as vcs from "../gen/bee/v1/vcs_pb";
import * as workflows from "../gen/bee/v1/workflows_pb";
import * as apps from "../gen/bee/v1/apps_pb";
import * as packages from "../gen/bee/v1/packages_pb";
import * as pages from "../gen/bee/v1/pages_pb";
import * as search from "../gen/bee/v1/search_pb";
import * as teams from "../gen/bee/v1/teams_pb";
import { SERVICE_KEYS } from "./client";
import { OPS, wireOf } from "./ops";
import { adoptOutcome } from "./ops/posts";
import { CATALOG_CLI, kebab, toCli, toCurl, toMcp } from "../copyas";

const services = new Map<string, DescService>();
for (const mod of [briefs, billing, enterprise, insights, ops, users, agents, audit, auth, forge, imports, issues, policy, posts, projects, releases, security, vcs, workflows, apps, packages, pages, search, teams])
  for (const v of Object.values(mod)) if (typeof v === "object" && v && (v as { kind?: string }).kind === "service") services.set((v as DescService).name, v as DescService);

/** A request with every field any UI method reads, so each `wire` can run. */
const sample = {
  repo: { fullName: "acme/api" },
  org: "acme",
  id: "dependency:1",
  changeRequestId: "cr",
  threadId: "t",
  principalId: "p",
  runId: "r",
  jobId: "j",
  number: 12,
  issueNumber: 12,
  projectId: "pj",
  toColumnId: "done",
  tag: "v1.0.0",
  source: "",
  revision: 1,
  ref: "main",
  nameOrId: "acme",
  owner: "acme",
  query: "lineTotal",
  name: "main",
  reason: "x",
  commentIds: [],
  assigneeIds: [],
  scope: { actions: ["read"] },
  path: "",
  at: { expr: "main" },
  from: { expr: "main" },
  to: { expr: "main" },
  login: "mara",
  fullName: "lattice/engine",
  limit: 5,
  planId: "team",
};

describe("mapping layer", () => {
  it("implements every service of the method table", () => {
    expect(Object.keys(OPS).sort()).toEqual(Object.keys(SERVICE_KEYS).sort());
  });

  it("every UI method sends a real bee.v1 RPC with a valid request", () => {
    const seen: string[] = [];
    for (const [service, methods] of Object.entries(OPS)) {
      for (const method of Object.keys(methods)) {
        const w = wireOf(service, method, sample);
        const desc = services.get(w.service);
        expect(desc, service + "/" + method + " → " + w.service).toBeDefined();
        const m = desc?.methods.find((x) => x.name === w.method);
        expect(m, service + "/" + method + " → " + w.service + "/" + w.method).toBeDefined();
        expect(w.streaming).toBe(m?.methodKind === "server_streaming");
        seen.push(service + "/" + method);
      }
    }
    expect(seen.length).toBeGreaterThan(110);
  });

  it("Copy as shows the real RPC, not the UI method name", () => {
    const op = { service: "ChangeService" as const, method: "ListThreads", request: { repo: sample.repo, changeRequestId: "cr" } };
    expect(toCurl(op)).toContain("/bee.v1.ChangeService/ListReviewThreads");
    expect(JSON.parse(toMcp(op)).params.name).toBe("change_list_review_threads");
    const logs = { service: "WorkflowService" as const, method: "StreamLogs", request: { repo: sample.repo, runId: "r", jobId: "j" } };
    expect(toCurl(logs)).toContain("application/connect+json");
    expect(toCurl(logs)).toContain("/bee.v1.WorkflowService/TailWorkflowJobLogs");
  });
});

describe("community, discovery, posts, cloud and admin methods", () => {
  const cliDir = fileURLToPath(new URL("../../../../schemas/v1/cli", import.meta.url));
  const schemas = new Set(readdirSync(cliDir).map((n) => n.replace(/\.schema\.json$/, "")));
  const NEW = [
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
    ...Object.keys(OPS.PostService).map((m) => "PostService/" + m),
    "AgentService/GetAgentFeedTrigger",
    "AgentService/SetAgentFeedTrigger",
    "AgentService/ListAgentFeedTriggers",
    "AgentService/ListAdoptRuns",
    "IssueService/CreateIssue",
    ...Object.keys(OPS.BriefService).map((m) => "BriefService/" + m),
    "ChangeService/ListComments",
    "ChangeService/CommentChange",
    "AgentService/GetSessionsForChange",
    ...Object.keys(OPS.UserService).map((m) => "UserService/" + m),
    ...Object.keys(OPS.BillingService).map((m) => "BillingService/" + m),
    ...Object.keys(OPS.OpsService).map((m) => "OpsService/" + m),
    ...Object.keys(OPS.EnterpriseService).map((m) => "EnterpriseService/" + m),
  ];

  it("every new method has a mapping and a dedicated CLI command", () => {
    expect([...CATALOG_CLI].sort()).toEqual([...NEW].sort());
    for (const key of NEW) {
      const [service = "", method = ""] = key.split("/");
      const w = wireOf(service, method, sample);
      expect(services.get(w.service), key).toBeDefined();
      const cli = toCli({ service: service as never, method, request: sample });
      expect(cli, key).toMatch(/^bee [a-z-]+ [a-z-]+/);
      const [, noun = "", verb = ""] = cli.split(" ");
      expect(schemas.has(noun + "." + verb), key + " -> " + noun + "." + verb).toBe(true);
    }
  });

  it("names verbs and flags the way the CLI does", () => {
    expect(kebab("WhoAmI")).toBe("who-am-i");
    expect(kebab("GetSBOMExport")).toBe("get-sbom-export");
    expect(kebab("changeRequestId")).toBe("change-request-id");
  });

  it("Copy as renders the CLI, curl and MCP forms of the real RPC", () => {
    const star = { service: "UserService" as const, method: "StarRepo", request: { repo: { fullName: "lattice/engine" } } };
    expect(toCli(star)).toBe("bee user star-repo --repo lattice/engine --json");
    expect(toCurl(star)).toContain("/bee.v1.UserService/StarRepo");
    expect(JSON.parse(toMcp(star)).params).toEqual({ name: "user_star_repo", arguments: { repo: { fullName: "lattice/engine" } } });

    const plan = { service: "BillingService" as const, method: "ChangePlan", request: { org: "acme", planId: "team", seats: 12 } };
    expect(toCli(plan)).toBe("bee billing change-billing-plan --scope '{\"org\":\"acme\"}' --plan-id team --seats 12 --json");
    expect(toCurl(plan)).toContain("/bee.v1.BillingService/ChangeBillingPlan");
    expect(JSON.parse(toMcp(plan)).params.name).toBe("billing_change_billing_plan");

    const saml = { service: "EnterpriseService" as const, method: "UpdateSamlConfig", request: { scope: { enterprise: "acme-group" }, enabled: false, idpSsoUrl: "https://idp.example/sso" } };
    expect(toCli(saml)).toBe("bee enterprise update-saml-sso-config --scope '{\"enterprise\":\"acme-group\"}' --enabled=false --idp-sso-url https://idp.example/sso --json");
    expect(JSON.parse(toMcp(saml)).params.name).toBe("enterprise_update_saml_sso_config");

    const board = { service: "SearchService" as const, method: "ListLeaderboard", request: { window: "TRENDING_WINDOW_WEEKLY", subject: "LEADERBOARD_SUBJECT_AGENTS", metric: "LEADERBOARD_METRIC_LANDS" } };
    expect(toCli(board)).toBe("bee search list-leaderboard --window TRENDING_WINDOW_WEEKLY --subject LEADERBOARD_SUBJECT_AGENTS --metric LEADERBOARD_METRIC_LANDS --json");
  });

  it("Copy as renders posts, follows and feed triggers as their RPCs", () => {
    const like = { service: "PostService" as const, method: "LikePost", request: { postId: "01P" } };
    expect(toCli(like)).toBe("bee post like-post --post-id 01P --json");
    expect(JSON.parse(toMcp(like)).params).toEqual({ name: "post_like_post", arguments: { postId: "01P" } });
    const set = { service: "PostService" as const, method: "SetChangePost", request: { repo: sample.repo, changeId: "cr", text: "It works", mediaIds: ["m1", "m2"] } };
    expect(toCli(set)).toBe("bee post set-change-post --repo acme/api --change-id cr --text 'It works' --media-ids m1 --media-ids m2 --json");
    const follow = { service: "UserService" as const, method: "FollowRepo", request: { repo: { fullName: "lattice/engine" } } };
    expect(toCli(follow)).toBe("bee user follow-repo --repo lattice/engine --json");
    expect(toCurl(follow)).toContain("/bee.v1.UserService/FollowRepo");
    const trigger = { service: "AgentService" as const, method: "SetAgentFeedTrigger", request: { repo: sample.repo, agent: "p", enabled: true, dailyUsdBudget: 5, maxRunsPerDay: 10, sources: [{ fullName: "lattice/engine" }] } };
    expect(toCli(trigger)).toBe("bee agent set-agent-feed-trigger --repo acme/api --agent p --enabled --daily-usd-budget 5 --max-runs-per-day 10 --sources lattice/engine --json");
    expect(JSON.parse(toMcp(trigger)).params.name).toBe("agent_set_agent_feed_trigger");
  });

  it("Copy as renders briefs, evidence, comments and sessions as their RPCs", () => {
    const brief = { service: "BriefService" as const, method: "GetBriefChecklist", request: { repo: sample.repo, changeId: "cr", revision: 2 } };
    expect(toCli(brief)).toBe("bee brief get-brief-checklist --repo acme/api --change-id cr --revision 2 --json");
    expect(JSON.parse(toMcp(brief)).params).toEqual({ name: "brief_get_brief_checklist", arguments: { repo: sample.repo, changeId: "cr", revision: 2 } });
    const list = { service: "BriefService" as const, method: "ListEvidence", request: { repo: sample.repo, changeIds: ["a", "b"], heroOnly: true } };
    expect(toCli(list)).toBe("bee brief list-evidence --repo acme/api --change-ids a --change-ids b --hero-only --json");
    expect(toCurl(list)).toContain("/bee.v1.BriefService/ListEvidence");
    const comment = { service: "ChangeService" as const, method: "CommentChange", request: { repo: sample.repo, changeRequestId: "cr", body: 'Decision on "Ship it?": Yes' } };
    expect(toCli(comment)).toBe("bee change create-change-comment --repo acme/api --change-request-id cr --body 'Decision on \"Ship it?\": Yes' --json");
    expect(toCurl({ service: "ChangeService", method: "ListComments", request: { repo: sample.repo, changeRequestId: "cr" } })).toContain("/bee.v1.ChangeService/ListChangeComments");
    const sessions = { service: "AgentService" as const, method: "GetSessionsForChange", request: { repo: sample.repo, changeRequest: "101" } };
    expect(toCli(sessions)).toBe("bee agent get-sessions-for-change --repo acme/api --change-request 101 --json");
  });

  it("composed methods show their primary RPC", () => {
    expect(wireOf("RepoService", "GetRepoHome", { fullName: "lattice/engine" })).toMatchObject({ service: "RepoService", method: "GetRepo", request: { repo: { fullName: "lattice/engine" } } });
    expect(wireOf("UserService", "GetProfile", { owner: "rhea" })).toMatchObject({ service: "UserService", method: "GetUser", request: { login: "rhea" } });
    expect(wireOf("BillingService", "GetAccountPlan", { org: "acme" })).toMatchObject({ method: "GetAccountBillingPlan", request: { scope: { org: "acme" } } });
    expect(wireOf("AgentService", "ListAdoptRuns", { repo: sample.repo, agentPrincipalId: "p" })).toMatchObject({
      method: "ListAgentRuns",
      request: { repo: sample.repo, agentPrincipalId: "p", taskKind: "AGENT_TASK_KIND_ADOPT_EVALUATE" },
    });
    expect(wireOf("SearchService", "ListPublicActivity", { limit: 50 })).toMatchObject({ method: "SearchRepos", request: { query: "is:public sort:updated", page: { pageSize: 20 } } });
  });
});

describe("feed-trigger runs", () => {
  const run = (state: string, summary = "", failureReason = "") => ({ state, summary, failureReason, repo: { fullName: "acme/api" } }) as never;
  it("reads the outcome from the run's state and summary", () => {
    expect(adoptOutcome(run("AGENT_RUN_STATE_RUNNING"))).toEqual({ outcome: "running", reason: "" });
    expect(adoptOutcome(run("AGENT_RUN_STATE_FAILED", "", "out of budget"))).toEqual({ outcome: "failed", reason: "out of budget" });
    expect(adoptOutcome(run("AGENT_RUN_STATE_SUCCEEDED", "not relevant: no rebase here"))).toEqual({ outcome: "not_relevant", reason: "no rebase here" });
    expect(adoptOutcome(run("AGENT_RUN_STATE_SUCCEEDED", "opened issue #23: Record usage"))).toMatchObject({ outcome: "opened_issue", opened: { kind: "issue", number: "23", title: "Record usage", repo: { fullName: "acme/api" } } });
    expect(adoptOutcome(run("AGENT_RUN_STATE_SUCCEEDED", "opened change request #110: Port the walk"))).toMatchObject({ outcome: "opened_change", opened: { kind: "change", number: "110" } });
    // A summary the format does not cover is shown as it is, never dropped.
    expect(adoptOutcome(run("AGENT_RUN_STATE_SUCCEEDED", "Looked at it; nothing to do."))).toEqual({ outcome: "not_relevant", reason: "Looked at it; nothing to do." });
  });
});

describe("screens use only the public client", () => {
  const root = fileURLToPath(new URL("../..", import.meta.url));
  const files: string[] = [];
  const walk = (d: string) => {
    for (const n of readdirSync(d)) {
      const p = join(d, n);
      if (statSync(p).isDirectory()) walk(p);
      else if (/\.tsx?$/.test(n)) files.push(p);
    }
  };
  walk(join(root, "app"));
  walk(join(root, "components"));

  it("no screen imports generated code, the fixture or the transport internals, or hand-rolls RPC requests", () => {
    const allowedRoute = /app\/api\/connect\//;
    const offenders = files.filter((f) => {
      const src = readFileSync(f, "utf8");
      if (allowedRoute.test(f)) return false;
      return /from "@\/lib\/(gen\/|api\/fixture|api\/rpc|api\/http)/.test(src) || /fetch\([^)]*bee\.v1\./.test(src);
    });
    expect(offenders).toEqual([]);
    expect(files.length).toBeGreaterThan(30);
  });
});
