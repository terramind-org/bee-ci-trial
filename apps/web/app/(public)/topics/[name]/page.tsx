import type { Metadata } from "next";
import Link from "next/link";
import { notFound } from "next/navigation";
import { CopyAs, PageOp } from "@/components/copy-as";
import { Markdown } from "@/components/markdown";
import { Pager, href } from "@/components/discover/pager";
import { RepoEntries } from "@/components/discover/repo-entry";
import { Segments } from "@/components/discover/window-switch";
import { parseChoice } from "@/components/discover/params";
import { Badge, EmptyState, PageHeader } from "@/components/ui";
import { serverClient } from "@/lib/api/server";
import { guard, param, type SearchParams } from "@/lib/page";

const SORTS = ["stars", "updated", "forks"] as const;
const SORT_LABEL = { stars: "Most stars", updated: "Recently updated", forks: "Most forks" } as const;

async function loadTopic(name: string) {
  return (await serverClient()).search.getTopic({ name, topRepoCount: 1 });
}

export async function generateMetadata({ params }: { params: Promise<{ name: string }> }): Promise<Metadata> {
  const name = decodeURIComponent((await params).name);
  try {
    const t = await loadTopic(name);
    return { title: (t.displayName || t.name) + " · Topics", description: t.shortDescription || undefined };
  } catch {
    return { title: "Topic" };
  }
}

export default async function TopicPage({ params, searchParams }: { params: Promise<{ name: string }>; searchParams: SearchParams }) {
  const name = decodeURIComponent((await params).name);
  const sp = await searchParams;
  const sort = parseChoice(param(sp, "sort"), SORTS, "stars");
  const after = param(sp, "after") || undefined;
  const here = (over: Record<string, string | undefined>) => href("/topics/" + encodeURIComponent(name), { sort: sort === "stars" ? undefined : sort, ...over });

  return guard(async () => {
    const api = await serverClient();
    const topicReq = { name, topRepoCount: 1 };
    const topic = await api.search.getTopic(topicReq); // not_found → the 404 page
    if (!topic.name) notFound();
    const reposReq = { query: "topic:" + topic.name, sort, page: { pageSize: 24, ...(after ? { pageToken: after } : {}) } };
    const repos = await api.search.searchRepos(reposReq);
    const op = { service: "SearchService" as const, method: "SearchRepos", request: reposReq, label: "List repositories tagged " + topic.name };
    const count = Number(topic.repoCount);
    const next = repos.page?.nextPageToken ? here({ after: repos.page.nextPageToken }) : null;
    return (
      <>
        <PageOp op={op} />
        <PageHeader
          eyebrow={
            <Link href="/explore" className="link">
              Explore
            </Link>
          }
          title={topic.displayName || topic.name}
          meta={
            <>
              <span className="text-ink-3">{topic.name}</span>
              <span>
                <span className="num">{count}</span> {count === 1 ? "repository" : "repositories"}
              </span>
              {topic.curated ? <Badge tone="outline">Curated</Badge> : null}
            </>
          }
          actions={<CopyAs op={op} />}
        />
        {topic.description || topic.shortDescription || topic.related.length ? (
          <div className="mb-10 flex max-w-2xl flex-col gap-4">
            {topic.description ? <Markdown source={topic.description} /> : topic.shortDescription ? <p className="text-lg text-ink-2">{topic.shortDescription}</p> : null}
            {topic.related.length ? (
              <nav aria-label="Related topics" className="flex flex-wrap items-center gap-x-5 gap-y-1">
                <span className="text-base text-ink-3">Related</span>
                {topic.related.map((r) => (
                  <Link key={r} href={"/topics/" + encodeURIComponent(r)} className="inline-flex h-7 items-center text-base text-accent-ink hover:underline">
                    {r}
                  </Link>
                ))}
              </nav>
            ) : null}
          </div>
        ) : null}
        <div className="mb-2">
          <Segments label="Sort repositories" current={sort} segments={SORTS.map((s) => ({ value: s, label: SORT_LABEL[s], href: here({ sort: s === "stars" ? undefined : s, after: undefined }) }))} />
        </div>
        <RepoEntries hits={repos.hits} empty={<EmptyState title={"No repositories are tagged " + topic.name + " yet"} icon="repo" />} />
        <Pager nextHref={next} />
      </>
    );
  });
}
