/**
 * Who may call what without a token. Mirrors the server: a request with no Authorization
 * header is anonymous, and may only make the RPCs in `ANONYMOUS_READ_METHODS`
 * (crates/bee-server/src/access.rs) plus the open ones. A repo it names must be public;
 * private repos and repos that do not exist answer the same `REPO_NOT_FOUND`, so existence
 * never leaks. Signed-in callers get the same repo check against what they may read.
 */
import { BeeError } from "../transport";
import type { FixtureStore } from "./store";

const p = (service: string, ...methods: string[]) => methods.map((m) => `/bee.v1.${service}/${m}`);

/** Same list as `ANONYMOUS_READ_METHODS` in access.rs. */
export const ANONYMOUS_READ_METHODS: ReadonlySet<string> = new Set([
  ...p("ObjectService", "GetObjects", "HasObjects", "ResolveRef"),
  ...p("QueryService", "GetChangesets", "ReadPath", "ListTree", "Log", "Blame", "Diff", "IsAncestor", "MergeBase", "Resolve", "GetDerived", "ListDerivedTypes", "CompareRevisions", "FindCopies"),
  ...p("RefService", "ListRefs", "GetRefLog"),
  ...p("RepoService", "GetRepo", "GetContext"),
  // The tabs of a public repo. AuthService.GetPrincipal stays closed (it returns external ids),
  // so names the server sends bare stay bare for a signed-out visitor.
  ...p("ChangeService", "ListChanges", "GetChange", "GetMergeQueue", "EvaluateLanding", "ListReviews", "ListReviewThreads", "DiffChangeRevisions"),
  ...p("CheckService", "ListChecks", "ListCheckAnnotations"),
  ...p("LeaseService", "ListLeases"),
  ...p("WorkflowService", "ListWorkflowRuns", "GetWorkflowRun", "ListWorkflowJobs", "TailWorkflowJobLogs"),
  ...p("IssueService", "ListIssues", "GetIssueByNumber", "ListIssueComments", "ListIssueSubIssues"),
  ...p("ProjectService", "ListProjects", "ListProjectFields", "ListProjectItems"),
  ...p("ReleaseService", "ListReleases"),
  ...p("PostService", "GetPost", "ListRepoPosts"),
  ...p("BriefService", "GetBriefChecklist", "GetEvidence", "ListEvidence"),
  ...p("BriefService", "GetBriefChecklist", "GetEvidence", "ListEvidence"),
  ...p("PackageService", "ListPackages", "ListPackageVersions"),
  ...p("PagesService", "GetPagesSite", "ListPagesDeployments"),
  ...p("InsightsService", "GetLanguageStats"),
  ...p("EventService", "Subscribe"),
  // A public repo's posts: permalinks and the Updates tab.
  ...p("PostService", "GetPost", "ListRepoPosts"),
  ...p(
    "SearchService",
    "SearchCode",
    "StreamSearchCode",
    "SearchSymbols",
    "GetSymbolDefinitions",
    "GetSymbolReferences",
    "GetDocumentSymbols",
    "SearchIssues",
    "SearchRepos",
    "SearchChangesets",
    "SearchUsers",
    "SearchTopics",
    "SearchDiscussions",
    "SearchPackages",
    "ParseSearchQuery",
    "ListTrendingRepos",
    "ListTrendingDevelopers",
    "ListLeaderboard",
    "GetTopic",
    "ListFeaturedTopics",
    "ListTopicCollections",
    "GetTopicCollection",
  ),
  ...p(
    "UserService",
    "GetUser",
    "ListUserPinnedItems",
    "ListUserFollowers",
    "ListUserFollowing",
    "ListStarredRepos",
    "ListRepoStargazers",
    "GetStarList",
    "ListStarLists",
    "ListStarListRepos",
    "GetUserContributionGraph",
    "GetUserActivityOverview",
    "ListUserAchievements",
  ),
]);

/** Answered for everyone: the server's identity, and signing in itself. */
export const OPEN_METHODS: ReadonlySet<string> = new Set([...p("ObjectService", "GetServerInfo"), ...p("AuthService", "Login")]);

interface Named {
  id?: string;
  fullName?: string;
}

/** Every repo a request names: `repo` refs, `repoId`, `repoIds` and `fullName`, at any depth. */
export function namedRepos(req: unknown): Named[] {
  const out: Named[] = [];
  const walk = (v: unknown, key: string) => {
    if (Array.isArray(v)) {
      for (const x of v) walk(x, key);
    } else if (v && typeof v === "object") {
      if (key === "repo") {
        const r = v as Named;
        if (r.id || r.fullName) out.push({ id: r.id, fullName: r.fullName });
      }
      for (const [k, x] of Object.entries(v)) walk(x, k);
    } else if (typeof v === "string" && v) {
      if (key === "repoId" || key === "repoIds") out.push({ id: v });
      else if (key === "fullName" && !out.some((o) => o.fullName === v)) out.push({ fullName: v });
    }
  };
  walk(req, "");
  return out;
}

/** Throws the error the real server would give before any handler runs. */
export function gate(s: FixtureStore, method: string, req: unknown, token: string | undefined): void {
  if (OPEN_METHODS.has(method)) return;
  if (!token) {
    if (!ANONYMOUS_READ_METHODS.has(method)) throw new BeeError("unauthenticated", { reason: "UNAUTHENTICATED", message: "sign in to use this" });
  }
  const viewer = s.viewer(token);
  for (const ref of namedRepos(req)) {
    if (ref.fullName?.endsWith("/*")) continue;
    const repo = s.lookup(ref);
    if (!repo || !s.canRead(viewer, repo)) throw new BeeError("not_found", { reason: "REPO_NOT_FOUND", message: "no repository " + (ref.fullName ?? ref.id) });
  }
}
