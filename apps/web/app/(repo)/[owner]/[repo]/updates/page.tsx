import type { Metadata } from "next";
import { redirect } from "next/navigation";
import { requestMode } from "@/app/request-mode";
import { PageFacts } from "@/components/bee/page-facts";
import { ButtonLink } from "@/components/kit";
import { PageMenu } from "@/components/menu";
import { postOwner } from "@/components/posts/post-data";
import { postFacts } from "@/components/posts/post-facts";
import { PostList, PostPager } from "@/components/posts/post-list";
import { PageHeader } from "@/components/ui";
import { canWriteRepo, type RepoParams, scopeFromParams, serverClient, sessionState } from "@/lib/api/server";
import { BeeError } from "@/lib/api/transport";
import { guard, param, type SearchParams } from "@/lib/page";
import { repoHref } from "@/lib/routes";
import { pageTools } from "@/lib/tools";

export const metadata: Metadata = { title: "Updates" };

const PAGE = 20;

export default async function UpdatesPage({ params, searchParams }: { params: Promise<RepoParams>; searchParams: SearchParams }) {
  const cursor = param(await searchParams, "cursor");
  return guard(async () => {
    const { fullName, ref } = await scopeFromParams(params);
    const [{ me }, api, { mode }] = await Promise.all([sessionState(), serverClient(), requestMode()]);
    const here = repoHref(fullName, "updates");
    const req = { repo: ref, page: { pageSize: PAGE, pageToken: cursor || undefined } };
    const res = await api.post.listRepoPosts(req).catch((err: unknown) => {
      if (cursor && err instanceof BeeError && /bad_page_token/i.test(err.reason)) redirect(here);
      throw err;
    });
    const posts = res.posts.filter((post) => post.state === "POST_STATE_PUBLISHED");
    const items = await Promise.all(posts.map(async (post) => ({ post, ownerName: await postOwner(post, me) })));
    const next = res.page?.nextPageToken;
    const empty = cursor ? "There are no older posts." : "This repository has no published posts.";
    const pager = <PostPager older={next ? here + "?cursor=" + encodeURIComponent(next) : null} newest={cursor ? here : null} />;
    if (mode === "bee") return (
      <>
        <PageFacts page="updates" title="Updates" groups={[
          { title: "scope", facts: [{ label: "repository", value: fullName }] },
          ...(items.length ? items.map(postFacts) : [{ title: "posts", empty }]),
        ]} tools={pageTools("updates")} />
        {pager}
      </>
    );
    const canPublish = me?.kind === "PRINCIPAL_KIND_HUMAN" && await canWriteRepo(fullName);
    return (
      <div className="mx-auto w-full max-w-[640px]">
        <PageHeader title="Updates" actions={<div className="flex items-center gap-2">
          {canPublish ? <ButtonLink size="sm" href={repoHref(fullName, "posts/new")}>Write a post</ButtonLink> : null}
          <PageMenu op={{ service: "PostService", method: "ListRepoPosts", request: req, label: "Read this repository's posts" }} />
        </div>} />
        {items.length ? <PostList items={items} signedIn={!!me} label={"Posts in " + fullName} /> : <p className="text-ink-2">{empty}</p>}
        {pager}
      </div>
    );
  });
}
