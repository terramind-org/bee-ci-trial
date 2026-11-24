/**
 * "Inspired by <repo>'s post": the banner at the top of a change request or issue that adopted an
 * upgrade another repo posted about. It names the source repo, quotes the start of the post with
 * its first image, and links to the permalink. The server sets `inspired_by` only for a reader who
 * may read the source repo, and `InspiredByBanner` (inspired-by-banner.tsx) drops the banner when the post
 * itself cannot be read, so it never names a repo the reader cannot see.
 */
import { LightbulbIcon } from "@phosphor-icons/react/dist/ssr";
import Link from "next/link";
import type { PostRef } from "@/lib/api/types";
import { repoHref } from "@/lib/routes";
import { describePost } from "./post-text";

/** The part of the source post the banner shows. */
export interface InspiredPost {
  text: string;
  /** The first image, as the permalink's media route. */
  thumb?: { src: string; alt: string };
}

export function InspiredBy({ inspiredBy, repo, post }: { inspiredBy: PostRef; /** owner/name of the post's repo. */ repo: string; post?: InspiredPost }) {
  const number = Number(inspiredBy.changeNumber);
  const excerpt = post ? describePost(post.text) : "";
  return (
    <aside aria-label="Inspired by" data-inspired-by className="flex items-start gap-3 border-y border-line py-3.5">
      <LightbulbIcon size={22} aria-hidden="true" className="mt-0.5 shrink-0 text-ink-2" />
      <div className="min-w-0 flex-1">
        <p className="text-base text-ink-2">
          <Link href={repoHref(repo, "posts/" + encodeURIComponent(inspiredBy.postId))} className="font-medium text-ink underline decoration-line-strong underline-offset-4 hover:decoration-ink">
            Inspired by {repo}&rsquo;s post
          </Link>
          {inspiredBy.changeTitle ? (
            <>
              {" about "}
              <span className="text-ink">{inspiredBy.changeTitle}</span>
              {number ? <span className="num text-ink-3"> (#{number})</span> : null}
            </>
          ) : null}
        </p>
        {excerpt ? (
          <blockquote className="mt-1.5 text-base text-ink-2" data-inspired-excerpt>
            {excerpt}
          </blockquote>
        ) : null}
      </div>
      {post?.thumb ? (
        // A plain <img>: the source is a redirect to a signed URL, which next/image cannot optimise.
        // eslint-disable-next-line @next/next/no-img-element
        <img src={post.thumb.src} alt={post.thumb.alt} width={96} height={64} loading="lazy" decoding="async" className="h-16 w-24 shrink-0 rounded-md border border-line object-cover" data-inspired-thumb />
      ) : null}
    </aside>
  );
}
