/**
 * The tools a page offers an agent (docs/spec/web-ui.md §2, "Two modes"): each action the page
 * performs, named as the MCP tool an agent calls for it and marked read, write or needs a person.
 *
 * Nothing here is a name typed by hand. A page lists operations from the UI method table
 * (lib/api/methods.ts; `OpRef` is checked against it at compile time), and the tool's name is
 * derived from the bee.v1 call the mapping layer sends for that operation (lib/api/ops), exactly
 * as Copy as derives it. A page that stops using an operation drops it from its list here.
 */
import type { MethodName, ServiceName } from "./api/methods";
import { OPS, wireOf } from "./api/ops";
import { mcpToolName } from "./copyas";

/** `Service/Method` for every entry of the UI method table, and nothing else. */
export type OpRef = { [S in ServiceName]: `${S}/${MethodName<S>}` }[ServiceName];

export type ToolKind = "read" | "write" | "needs a person";

export interface PageTool {
  /** The UI operation, `Service/Method`. */
  op: OpRef;
  /** The MCP tool behind it: `<service>_<method>` of the bee.v1 call, in snake case. */
  name: string;
  /** What it does, in a few words. */
  what: string;
  kind: ToolKind;
}

/**
 * Operations that approve or destroy something. In the person's view these sit behind the kit's
 * `ConfirmButton`; an agent that calls one sends the request to the person who answers for it.
 */
export const NEEDS_PERSON: ReadonlySet<OpRef> = new Set<OpRef>([
  "ChangeService/SubmitReview",
  "ChangeService/Enqueue",
  "ChangeService/CloseChange",
  "WriteService/Land",
  "PostService/ApprovePost",
  "AuthService/Revoke",
  // Changing the ceiling for every agent owned by a person needs their administrator's confirmation.
  "AgentService/SetAgentBudget",
  // Setting an organisation role can grant access or remove the member.
  "OrgService/SetMember",
  "OrgService/CreateOrgInvitation",
  "BillingService/ChangePlan",
  "SecurityService/DismissAlert",
  "RepoService/ForkRepo",
  "RepoService/SyncFork",
  "RepoService/UpdateRepo",
  // Issue state is reversible, but both controls explicitly require a person.
  "IssueService/SetIssueState",
  // Stopping a run or turning off a workflow requires the CI confirmation controls.
  "AgentService/CancelAgentRun",
  "SwarmService/DecideSwarmJoin",
  "SafetyService/ResolveFinding",
  "SafetyService/SetVerdict",
  "SafetyService/RequestSafetyReview",
  "SafetyService/AcknowledgeSafetyAttention",
  "SafetyService/StopEverything",
  "SafetyService/ResumeEverything",
  "WorkflowService/CancelRun",
  "WorkflowService/DisableWorkflow",
]);

/** A method that only reads starts with one of these words. Everything else changes something. */
const READS = /^(Get|List|Search|Evaluate|Resolve|Read|Query|Log|Blame|Diff|Compare|Subscribe|Stream|Validate|Test|Check|Is|WhoAmI)(?=[A-Z]|$)/;
/** Reads whose names do not start with a reading word, and writes whose names do. */
// Record lists the chain verifier as a read because it leaves the log untouched.
const READ_ANYWAY: ReadonlySet<string> = new Set(["ReleaseService/GenerateNotes", "AuditService/VerifyAuditChain", "SafetyService/SpotCheck"]);
const WRITE_ANYWAY: ReadonlySet<string> = new Set(["EnterpriseService/CheckVerifiedDomain"]);

export function kindOf(op: OpRef, request?: unknown): ToolKind {
  if (op === "SwarmService/SetSwarmState" && typeof request === "object" && request !== null
      && "state" in request && (request.state === "SWARM_STATE_STOPPED" || request.state === "SWARM_STATE_FINISHED")) return "needs a person";
  if (NEEDS_PERSON.has(op)) return "needs a person";
  const method = op.slice(op.indexOf("/") + 1);
  if (WRITE_ANYWAY.has(op)) return "write";
  return READ_ANYWAY.has(op) || READS.test(method) ? "read" : "write";
}

/** Every operation the mapping layer implements, as `Service/Method`, in the table's order. */
export function allOps(): OpRef[] {
  return Object.entries(OPS as unknown as Record<string, Record<string, unknown>>).flatMap(([service, methods]) => Object.keys(methods).map((m) => (service + "/" + m) as OpRef));
}

export function isOp(ref: string): ref is OpRef {
  const [service, method] = ref.split("/");
  return !!service && !!method && !!(OPS as unknown as Record<string, Record<string, unknown> | undefined>)[service]?.[method];
}

/**
 * The MCP tool an agent calls for a UI operation. A few operations send a different call
 * depending on the request (closing and reopening an issue); `request` picks the one meant.
 */
export function toolName(op: OpRef, request: unknown = {}): string {
  const [service, method] = op.split("/") as [string, string];
  try {
    const w = wireOf(service, method, request);
    return mcpToolName(w.service, w.method);
  } catch {
    // A mapping that cannot be built from an empty request is still named after its own method.
    return mcpToolName(service, method);
  }
}

export function toolFor(op: OpRef, what: string, request?: unknown): PageTool {
  return { op, name: toolName(op, request), what, kind: kindOf(op, request) };
}

export type PageId = "agents" | "agent-run" | "agent-principal" | "needs" | "code" | "pulls" | "pull" | "issues" | "issue"
  | "issue-board" | "settings" | "ci" | "run" | "workflows"
  | "repository-header" | "repositories"
  | "feed" | "everything" | "post" | "compose" | "updates";

/**
 * What each screen does, as the operations it really calls: its reads first, then what its
 * controls write, then what waits for a person.
 */
// The feed, permalink and repository list share the same published-post actions.
const POST_ACTIONS: [OpRef, string][] = [
  ["IssueService/CreateIssue", "Open an issue inspired by this post"],
  ["PostService/LikePost", "Like a post"],
  ["PostService/UnlikePost", "Remove your like"],
  ["PostService/RepostPost", "Repost to your followers"],
  ["PostService/UndoRepost", "Remove your repost"],
];

const PAGES: Record<PageId, [op: OpRef, what: string, request?: unknown][]> = {
  feed: [
    ["PostService/ListFollowingFeed", "Read posts from repositories and people you follow"],
    ...POST_ACTIONS,
  ],
  everything: [
    ["SearchService/SearchRepos", "Find repositories you can read"],
    ["PostService/ListRepoPosts", "Read published posts from a repository"],
    ...POST_ACTIONS,
  ],
  post: [
    ["PostService/GetPost", "Read this post and its attachments"],
    ["PostService/ListPostAdoptions", "Read the issues and pull requests inspired by this post"],
    ["ChangeService/ListComments", "Read comments on the carrying pull request"],
    ...POST_ACTIONS,
    ["PostService/ApprovePost", "Approve an agent's post"],
  ],
  compose: [
    ["ChangeService/ListChanges", "Find pull requests to attach"],
    ["IssueService/ListIssues", "Find an issue to attach"],
    ["ReleaseService/ListReleases", "Find a published release to attach"],
    ["AgentService/ListAdoptRuns", "Find an adoption run to attach"],
    ["PostService/BeginPostMediaUpload", "Start uploading an image or video"],
    ["PostService/FinalizePostMedia", "Finish the upload and describe it"],
    ["PostService/CreatePost", "Write a post in a repository"],
  ],
  updates: [
    ["PostService/ListRepoPosts", "Read published posts from this repository"],
    ...POST_ACTIONS,
  ],
  // Settings describes the read-only draft and the confirmed repository visibility write.
  settings: [
    ["PolicyService/GetPolicy", "Read the effective repository rules"],
    ["PolicyService/EvaluatePolicy", "Replay a draft against recent pull requests"],
    ["AgentService/ResolveAgentInstructions", "Resolve instructions and their sources"],
    ["RepoService/ListCollaborators", "List repository roles for collaborators"],
    ["RepoService/ListDeployKeys", "List deploy keys and their access"],
    ["TeamService/ListRepoTeams", "List team roles for this repository"],
    ["RepoService/GetRepo", "Read the repository visibility"],
    ["ChangeService/OpenChange", "Open a pull request proposing a change to the rules or instructions"],
    ["RepoService/UpdateRepo", "Change repository visibility"],
  ],
  needs: [
    ["SafetyService/ListSafetyAttention", "Read tripwires and concerns addressed to you"],
    ["OrgService/ListOrgs", "The organisations you belong to"],
    ["RepoService/ListRepos", "The repositories of an organisation"],
    ["ChangeService/ListChanges", "Pull requests in a repository, by state"],
    ["ChangeService/EvaluateLanding", "Which rules a pull request still has to meet"],
    ["CheckService/ListChecks", "Checks reported on a commit"],
    ["ChangeService/ListReviews", "Reviews on a pull request"],
    ["AgentService/ListAgents", "Agents, and the person each answers to"],
    ["UserService/ListHomeFeed", "What merged in the repositories you can see"],
  ],
  repositories: [
    ["UserService/GetViewer", "Read your account login for the Yours filter"],
    ["SearchService/SearchRepos", "Find repositories you can read"],
    ["RepoService/ListMyRepos", "Repositories in your organizations"],
    ["UserService/ListStarred", "Repositories you starred"],
    ["RepoService/GetRepo", "Visibility and last update of a starred repository"],
    ["LeaseService/ListLeases", "Agents with active editing leases"],
    ["ChangeService/ListChanges", "Open pull requests for your Needs you count"],
    ["ChangeService/EvaluateLanding", "Which rules those pull requests still need to meet"],
    ["CheckService/ListChecks", "Checks reported on a commit"],
    ["ChangeService/ListReviews", "Reviews on a pull request"],
    ["RepoService/CreateRepo", "Create a repository"],
    ["ImportService/StartImport", "Import a repository from GitHub"],
  ],
  "repository-header": [
    ["RepoService/GetRepo", "Repository details, visibility and fork relationship"],
    ["UserService/ListRepoFollowers", "Who follows this repository"],
    ["RepoService/ListForks", "Forks visible to you"],
    ["UserService/FollowRepo", "Follow the repository"],
    ["UserService/UnfollowRepo", "Stop following the repository"],
    ["UserService/StarRepo", "Star the repository"],
    ["UserService/UnstarRepo", "Remove your star"],
    ["RepoService/ForkRepo", "Create a fork in your account or organization"],
    ["RepoService/SyncFork", "Bring the default branch up to date with upstream"],
  ],
  code: [
    ["RepoService/GetRepoHome", "The repository, its README and who is editing"],
    ["SwarmService/ListRepoWork", "Find the work attached to editing activity"],
    ["QueryService/ListTree", "List a directory at a ref"],
    ["QueryService/ReadPath", "Read a file at a ref"],
    ["QueryService/Log", "Commits, with the agent and the person behind each"],
  ],
  agents: [
    ["SwarmService/ListRepoWork", "List swarms and individual runs in activity order"],
    ["AgentService/ListAgents", "Find agent profiles and the people they answer to"],
    ["ChangeService/GetChange", "Read a linked pull request and its state"],
    ["ChangeService/GetChangeByNumber", "Read a pull request reported by a live run"],
  ],
  "agent-run": [
    ["AgentService/GetAgentRun", "Read the run's goal, state and report"],
    ["AgentService/ListAgentRunEvents", "Read session logs, events and messages"],
    ["SwarmService/ListRepoWork", "Find the run's pull requests and visible finances"],
    ["ChangeService/GetChange", "Read a linked pull request and its state"],
    ["ChangeService/GetChangeByNumber", "Read a pull request reported by a live run"],
    ["AgentService/SendAgentRunMessage", "Send a steering message to this agent"],
    ["AgentService/RetryAgentRun", "Start a new run with the same task"],
    ["AgentService/CancelAgentRun", "Stop the run"],
  ],
  "agent-principal": [
    ["AgentService/GetAgent", "Read the agent and the person it answers to"],
    ["AgentService/ListAgentRuns", "Read this agent’s repository runs, including adoption outcomes"],
    ["AgentService/ListProvenance", "Read recorded work, model, session and cost"],
    ["LeaseService/ListLeases", "Find the agent's current file claims"],
    ["UserService/ListFollowedRepos", "Read the repositories this agent follows"],
    ["AgentService/GetAgentFeedTrigger", "Read its feed trigger"],
    ["AgentService/ListTokens", "Read the agent's tokens and their scopes"],
    ["AuthService/MintToken", "Mint a narrowed token"],
    ["AgentService/SetAgentFeedTrigger", "Update the feed trigger"],
    ["AuthService/Revoke", "Revoke a token and its descendants"],
  ],
  pulls: [
    ["ChangeService/ListChanges", "Pull requests in this repository, by state or query"],
    ["ChangeService/EvaluateLanding", "Which rules a pull request still has to meet"],
    ["CheckService/ListChecks", "Checks reported on a commit"],
    ["ChangeService/ListQueue", "The merge queue"],
  ],
  pull: [
    ["ChangeService/GetChange", "The pull request: title, state, revisions, brief"],
    ["ChangeService/EvaluateLanding", "The rules it still has to meet"],
    ["CheckService/ListChecks", "Its checks"],
    ["ChangeService/GetChangeDiff", "The files it changes"],
    ["BriefService/ListEvidence", "The evidence attached to it"],
    ["AgentService/GetSessionsForChange", "Who did the work: model, runtime, cost, prompt digest"],
    ["ChangeService/CommentChange", "Comment on it"],
    ["ChangeService/SubmitReview", "Approve it, or request changes"],
    ["ChangeService/Enqueue", "Merge it through the merge queue"],
    ["ChangeService/CloseChange", "Close it without merging"],
  ],
  // The issue routes share this registry so the agent view names the actual wire tools.
  issues: [
    ["IssueService/ListIssues", "Issues in this repository, open or closed"],
    ["IssueService/CreateIssue", "Open an issue with a title, description, labels and assignee"],
  ],
  ci: [
    ["WorkflowService/ListRuns", "List CI runs and their jobs"],
    ["ChangeService/ListChanges", "Name the pull requests the runs checked"],
    ["ChangeService/GetChange", "Read a pull request linked to a run"],
    ["DeploymentService/ListDeployments", "List deployments and their states"],
  ],
  run: [
    ["WorkflowService/GetRun", "Read the run and its jobs"],
    ["ChangeService/GetChange", "Read the pull request this run checked"],
    ["OrgService/ListMembers", "Find the person the agent answers to"],
    ["WorkflowService/StreamLogs", "Read a job log"],
    ["WorkflowService/Rerun", "Re-run all jobs", { failedOnly: false }],
    ["WorkflowService/Rerun", "Re-run failed jobs", { failedOnly: true }],
    ["WorkflowService/CancelRun", "Cancel a run"],
  ],
  workflows: [
    ["WorkflowService/ListWorkflows", "List the workflows and their states"],
    ["WorkflowService/GetWorkflowBadge", "Get a workflow badge"],
    ["WorkflowService/EnableWorkflow", "Enable a workflow"],
    ["WorkflowService/DisableWorkflow", "Disable a workflow"],
  ],
  issue: [
    ["IssueService/GetIssue", "The issue, its comments and linked pull requests"],
    ["IssueService/ListIssues", "Find related issues in the repository"],
    ["ChangeService/ListChanges", "Find the pull requests linked to this issue"],
    ["OrgService/ListMembers", "Find people in the organisation"],
    ["AgentService/ListAgents", "Find agents for this repository"],
    ["IssueService/CommentIssue", "Comment on it"],
    ["IssueService/AssignIssue", "Take it, or give it to someone"],
    ["IssueService/SetIssueState", "Close it", { state: "closed" }],
    ["IssueService/SetIssueState", "Reopen it", { state: "open" }],
  ],
  "issue-board": [
    ["ProjectService/GetProject", "Find the project linked to the repository"],
    ["IssueService/ListIssues", "Read the issues on the board"],
    ["ProjectService/MoveItem", "Change an issue's board column"],
  ],
};

export const PAGE_IDS = Object.keys(PAGES) as PageId[];

/** The tools on one screen, in reading order: reads, then writes, then what needs a person. */
export function pageTools(page: PageId): PageTool[] {
  const rank: Record<ToolKind, number> = { read: 0, write: 1, "needs a person": 2 };
  return PAGES[page]
    .map(([op, what, request]) => toolFor(op, what, request))
    .map((t, i) => ({ t, i }))
    .sort((a, b) => rank[a.t.kind] - rank[b.t.kind] || a.i - b.i)
    .map((x) => x.t);
}
