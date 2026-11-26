import { notFound } from "next/navigation";
import { Suspense, type ReactNode } from "react";
import { DegradedStatus } from "@/components/degraded-status";
import { RepoHeader, RepoHeaderFacts, RepoInfoBand, RepoInfoBandSkeleton } from "@/components/repo-header";
import { forkStatus } from "@/components/repo-header-data";
import { canWriteRepo, serverClient, loadRepo, repoStarState, scopeFromParams, sessionState } from "@/lib/api/server";
import { isRepoFullName } from "@/lib/routes";
import type { Repo } from "@/lib/api/types";
import { Frame } from "../../../frame";
import { RepoLoading } from "../../../layout-skeletons";
import { requestMode } from "../../../request-mode";
import { RepoFollow, repoFollowState } from "./follow";

/**
 * Every `/{owner}/{repo}/…` page. The repo in the URL is the scope; a repo the caller cannot read
 * (including any non-public repo when signed out) is a plain 404, so existence never leaks.
 */
export default async function RepoLayout({ children, params }: { children: ReactNode; params: Promise<{ owner: string; repo: string }> }) {
  const { fullName } = await scopeFromParams(params);
  if (!isRepoFullName(fullName)) notFound();
  const [repo, { me }] = await Promise.all([loadRepo(fullName), sessionState()]);
  if (!repo || (!me && repo.visibility !== "VISIBILITY_PUBLIC")) notFound();
  // Not found is decided before anything streams, so it answers with a real 404.
  return (
    <Suspense fallback={<RepoLoading />}>
      <RepoFrame fullName={fullName} repo={repo} signedIn={!!me}>
        {children}
      </RepoFrame>
    </Suspense>
  );
}

async function RepoFrame({ fullName, repo, signedIn, children }: { fullName: string; repo: Repo; signedIn: boolean; children: ReactNode }) {
  const owner = fullName.split("/")[0] as string;
  // Every repository page mounts the same header and uses its account and fork state.
  const api = await serverClient();
  const [canWrite, star, upstream, { mode }] = await Promise.all([
    canWriteRepo(fullName), repoStarState(fullName, signedIn), forkStatus(api, repo), requestMode(),
  ]);
  const agent = mode === "bee";
  const writable = canWrite && !repo.archived;
  const counts = { changes: Number(repo.openChanges ?? 0), issues: Number(repo.openIssues ?? 0) };
  const forks = Number(repo.forks ?? 0);
  return (
    <Frame repo={{ fullName, canWrite }} org={owner}>
      <RepoHeader
        fullName={fullName}
        description={repo.description}
        visibility={repo.visibility}
        canWrite={canWrite}
        canSync={writable}
        signedIn={signedIn}
        star={star}
        forks={forks}
        upstream={upstream}
        agent={agent}
        actions={
          <Suspense fallback={<span aria-hidden="true" className="inline-block h-[34px] w-[104px] rounded-full bg-sunken" />}>
            <RepoFollow fullName={fullName} signedIn={signedIn} />
          </Suspense>
        }
        counts={counts}
        band={
          <Suspense fallback={<RepoInfoBandSkeleton />}>
            <RepoInfoBand fullName={fullName} defaultRef={repo.defaultRef} />
          </Suspense>
        }
      />
      <DegradedStatus fullName={fullName} defaultRef={repo.defaultRef} />
      {children}
      {agent ? (
        <Suspense>
          <RepoFacts fullName={fullName} repo={repo} signedIn={signedIn} star={star} forks={forks} counts={counts} upstream={upstream} canSync={writable && upstream?.behind === true} />
        </Suspense>
      ) : null}
    </Frame>
  );
}

async function RepoFacts({ repo, ...props }: Omit<Parameters<typeof RepoHeaderFacts>[0], "visibility" | "description"> & { repo: Repo }) {
  const follow = await repoFollowState(props.fullName, props.signedIn);
  return <RepoHeaderFacts {...props} description={repo.description} visibility={repo.visibility} following={follow.following} followers={follow.count} defaultRef={repo.defaultRef} />;
}
