import { notFound } from "next/navigation";
import { Suspense, type ReactNode } from "react";
import { DegradedStatus } from "@/components/degraded-status";
import { RepoHeader, RepoInfoBand, RepoInfoBandSkeleton } from "@/components/repo-header";
import { canWriteRepo, loadRepo, repoStarState, scopeFromParams, sessionState } from "@/lib/api/server";
import { isRepoFullName } from "@/lib/routes";
import type { Repo } from "@/lib/api/types";
import { Frame } from "../../../frame";
import { RepoLoading } from "../../../layout-skeletons";
import { RepoFollow } from "./follow";

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
  const [canWrite, star] = await Promise.all([canWriteRepo(fullName), repoStarState(fullName, signedIn)]);
  return (
    <Frame repo={{ fullName, canWrite }} org={owner}>
      <RepoHeader
        fullName={fullName}
        description={repo.description}
        visibility={repo.visibility}
        canWrite={canWrite}
        signedIn={signedIn}
        star={star}
        actions={
          <Suspense fallback={<span aria-hidden="true" className="inline-block h-8 w-[104px] animate-pulse rounded-lg bg-sunken" />}>
            <RepoFollow fullName={fullName} signedIn={signedIn} />
          </Suspense>
        }
        counts={{ changes: Number(repo.openChanges ?? 0), issues: Number(repo.openIssues ?? 0) }}
        band={
          <Suspense fallback={<RepoInfoBandSkeleton />}>
            <RepoInfoBand fullName={fullName} defaultRef={repo.defaultRef} />
          </Suspense>
        }
      />
      <DegradedStatus fullName={fullName} defaultRef={repo.defaultRef} />
      {children}
    </Frame>
  );
}
