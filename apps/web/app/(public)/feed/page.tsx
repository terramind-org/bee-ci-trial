import type { Metadata } from "next";
import Link from "next/link";
import { redirect } from "next/navigation";
import { FollowRepoButton } from "@/components/follow-repo-button";
import { PageMenu } from "@/components/menu";
import { PostList, PostPager, type PostListItem } from "@/components/post-list";
import { EmptyState, PageHeader } from "@/components/ui";
import { serverClient, sessionState } from "@/lib/api/server";
import { BeeError } from "@/lib/api/transport";
import type { TrendingRepo } from "@/lib/api/types";
import { guard, param, type SearchParams } from "@/lib/page";
import { repoHref } from "@/lib/routes";

export const metadata: Metadata = { title: "Feed" };

const PAGE = 20;
const SUGGESTIONS = 5;

/**
 * `/feed`: the Following feed. Posts from the repos and people the reader follows, and the posts
 * those people reposted, newest first (PostService.ListFollowingFeed). Signed in only.
 */
export default async function FeedPage({ searchParams }: { searchParams: SearchParams }) {
  const sp = await searchParams;
  const { me } = await sessionState();
  if (!me) redirect("/signin?next=/feed");
  const cursor = param(sp, "cursor");
  return guard(async () => {
    const api = await serverClient();
    const req = { page: { pageSize: PAGE, ...(cursor ? { pageToken: cursor } : {}) } };
    let res;
    try {
      res = await api.post.listFollowingFeed(req);
    } catch (err) {
      // A cursor from an old link the server no longer reads: start again from the newest.
      if (cursor && err instanceof BeeError && /bad_page_token|BAD_PAGE_TOKEN/.test(err.reason)) redirect("/feed");
      throw err;
    }
    const items: PostListItem[] = res.items.flatMap((it) => (it.post ? [{ post: it.post, repostedBy: it.repostedBy }] : []));
    const next = res.page?.nextPageToken ?? "";
    const op = { service: "PostService" as const, method: "ListFollowingFeed", request: req, label: "Read your Following feed" };
    return (
      <div className="max-w-[640px]">
        <PageHeader title="Feed" lede="What the repos and people you follow have merged and said about it." actions={<PageMenu op={op} />} />
        {items.length ? (
          <>
            <PostList items={items} signedIn label="Posts from what you follow" />
            <PostPager older={next ? "/feed?cursor=" + encodeURIComponent(next) : null} newest={cursor ? "/feed" : null} />
          </>
        ) : cursor ? (
          <EmptyState
            icon="feed"
            title="No older posts"
            action={
              <Link href="/feed" className="link">
                Back to the newest
              </Link>
            }
          >
            You have reached the start of your feed.
          </EmptyState>
        ) : (
          <Suggestions />
        )}
      </div>
    );
  });
}

/** An empty feed suggests five trending repos to follow, each with its Follow button. */
async function Suggestions() {
  const api = await serverClient();
  let repos: TrendingRepo[] = [];
  try {
    repos = (await api.search.listTrendingRepos({ window: "TRENDING_WINDOW_WEEKLY", page: { pageSize: 20 } })).repos;
  } catch (err) {
    if (!(err instanceof BeeError)) throw err;
  }
  const names = repos.map((r) => r.repo?.fullName ?? "").filter(Boolean);
  const following = await Promise.all(names.map((fullName) => api.user.isFollowingRepo({ repo: { fullName } }).then((r) => r.following, () => false)));
  const picks = repos.filter((r, i) => r.repo?.fullName && !following[i]).slice(0, SUGGESTIONS);
  return (
    <section aria-labelledby="feed-empty-title">
      <h2 id="feed-empty-title" className="text-xl font-medium text-ink">
        Nothing here yet
      </h2>
      <p className="mt-2 text-md text-ink-2">
        Follow a repository and its posts show up here when its change requests merge. These are trending this week.
      </p>
      {picks.length ? (
        <ul className="mt-6 flex flex-col" data-testid="feed-suggestions">
          {picks.map((t) => {
            const r = t.repo!;
            return (
              <li key={r.fullName} className="flex items-start gap-4 border-t border-line/70 py-4 first:border-t-0">
                <div className="min-w-0 flex-1">
                  <Link href={repoHref(r.fullName)} className="text-md font-medium text-ink hover:underline">
                    {r.fullName}
                  </Link>
                  {r.description ? <p className="mt-0.5 text-base text-ink-2">{r.description}</p> : null}
                </div>
                <FollowRepoButton target={r.fullName} following={false} />
              </li>
            );
          })}
        </ul>
      ) : (
        <p className="mt-6 text-base text-ink-2">
          <Link href="/explore" className="link">
            Explore repositories
          </Link>{" "}
          to find some to follow.
        </p>
      )}
    </section>
  );
}
