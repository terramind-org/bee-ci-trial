import type { Metadata } from "next";
import { PageFacts } from "@/components/bee/page-facts";
import { PageNav, RepoEntries } from "@/components/discover/repo-entry";
import { PageMenu } from "@/components/menu";
import { hitOfRepo, visibilityWord } from "@/components/repo/query";
import { EmptyState, PageHeader } from "@/components/ui";
import { scopeFromParams, serverClient, type RepoParams } from "@/lib/api/server";
import { plural } from "@/lib/format";
import { guard, param, type SearchParams } from "@/lib/page";
import { repoHref } from "@/lib/routes";
import { toolFor } from "@/lib/tools";
import { requestMode } from "../../../../../request-mode";

export const metadata: Metadata = { title: "Forks" };

export default async function ForksPage({ params, searchParams }: { params: Promise<RepoParams>; searchParams: SearchParams }) {
  return guard(async () => {
    const [{ ref, fullName }, sp, { mode }, api] = await Promise.all([scopeFromParams(params), searchParams, requestMode(), serverClient()]);
    const after = param(sp, "after");
    const request = { repo: ref, page: { pageSize: 25, pageToken: after } };
    const { forks, page } = await api.repo.listForks(request);
    const here = repoHref(fullName, "insights/forks");
    const next = page?.nextPageToken ? here + "?after=" + encodeURIComponent(page.nextPageToken) : undefined;
    const nav = <PageNav label="Fork pages" first={after ? here : undefined} next={next} />;
    const empty = "No forks are visible to you.";
    if (mode === "bee") {
      return (
        <>
          <PageFacts page="forks" title={fullName + " · forks"} tools={[toolFor("RepoService/ListForks", "List the forks you can read")]}
            groups={[{ title: "forks", lines: forks.map((r) => [
              r.fullName, visibilityWord(r.visibility), r.description, r.primaryLanguage, plural(Number(r.stars), "star"),
              r.pushedAt, r.topics.join(", "), repoHref(r.fullName),
            ].filter(Boolean).join(" · ")), empty }]} />
          {nav}
        </>
      );
    }
    return (
      <>
        <PageHeader title="Forks" actions={<PageMenu op={{ service: "RepoService", method: "ListForks", request, label: "List forks" }} />} />
        <RepoEntries hits={forks.map(hitOfRepo)} empty={<EmptyState title={empty} />} />
        {nav}
      </>
    );
  });
}
