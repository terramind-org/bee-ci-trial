/**
 * A column of post cards and the link to the next page, shared by the Following feed and a repo's
 * Updates. Cards are separated by space and a hairline, never boxed. Paging is by cursor: the
 * server's page token goes in the URL, so every page is a link and the back button works.
 */
import Link from "next/link";
import type { Post, Principal } from "@/lib/api/types";
import { PostCard } from "./post-card";
import { Line } from "./skeletons";

export interface PostListItem {
  post: Post;
  repostedBy?: Principal;
}

export function PostList({ items, signedIn, label }: { items: PostListItem[]; signedIn: boolean; label: string }) {
  return (
    <ol aria-label={label} className="flex flex-col" data-testid="post-list">
      {items.map((it, i) => (
        <li key={it.post.id + ":" + (it.repostedBy?.id ?? "")} className={i === 0 ? "pb-8" : "border-t border-line/70 py-8"}>
          <PostCard post={it.post} repostedBy={it.repostedBy} signedIn={signedIn} />
        </li>
      ))}
    </ol>
  );
}

/** "Older posts" when the server has another page; "Back to the newest" once the reader paged. */
export function PostPager({ older, newest }: { older: string | null; newest: string | null }) {
  if (!older && !newest) return null;
  return (
    <nav aria-label="Pages" className="mt-4 flex items-center justify-between gap-4 border-t border-line/70 pt-6 text-base">
      {newest ? (
        <Link href={newest} className="inline-flex min-h-10 items-center gap-2 text-ink-2 hover:text-ink hover:underline">
          Back to the newest
        </Link>
      ) : (
        <span />
      )}
      {older ? (
        <Link href={older} rel="next" className="inline-flex min-h-10 items-center gap-2 font-medium text-ink hover:underline">
          Older posts →
        </Link>
      ) : null}
    </nav>
  );
}

/** Post cards' places while a list loads: avatar, byline, three lines of text and a media block. */
export function PostListSkeleton({ count = 3 }: { count?: number }) {
  return (
    <ol aria-hidden="true" className="flex flex-col">
      {Array.from({ length: count }, (_, i) => (
        <li key={i} className={i === 0 ? "pb-8" : "border-t border-line/70 py-8"}>
          <div className="flex gap-3">
            <span className="size-10 shrink-0 animate-pulse rounded-full bg-hover" />
            <div className="min-w-0 flex-1">
              <Line w={160} lh={22} bar={10} />
              <Line w={120} lh={20} bar={9} />
              <div className="mt-2 flex flex-col gap-1">
                <Line w="92%" lh={28} bar={11} />
                <Line w="84%" lh={28} bar={11} />
                <Line w={`${40 + ((i * 13) % 30)}%`} lh={28} bar={11} />
              </div>
              <span className="mt-3 block aspect-[16/10] w-full animate-pulse rounded-xl bg-hover" />
            </div>
          </div>
        </li>
      ))}
    </ol>
  );
}
