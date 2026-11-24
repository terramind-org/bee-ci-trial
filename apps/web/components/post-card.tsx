"use client";

/**
 * A post on a merged change request, as the feed, a repo's Updates and the permalink show it.
 * Document style: no box, 16px reading text, the author and the time as a byline, the media below
 * the words, and a quiet footer naming the change it came from.
 *
 *   ⟲ Reposted by Rhea
 *   (avatar) kit · via Claude Code                                   …
 *            lattice/agent-kit · 2h
 *            Tools register once and get typed arguments for free. #agents
 *            [ media: one, two side by side, one plus two, or two by two ]
 *            Merged Add the tool registry · #31
 *            ♥ 12   ⟲ 3   💬 5                      Adopted by 2 repos
 *
 * Like and Repost are optimistic: the count moves at once and rolls back, with the server's
 * reason in a toast, if the call fails. Signed out they are links to sign in.
 */
import Link from "next/link";
import { useState, useTransition, type ReactNode } from "react";
import type { Op } from "@/lib/copyas";
import type { Post, PostMedia, PostViewer, Principal } from "@/lib/api/types";
import { plural } from "@/lib/format";
import { repoHref } from "@/lib/routes";
import { Avatar, toAvatar } from "./avatar";
import { CopyAsDialog } from "./copy-as";
import { Icon, type IconName } from "./icons";
import { Menu, MenuButton, MenuLink } from "./menu";
import { setPostLike, setPostRepost, type PostActionResult } from "./post-actions";
import { changeHref, countsOf, loginOf, mediaHref, mediaLayout, postHref, postRepo, tokenize, viaLine } from "./post-text";
import { Time } from "./time";
import { useToast } from "./toast";
import { buttonClass, cx } from "./ui";

export interface PostCardProps {
  post: Post;
  /** Set when the post is in the feed because someone the reader follows reposted it. */
  repostedBy?: Principal;
  signedIn: boolean;
  /** The permalink shows the post at a larger size and does not link the time to itself. */
  standalone?: boolean;
  /**
   * Where `<img>` and `<video>` load from. `route` (the default) goes through the permalink's
   * media route, which never expires; `direct` uses the signed URL on the post, for a draft in
   * the composer's preview that has no permalink yet.
   */
  mediaSource?: "route" | "direct";
  /** Hide the Like and Repost controls (the composer's preview of a draft). */
  preview?: boolean;
}

export function PostCard({ post, repostedBy, signedIn, standalone = false, mediaSource = "route", preview = false }: PostCardProps) {
  const repo = postRepo(post);
  const author = post.author;
  const login = loginOf(author);
  const via = viaLine(author);
  const href = postHref(post);
  const at = post.publishedAt ?? post.createdAt;
  const withdrawn = post.state === "POST_STATE_WITHDRAWN";
  const titleId = "post-" + post.id + "-by";
  return (
    <article className="min-w-0" aria-labelledby={titleId} data-testid="post-card" data-post-id={post.id}>
      {repostedBy ? (
        <p className="mb-2 flex items-center gap-2 pl-[52px] text-sm text-ink-3">
          <Icon name="repost" size={16} />
          <span>
            Reposted by <PersonLink principal={repostedBy} />
          </span>
        </p>
      ) : null}
      <div className="flex gap-3">
        {author ? <Avatar principal={toAvatar(author)} size={40} className="mt-0.5" /> : <span className="size-10 shrink-0" />}
        <div className="min-w-0 flex-1">
          <header className="flex items-start gap-2">
            <div className="min-w-0 flex-1">
              <p id={titleId} className="flex flex-wrap items-baseline gap-x-1.5 text-base">
                {login ? (
                  <Link href={"/" + login} className="inline-block min-h-6 leading-6 font-medium text-ink hover:underline">
                    {author?.display}
                  </Link>
                ) : (
                  <span className="font-medium text-ink">{author?.display || "Someone"}</span>
                )}
                {via ? <span className="text-ink-3">{via}</span> : null}
              </p>
              <p className="flex flex-wrap items-baseline gap-x-1.5 text-sm text-ink-3">
                <Link href={repoHref(repo)} className="inline-block min-h-6 leading-6 text-ink-2 hover:text-ink hover:underline">
                  {repo}
                </Link>
                {at ? (
                  <>
                    <span aria-hidden="true">·</span>
                    {standalone ? (
                      <Time iso={at} />
                    ) : (
                      <Link href={href} className="inline-block min-h-6 leading-6 hover:text-ink hover:underline">
                        <Time iso={at} />
                      </Link>
                    )}
                  </>
                ) : null}
              </p>
            </div>
            {preview || standalone ? null : <CardMenu post={post} />}
          </header>

          {withdrawn ? (
            <p className="mt-3 text-md text-ink-2" data-testid="post-withdrawn">
              This post was withdrawn{post.withdrawnReason ? ": " + post.withdrawnReason : "."}
            </p>
          ) : (
            <>
              {post.text ? (
                <p className={cx("mt-2 text-md whitespace-pre-wrap text-ink [overflow-wrap:anywhere]", standalone ? "text-lg leading-8" : "leading-7")}>
                  <PostText text={post.text} />
                </p>
              ) : null}
              {post.media.length ? <MediaGrid post={post} source={mediaSource} /> : null}
            </>
          )}

          <p className="mt-3 text-sm text-ink-3">
            Merged{" "}
            <Link href={changeHref(post)} className="inline-block min-h-6 leading-6 text-ink-2 hover:text-ink hover:underline">
              <em>{post.changeTitle || "a change request"}</em>
              {post.changeNumber && post.changeNumber !== "0" ? <span className="num"> · #{post.changeNumber}</span> : null}
            </Link>
          </p>

          {preview || withdrawn ? null : <Footer post={post} signedIn={signedIn} />}
        </div>
      </div>
    </article>
  );
}

function PersonLink({ principal }: { principal: Principal }) {
  const login = loginOf(principal);
  return login ? (
    <Link href={"/" + login} className="inline-block min-h-6 leading-6 font-medium text-ink-2 hover:text-ink hover:underline">
      {principal.display}
    </Link>
  ) : (
    <span className="font-medium text-ink-2">{principal.display}</span>
  );
}

const LINK = "text-ink underline decoration-line-strong underline-offset-[3px] hover:decoration-ink";

/** The post's words with URLs, `#tags` and `@mentions` as links. */
export function PostText({ text }: { text: string }) {
  return (
    <>
      {tokenize(text).map((t, i) => {
        if (t.kind === "text") return <span key={i}>{t.text}</span>;
        if (t.kind === "url") {
          return (
            <a key={i} href={t.href} rel="nofollow ugc noopener" target="_blank" className={LINK}>
              {t.text.replace(/^https?:\/\//, "")}
            </a>
          );
        }
        return (
          <Link key={i} href={t.href} className={LINK}>
            {t.text}
          </Link>
        );
      })}
    </>
  );
}

/** Ratio of a single image's box: its own, kept between 4:5 and 2:1 so it never towers or slivers. */
function ratioOf(m: PostMedia): string {
  const r = m.width && m.height ? m.width / m.height : 16 / 10;
  const clamped = Math.min(2, Math.max(0.8, r));
  return String(Number(clamped.toFixed(4)));
}

function MediaGrid({ post, source }: { post: Post; source: "route" | "direct" }) {
  const media = post.media;
  const layout = mediaLayout(media.length);
  const src = (m: PostMedia) => (source === "direct" ? m.url : mediaHref(post, m));
  const video = media.find((m) => m.kind === "POST_MEDIA_KIND_VIDEO");
  if (video) {
    return (
      <div className="mt-3 overflow-hidden rounded-xl bg-sunken" style={{ aspectRatio: ratioOf(video) }}>
        <video
          src={src(video)}
          controls
          preload="metadata"
          playsInline
          className="size-full"
          aria-label={video.altText || "Video attached to the post"}
          width={video.width || undefined}
          height={video.height || undefined}
          data-testid="post-video"
        />
      </div>
    );
  }
  const images = media.slice(0, 4);
  return (
    <div
      className={cx(
        "mt-3 grid gap-1 overflow-hidden rounded-xl",
        layout === "one" && "grid-cols-1",
        layout === "two" && "aspect-[16/9] grid-cols-2",
        (layout === "three" || layout === "four") && "aspect-[16/10] grid-cols-2 grid-rows-2",
      )}
      style={layout === "one" && images[0] ? { aspectRatio: ratioOf(images[0]) } : undefined}
      data-layout={layout}
      data-testid="post-media"
    >
      {images.map((m, i) => (
        <a key={m.id} href={src(m)} target="_blank" rel="noopener" className={cx("block min-h-0 overflow-hidden bg-sunken", layout === "three" && i === 0 && "row-span-2")}>
          {/* A plain <img>: the source is a redirect to a signed URL, which next/image cannot optimise. */}
          {/* eslint-disable-next-line @next/next/no-img-element */}
          <img src={src(m)} alt={m.altText} width={m.width || undefined} height={m.height || undefined} loading="lazy" decoding="async" className="size-full object-cover" />
        </a>
      ))}
    </div>
  );
}

function CardMenu({ post }: { post: Post }) {
  const [copy, setCopy] = useState(false);
  const toast = useToast();
  const op: Op = { service: "PostService", method: "GetPost", request: { postId: post.id }, label: "Read this post" };
  return (
    <>
      <Menu label={"More actions for this post"} labelled triggerClass={buttonClass("ghost", "sm", "-mt-1 -mr-2 size-9 px-0 text-ink-3")} trigger={<Icon name="more" size={20} weight="bold" />}>
        <MenuLink href={postHref(post)} icon="link">
          Open post
        </MenuLink>
        <MenuLink href={changeHref(post)} icon="merge">
          Open change request
        </MenuLink>
        <MenuButton
          icon="copy"
          onSelect={() => {
            void navigator.clipboard?.writeText(window.location.origin + postHref(post)).then(() => toast.push({ tone: "ok", title: "Link copied" }));
          }}
        >
          Copy link
        </MenuButton>
        <MenuButton icon="terminal" onSelect={() => setCopy(true)}>
          Copy as CLI, HTTP or MCP
        </MenuButton>
      </Menu>
      {copy ? <CopyAsDialog op={op} open={copy} onOpenChange={setCopy} /> : null}
    </>
  );
}

function Footer({ post, signedIn }: { post: Post; signedIn: boolean }) {
  const [counts, setCounts] = useState(() => countsOf(post));
  const [viewer, setViewer] = useState<Pick<PostViewer, "liked" | "reposted">>({ liked: !!post.viewer?.liked, reposted: !!post.viewer?.reposted });
  const [pending, start] = useTransition();
  const toast = useToast();
  const href = postHref(post);

  const toggle = (key: "liked" | "reposted", count: "likes" | "reposts", act: (id: string, on: boolean) => Promise<PostActionResult>, words: { on: string; off: string }) => {
    const want = !viewer[key];
    const before = { counts, viewer };
    setViewer({ ...viewer, [key]: want });
    setCounts({ ...counts, [count]: Math.max(0, counts[count] + (want ? 1 : -1)) });
    start(async () => {
      const res = await act(post.id, want);
      if (!res.ok) {
        setViewer(before.viewer);
        setCounts(before.counts);
        toast.push({ tone: "bad", title: (want ? words.on : words.off) + " failed · " + res.reason, detail: res.message });
        return;
      }
      // The server's counts win once it answers: they include everyone else's taps too.
      if (res.counts) setCounts((c) => ({ ...c, ...countsOf({ counts: res.counts }), adoptions: c.adoptions }));
      if (res.viewer) setViewer({ liked: !!res.viewer.liked, reposted: !!res.viewer.reposted });
    });
  };

  return (
    <div className="mt-2 -ml-2 flex flex-wrap items-center gap-x-1 gap-y-1 text-sm text-ink-3">
      <Action
        icon="heart"
        label="Like"
        count={counts.likes}
        on={viewer.liked}
        signedIn={signedIn}
        next={href}
        busy={pending}
        onClick={() => toggle("liked", "likes", setPostLike, { on: "Like", off: "Unlike" })}
        tone="text-bad"
      />
      <Action
        icon="repost"
        label="Repost"
        count={counts.reposts}
        on={viewer.reposted}
        signedIn={signedIn}
        next={href}
        busy={pending}
        onClick={() => toggle("reposted", "reposts", setPostRepost, { on: "Repost", off: "Undo repost" })}
        tone="text-ok"
      />
      {counts.comments > 0 ? (
        <Link href={changeHref(post)} className={ACTION} aria-label={plural(counts.comments, "comment") + " on the change request"}>
          <Icon name="comment" size={18} />
          <span className="num">{counts.comments}</span>
        </Link>
      ) : null}
      {counts.adoptions > 0 ? (
        <Link href={href + "#adoptions"} className="ml-auto inline-flex min-h-9 items-center px-2 text-ink-2 hover:text-ink hover:underline" data-testid="post-adoptions">
          Adopted by {plural(counts.adoptions, "repo")}
        </Link>
      ) : null}
    </div>
  );
}

const ACTION = "inline-flex min-h-9 min-w-9 items-center gap-1.5 rounded-lg px-2 hover:bg-hover hover:text-ink focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent";

function Action({ icon, label, count, on, signedIn, next, busy, onClick, tone }: { icon: IconName; label: string; count: number; on: boolean; signedIn: boolean; next: string; busy: boolean; onClick: () => void; tone: string }) {
  const body: ReactNode = (
    <>
      <Icon name={icon} size={18} weight={on ? "fill" : "regular"} className={on ? tone : undefined} />
      <span className="sr-only">{label}</span>
      <span className={cx("num", on && "text-ink")}>{count}</span>
    </>
  );
  if (!signedIn) {
    return (
      <Link href={"/signin?next=" + encodeURIComponent(next)} className={ACTION}>
        {body}
        <span className="sr-only"> (sign in first)</span>
      </Link>
    );
  }
  return (
    <button type="button" className={ACTION} aria-pressed={on} aria-busy={busy || undefined} onClick={onClick}>
      {body}
    </button>
  );
}
