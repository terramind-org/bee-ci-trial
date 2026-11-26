import type { Metadata } from "next";
import { redirect } from "next/navigation";
import { requestMode } from "@/app/request-mode";
import { PageFacts } from "@/components/bee/page-facts";
import { ButtonLink, TabLinks } from "@/components/kit";
import { PageMenu } from "@/components/menu";
import { postOwner } from "@/components/posts/post-data";
import { postFacts } from "@/components/posts/post-facts";
import { PostList, PostPager, type PostListItem } from "@/components/posts/post-list";
import { PageHeader } from "@/components/ui";
import { serverClient, sessionState } from "@/lib/api/server";
import { BeeError } from "@/lib/api/transport";
import { guard, param, type SearchParams } from "@/lib/page";
import { pageTools } from "@/lib/tools";
import { everything } from "./everything";

export const metadata: Metadata = { title: "Feed" };
const PAGE = 20;

export default async function FeedPage({ searchParams }: { searchParams: SearchParams }) {
  const sp = await searchParams;
  const all = param(sp, "tab") === "everything";
  const cursor = all ? "" : param(sp, "cursor");
  const [{ me }, { mode }] = await Promise.all([sessionState(), requestMode()]);
  if (!me) redirect("/signin?next=" + (all ? encodeURIComponent("/feed?tab=everything") : "/feed"));
  return guard(async () => {
    const api = await serverClient();
    const req = { page: { pageSize: PAGE, pageToken: cursor || undefined } };
    let items: PostListItem[];
    let older: string | null = null;
    if (all) {
      items = await everything(api, PAGE);
    } else {
      const res = await api.post.listFollowingFeed(req).catch((err: unknown) => {
        if (cursor && err instanceof BeeError && /bad_page_token/i.test(err.reason)) redirect("/feed");
        throw err;
      });
      items = res.items.flatMap(({ post, repostedBy, at }) => post ? [{ post, repostedBy, at }] : []);
      if (res.page?.nextPageToken) older = "/feed?cursor=" + encodeURIComponent(res.page.nextPageToken);
    }
    items = await Promise.all(items.map(async (item) => ({ ...item, ownerName: await postOwner(item.post, me) })));
    const empty = cursor ? "There are no older posts." : all ? "There are no published posts in the repositories you can read." : "There are no posts from the repositories or people you follow.";
    const tabs = <TabLinks label="Feeds" items={[
      { label: "Following", href: "/feed", active: !all },
      { label: "Everything", href: "/feed?tab=everything", active: all },
    ]} />;
    const pager = <PostPager older={older} newest={cursor ? "/feed" : null} />;
    if (mode === "bee") return (
      <>
        {tabs}
        <PageFacts page="feed" title="Feed" groups={[
          { title: "feed", facts: [{ label: "view", value: all ? "Everything" : "Following" }], empty },
          ...(items.length ? items.map(postFacts) : [{ title: "posts", empty }]),
        ]} tools={pageTools(all ? "everything" : "feed")} />
        {pager}
      </>
    );
    const op = all
      ? { service: "SearchService" as const, method: "SearchRepos", request: { query: "" }, label: "Find repositories to read" }
      : { service: "PostService" as const, method: "ListFollowingFeed", request: req, label: "Read your Following feed" };
    return (
      <div className="mx-auto w-full max-w-[640px]">
        <PageHeader title="Feed" actions={<div className="flex items-center gap-2">
          {me.kind === "PRINCIPAL_KIND_HUMAN" ? <ButtonLink size="sm" href="/feed/new">Write a post</ButtonLink> : null}
          <PageMenu op={op} />
        </div>} />
        {tabs}
        {items.length ? <PostList items={items} signedIn label={all ? "Everything" : "Following posts"} /> : <p className="text-ink-2">{empty}</p>}
        {pager}
      </div>
    );
  });
}
