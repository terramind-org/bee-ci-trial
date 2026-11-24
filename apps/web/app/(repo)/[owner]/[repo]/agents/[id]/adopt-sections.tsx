/**
 * The adopt half of an agent's page: the repos it follows, the feed trigger that lets it adopt
 * their upgrades, and what its recent adopt runs came to. Written as a work log like the sections
 * around it: sentences, no boxes. `page.tsx` mounts it between "Working now" and "Access".
 *
 * Triggers and runs are visible to members of the org only; for anyone else the page shows what the
 * agent follows and says the rest is for members.
 */
import Link from "next/link";
import { Time } from "@/components/time";
import { Panel } from "@/components/ui";
import { canWriteRepo, serverClient } from "@/lib/api/server";
import { BeeError } from "@/lib/api/transport";
import type { AdoptRun, AgentFeedTrigger, Repo } from "@/lib/api/types";
import { usd } from "@/lib/format";
import { repoHref } from "@/lib/routes";
import { FeedTriggerForm } from "./feed-trigger-form";

const RUNS = 10;

async function orNull<T>(p: Promise<T>, ...codes: string[]): Promise<T | null> {
  try {
    return await p;
  } catch (err) {
    if (err instanceof BeeError && codes.includes(err.code)) return null;
    throw err;
  }
}

export async function AdoptSections({ repo, agentId, login }: { repo: string; agentId: string; /** The agent's login: what `ListFollowedRepos` is keyed by. */ login: string }) {
  const api = await serverClient();
  const ref = { fullName: repo };
  const [followed, trigger, runs, canEdit] = await Promise.all([
    orNull(api.user.listFollowedRepos({ user: login, page: { pageSize: 100 } }), "not_found", "permission_denied"),
    orNull(api.agent.getAgentFeedTrigger({ repo: ref, agent: agentId }), "not_found", "permission_denied"),
    orNull(api.agent.listAdoptRuns({ repo: ref, agentPrincipalId: agentId, limit: RUNS }), "not_found", "permission_denied"),
    canWriteRepo(repo),
  ]);
  // A member who has no trigger yet still gets the form; a non-member's lookup was refused.
  const visible = trigger !== null || runs !== null;
  const own = !!trigger || canEdit;
  return (
    <>
      <Panel title="Follows" id="agent-follows">
        <FollowList repos={followed?.repos ?? []} login={login} />
      </Panel>
      <Panel title="Feed trigger" id="agent-feed-trigger">
        {visible || canEdit ? (
          <FeedTrigger repo={repo} agentId={agentId} login={login} trigger={trigger ?? undefined} canEdit={canEdit} />
        ) : (
          <p className="text-md text-ink-2">Feed triggers are visible to members of {repo.split("/")[0]}.</p>
        )}
      </Panel>
      {own || runs?.runs.length ? (
        <Panel title="Recent adopt runs" id="agent-adopt-runs">
          <AdoptRuns repo={repo} runs={runs?.runs ?? []} />
        </Panel>
      ) : null}
    </>
  );
}

function FollowList({ repos, login }: { repos: Repo[]; login: string }) {
  if (!repos.length)
    return (
      <p className="text-md text-ink-2" data-testid="agent-follows-empty">
        {login} does not follow any repo yet. Following one puts its published posts in {login}&rsquo;s feed.
      </p>
    );
  return (
    <ul className="flex flex-col gap-2" aria-label={"Repos " + login + " follows"}>
      {repos.map((r) => (
        <li key={r.fullName} className="text-md text-ink-2">
          <Link href={repoHref(r.fullName)} className="font-medium text-ink hover:underline">
            {r.fullName}
          </Link>
          {r.description ? <span className="text-ink-3"> · {r.description}</span> : null}
        </li>
      ))}
    </ul>
  );
}

/** The trigger as a sentence for everyone who may read it, and the form for those who may change it. */
function FeedTrigger({ repo, agentId, login, trigger, canEdit }: { repo: string; agentId: string; login: string; trigger?: AgentFeedTrigger; canEdit: boolean }) {
  const sources = (trigger?.sources ?? []).map((s) => s.fullName ?? "").filter(Boolean);
  const seconds = Number(trigger?.checkIntervalSeconds ?? 0);
  return (
    <div className="flex flex-col gap-5">
      <p className="text-md text-ink" data-testid="feed-trigger-summary">
        {!trigger
          ? login + " has no feed trigger in " + repo + ", so it does not react to posts."
          : trigger.enabled
            ? login + " evaluates new posts from " + (sources.length ? sources.join(", ") : "no repo yet") + " and may open an issue or change request in " + repo + "."
            : "The feed trigger is off. " + login + " does not react to posts."}
      </p>
      {trigger ? (
        <p className="text-sm text-ink-3">
          Up to {trigger.maxRunsPerDay || "no"} runs and {trigger.dailyUsdBudget ? usd(Math.round(trigger.dailyUsdBudget * 1_000_000)) : "no"} a day. Today: {trigger.runsToday} {trigger.runsToday === 1 ? "run" : "runs"}, {usd(Math.round(trigger.spentUsdToday * 1_000_000))} spent
          {seconds ? ". Posts within " + seconds + " seconds of each other are evaluated together." : "."}
        </p>
      ) : null}
      {canEdit ? (
        <FeedTriggerForm
          repo={repo}
          agentId={agentId}
          initial={{ enabled: trigger?.enabled ?? false, sources, dailyUsdBudget: trigger?.dailyUsdBudget ?? 5, maxRunsPerDay: trigger?.maxRunsPerDay ?? 10 }}
        />
      ) : (
        <p className="text-sm text-ink-3">Only maintainers of {repo} can change this.</p>
      )}
    </div>
  );
}

async function AdoptRuns({ repo, runs }: { repo: string; runs: AdoptRun[] }) {
  if (!runs.length) return <p className="text-md text-ink-2">No adopt runs yet. A run starts when a followed repo publishes a post.</p>;
  const api = await serverClient();
  // A change request opens at an id; the run only reports its number.
  const changeIds = new Map<string, string>();
  if (runs.some((r) => r.opened?.kind === "change")) {
    const all = await orNull(api.change.listChanges({ repo: { fullName: repo } }), "not_found", "permission_denied");
    for (const c of all?.changes ?? []) changeIds.set(String(c.number), c.id);
  }
  return (
    <ol className="flex flex-col gap-5" aria-label="Adopt runs, newest first">
      {runs.map((r) => {
        const o = r.opened;
        const post = r.post;
        const postRepo = post?.repo?.fullName;
        return (
          <li key={r.id} data-testid="adopt-run" data-outcome={r.outcome}>
            <p className="text-md text-ink">
              {r.outcome === "not_relevant" ? (
                <>Not relevant: {r.reason || "no reason given"}</>
              ) : r.outcome === "failed" ? (
                <>Failed{r.reason ? ": " + r.reason : ""}</>
              ) : r.outcome === "running" ? (
                <>Evaluating&hellip;</>
              ) : o ? (
                <>
                  Opened {o.kind === "issue" ? "issue" : "change request"}{" "}
                  <Link href={o.kind === "issue" ? repoHref(repo, "work/" + o.number) : changeIds.has(String(o.number)) ? repoHref(repo, "changes/" + changeIds.get(String(o.number))) : repoHref(repo, "changes")} className="font-medium hover:underline">
                    #{o.number}
                  </Link>
                  {o.title ? ": " + o.title : ""}
                </>
              ) : null}
            </p>
            <p className="mt-0.5 text-sm text-ink-3">
              {postRepo && post ? (
                <>
                  Evaluated{" "}
                  <Link href={repoHref(postRepo, "posts/" + encodeURIComponent(post.postId))} className="hover:underline">
                    {postRepo}
                    {post.changeTitle ? ": " + post.changeTitle : ""}
                  </Link>
                  {" · "}
                </>
              ) : null}
              <Time iso={r.startedAt} />
              {Number(r.costUsdMicros) ? " · " + usd(r.costUsdMicros) : ""}
            </p>
          </li>
        );
      })}
    </ol>
  );
}
