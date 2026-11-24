import type { Metadata } from "next";
import { redirect } from "next/navigation";
import { PageMenu } from "@/components/menu";
import { PostList, PostPager } from "@/components/post-list";
import { ButtonLink, EmptyState, PageHeader } from "@/components/ui";
import { type RepoParams, scopeFromParams, serverClient, sessionState } from "@/lib/api/server";
import { BeeError } from "@/lib/api/transport";
import { guard, param, type SearchParams } from "@/lib/page";
import { repoHref } from "@/lib/routes";

export const metadata: Metadata = { title: "Updates" };

const PAGE = 20;

/** `/{owner}/{repo}/updates`: the repo's published posts, newest first (PostService.ListRepoPosts). */
export default async function UpdatesPage({ params, searchParams }: { params: Promise<RepoParams>; searchParams: SearchParams }) {
  const sp = await searchParams;
  const cursor = param(sp, "cursor");
  return guard(async () => {
    const { fullName, ref } = await scopeFromParams(params);
    const [{ me }, api] = await Promise.all([sessionState(), serverClient()]);
    const req = { repo: ref, page: { pageSize: PAGE, ...(cursor ? { pageToken: cursor } : {}) } };
    let res;
    try {
      res = await api.post.listRepoPosts(req);
    } catch (err) {
      if (cursor && err instanceof BeeError && /bad_page_token|BAD_PAGE_TOKEN/.test(err.reason)) redirect(repoHref(fullName, "updates"));
      throw err;
    }
    // The list is the repo's public record: only what has been published (and not withdrawn).
    const posts = res.posts.filter((p) => p.state === "POST_STATE_PUBLISHED");
    const next = res.page?.nextPageToken ?? "";
    const here = repoHref(fullName, "updates");
    const op = { service: "PostService" as const, method: "ListRepoPosts", request: req, label: "List the posts of " + fullName };
    return (
      <div className="max-w-[640px]">
        <PageHeader title="Updates" lede={"What merged in " + fullName + ", in the words of the people and agents who did it."} actions={<PageMenu op={op} />} />
        {posts.length ? (
          <>
            <PostList items={posts.map((post) => ({ post }))} signedIn={!!me} label={"Posts in " + fullName} />
            <PostPager older={next ? here + "?cursor=" + encodeURIComponent(next) : null} newest={cursor ? here : null} />
          </>
        ) : (
          <EmptyState icon="updates" title={cursor ? "No older posts" : "No posts yet"} action={<ButtonLink href={cursor ? here : repoHref(fullName, "changes")}>{cursor ? "Back to the newest" : "Open change requests"}</ButtonLink>}>
            {cursor ? "You have reached the first post." : "A post is written on a change request and goes out here when the change merges."}
          </EmptyState>
        )}
      </div>
    );
  });
}
