/**
 * Posts and following (posts.proto, users.proto, agents.proto): posts of their own and a change
 * request's post, their media, the Following feed, repo follows, adoptions, and the agent feed trigger with its runs.
 *
 * Every post RPC is one bee.v1 call, so the view types pass through. The one composed method
 * is `AgentService/ListAdoptRuns`: the run list plus the post each run evaluated.
 */
import { AgentService, type AgentRunJson } from "../../gen/bee/v1/agents_pb";
import { PostService } from "../../gen/bee/v1/posts_pb";
import { RepoService } from "../../gen/bee/v1/forge_pb";
import { UserService } from "../../gen/bee/v1/users_pb";
import type { Rpc } from "../rpc";
import { BeeError, type CallOptions } from "../transport";
import type * as T from "../types";
import { clean, list, mapLimit, pass, str, wire, type PartialOps, type UnaryOp } from "./shared";

const OPENED = /^opened (issue|change request) #(\d+): (.*)$/s;
const NOT_RELEVANT = /^not relevant:\s*(.*)$/s;

/** How a finished feed-trigger run is told apart; the run's summary is the record (see `T.AdoptRun`). */
export function adoptOutcome(run: AgentRunJson): Pick<T.AdoptRun, "outcome" | "reason" | "opened"> {
  const state = run.state;
  if (state === "AGENT_RUN_STATE_FAILED" || state === "AGENT_RUN_STATE_CANCELLED") return { outcome: "failed", reason: str(run.failureReason) || str(run.summary) };
  if (state !== "AGENT_RUN_STATE_SUCCEEDED") return { outcome: "running", reason: "" };
  const summary = str(run.summary).trim();
  const opened = OPENED.exec(summary);
  if (opened) {
    const kind = opened[1] === "issue" ? "issue" : "change";
    return { outcome: kind === "issue" ? "opened_issue" : "opened_change", reason: "", opened: { kind, number: opened[2] ?? "0", title: opened[3] ?? "", repo: run.repo as T.PostRepo | undefined } };
  }
  return { outcome: "not_relevant", reason: NOT_RELEVANT.exec(summary)?.[1] ?? summary };
}

/** The post a run evaluated, or undefined once the agent's caller can no longer read it. */
async function postRef(rpc: Rpc, postId: string, opts?: CallOptions): Promise<T.PostRef | undefined> {
  try {
    const p = await rpc.call(PostService.method.getPost, { postId }, opts);
    return { postId: str(p.id), repo: p.repo as T.PostRepo | undefined, changeNumber: str(p.changeNumber) || "0", changeTitle: str(p.changeTitle) };
  } catch (err) {
    if (err instanceof BeeError && (err.code === "not_found" || err.code === "permission_denied")) return undefined;
    throw err;
  }
}

function runsReq(r: T.ListAdoptRunsRequest) {
  return { repo: r.repo, agentPrincipalId: r.agentPrincipalId, taskKind: "AGENT_TASK_KIND_ADOPT_EVALUATE" as const, page: { pageSize: Math.max(1, Math.min(100, r.limit ?? 20)) } };
}

async function adoptRuns(rpc: Rpc, r: T.ListAdoptRunsRequest, opts?: CallOptions): Promise<T.ListAdoptRunsResponse> {
  const res = await rpc.call(AgentService.method.listAgentRuns, clean(runsReq(r)), opts);
  const runs = list(res.runs).filter((x) => x.task?.kind === "AGENT_TASK_KIND_ADOPT_EVALUATE");
  const ids = [...new Set(runs.map((x) => str(x.task?.postId)).filter(Boolean))];
  const posts = new Map((await mapLimit(ids, 6, async (id) => [id, await postRef(rpc, id, opts)] as const)).filter(([, v]) => v));
  return {
    runs: runs
      .map((x): T.AdoptRun => {
        const postId = str(x.task?.postId);
        return {
          id: str(x.id),
          agent: x.agent as T.Principal | undefined,
          postId,
          post: posts.get(postId),
          ...adoptOutcome(x),
          startedAt: x.startedAt ?? x.createdAt,
          finishedAt: x.finishedAt,
          costUsdMicros: str(x.cost?.usdMicros) || "0",
        };
      })
      .sort((a, b) => Date.parse(b.startedAt ?? "") - Date.parse(a.startedAt ?? "") || a.id.localeCompare(b.id)),
  };
}

/**
 * The real server names a post's repo by id only (`Post.repo`), while every page builds permalinks
 * and media URLs from its full name. `named` runs a post-returning op and fills in the names, one
 * `GetRepo` per distinct id; a repo the caller cannot look up keeps its id and is left alone.
 */
function named<Req, Res>(op: UnaryOp<Req, Res>, posts: (res: Res) => T.Post[]): UnaryOp<Req, Res> {
  return {
    wire: op.wire,
    run: async (rpc, r, opts) => {
      const res = await op.run(rpc, r, opts);
      const todo = posts(res).filter((p) => p.repo?.id && !p.repo.fullName);
      const names = new Map<string, string>();
      await Promise.all(
        [...new Set(todo.map((p) => p.repo?.id ?? ""))].map(async (id) => {
          try {
            names.set(id, str((await rpc.call(RepoService.method.getRepo, { repo: { id } }, opts)).fullName));
          } catch (err) {
            if (!(err instanceof BeeError)) throw err;
          }
        }),
      );
      for (const p of todo) {
        const fullName = names.get(p.repo?.id ?? "");
        if (fullName && p.repo) p.repo = { ...p.repo, fullName };
      }
      return res;
    },
  };
}

export const postOps = {
  PostService: {
    CreatePost: named(pass(PostService.method.createPost), (r) => [r]),
    SetChangePost: named(pass(PostService.method.setChangePost), (r) => [r]),
    ClearChangePost: pass(PostService.method.clearChangePost),
    GetChangePost: named(pass(PostService.method.getChangePost), (r) => [r]),
    BeginPostMediaUpload: pass(PostService.method.beginPostMediaUpload),
    FinalizePostMedia: pass(PostService.method.finalizePostMedia),
    ApprovePost: named(pass(PostService.method.approvePost), (r) => [r]),
    WithdrawPost: named(pass(PostService.method.withdrawPost), (r) => [r]),
    GetPost: named(pass(PostService.method.getPost), (r) => [r]),
    ListRepoPosts: named(pass(PostService.method.listRepoPosts), (r) => r.posts),
    LikePost: pass(PostService.method.likePost),
    UnlikePost: pass(PostService.method.unlikePost),
    RepostPost: pass(PostService.method.repostPost),
    UndoRepost: pass(PostService.method.undoRepost),
    ListFollowingFeed: named(pass(PostService.method.listFollowingFeed), (r) => r.items.flatMap((i) => (i.post ? [i.post] : []))),
    ListPostAdoptions: pass(PostService.method.listPostAdoptions),
  },
  UserService: {
    FollowRepo: pass(UserService.method.followRepo),
    UnfollowRepo: pass(UserService.method.unfollowRepo),
    IsFollowingRepo: pass(UserService.method.isFollowingRepo),
    ListFollowedRepos: pass(UserService.method.listFollowedRepos),
    ListRepoFollowers: pass(UserService.method.listRepoFollowers),
  },
  AgentService: {
    GetAgentFeedTrigger: pass(AgentService.method.getAgentFeedTrigger),
    SetAgentFeedTrigger: pass(AgentService.method.setAgentFeedTrigger),
    ListAgentFeedTriggers: pass(AgentService.method.listAgentFeedTriggers),
    ListAdoptRuns: {
      wire: (r) => wire(AgentService.method.listAgentRuns, runsReq(r)),
      run: (rpc, r, opts) => adoptRuns(rpc, r, opts),
    } satisfies UnaryOp<T.ListAdoptRunsRequest, T.ListAdoptRunsResponse>,
  },
} satisfies PartialOps<"PostService" | "UserService" | "AgentService">;
