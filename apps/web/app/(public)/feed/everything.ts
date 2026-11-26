import type { BeeClient } from "@/lib/api/client";
import { mapLimit } from "@/lib/api/ops/shared";
import type { Post } from "@/lib/api/types";
import { optionalRead } from "@/components/posts/post-data";
import type { PostListItem } from "@/components/posts/post-list";

const timestamp = (p: Post) => p.publishedAt ?? p.createdAt ?? "";
const sortNewest = (a: Post, b: Post) => Date.parse(timestamp(b)) - Date.parse(timestamp(a)) || b.id.localeCompare(a.id);

/** There is no global post RPC. Merge each visible repository's newest candidates. */
export async function everything(api: BeeClient, limit: number): Promise<PostListItem[]> {
  const repos = new Set<string>();
  let pageToken: string | undefined;
  do {
    const page = await api.search.searchRepos({ query: "", page: { pageSize: 100, pageToken } });
    for (const hit of page.hits) if (hit.fullName) repos.add(hit.fullName);
    pageToken = page.page?.nextPageToken;
  } while (pageToken);
  let newest: Post[] = [];
  await mapLimit([...repos], 6, async (fullName) => {
    const page = await optionalRead(api.post.listRepoPosts({ repo: { fullName }, page: { pageSize: limit } }));
    if (!page) return;
    // Keep only the best candidates so full article bodies do not accumulate per repository.
    newest = [...newest, ...page.posts.filter((post) => post.state === "POST_STATE_PUBLISHED")]
      .sort(sortNewest).slice(0, limit);
  });
  return newest.map((post) => ({ post }));
}
