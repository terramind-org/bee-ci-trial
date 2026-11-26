"use client";

import Link from "next/link";
import { useState, useTransition, type ReactNode } from "react";
import type { Post, PostMedia, PostViewer, Principal } from "@/lib/api/types";
import { plural } from "@/lib/format";
import { repoHref } from "@/lib/routes";
import { Avatar, toAvatar } from "../avatar";
import { Icon, type IconName } from "../icons";
import { Markdown } from "../markdown";
import { Mark } from "../parts";
import { Button, ButtonLink } from "../kit";
import { Adopt } from "./adopt";
import { Attachments } from "./attachments";
import { PostDisclosure } from "./disclosure";
import { setPostLike, setPostRepost, type PostActionResult } from "./post-actions";
import { countsOf, loginOf, mediaHref, mediaLayout, postHref, postRepo, tokenize, viaLine } from "./post-text";
import { Time } from "../time";
import { useToast } from "../toast";
import { buttonClass, cx } from "../ui";

export interface PostCardProps {
  post: Post;
  /** Set when the post is in the feed because someone the reader follows reposted it. */
  repostedBy?: Principal;
  signedIn: boolean;
  ownerName?: string;
  primary?: boolean;
  at?: string;
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

export function PostCard({ post, repostedBy, signedIn, ownerName, primary = false, at: feedAt, standalone = false, mediaSource = "route", preview = false }: PostCardProps) {
  const repo = postRepo(post);
  const author = post.author;
  const login = loginOf(author);
  const via = viaLine(author);
  const href = postHref(post);
  const at = feedAt ?? post.publishedAt ?? post.createdAt;
  const published = post.state === "POST_STATE_PUBLISHED";
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
      <div className="flex items-start gap-3">
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
                {via ? <span className="text-ink-2">{via}</span> : null}
                {ownerName ? <Mark className="text-sm text-ink-2">for {ownerName}</Mark> : null}
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
          </header>

        </div>
      </div>

      {withdrawn ? (
        <p className="mt-3 text-md text-ink-2" data-testid="post-withdrawn">
          This post was withdrawn{post.withdrawnReason ? ": " + post.withdrawnReason : "."}
        </p>
      ) : (
        <div className="mt-3 flex min-w-0 flex-col gap-3">
          {post.text ? (
            <p className={cx("whitespace-pre-wrap text-ink [overflow-wrap:anywhere]", standalone ? "text-lg leading-8" : "text-md leading-7")} data-testid="post-summary">
              <PostText text={post.text} />
            </p>
          ) : null}
          {post.media.length ? <MediaGrid post={post} source={mediaSource} /> : null}
          <Attachments post={post} expanded={standalone} />
          {post.bodyMarkdown ? <PostBody body={post.bodyMarkdown} repo={repo} expanded={standalone} /> : null}
          {post.article ? (
            <section className="border-t border-line pt-3" aria-label="Article" data-testid="post-article">
              {standalone ? <Markdown source={post.article.bodyMarkdown} repo={repo} headingOffset={1} /> : (
                <div className="flex items-center justify-between gap-4">
                  <span className="min-w-0 font-medium [overflow-wrap:anywhere]">{post.article.title}</span>
                  <ButtonLink href={href} size="sm">Read</ButtonLink>
                </div>
              )}
            </section>
          ) : null}
          {!preview && published ? <Footer post={post} signedIn={signedIn} primary={primary} standalone={standalone} /> : null}
        </div>
      )}
    </article>
  );
}

function PostBody({ body, repo, expanded }: { body: string; repo: string; expanded: boolean }) {
  // Keep a complete first paragraph when it is short. The disclosure always renders the full Markdown.
  const first = body.trim().split(/\r?\n\s*\r?\n/, 1)[0] ?? "";
  if (expanded || (body.trim() === first && first.length <= 240 && first.split("\n").length <= 3)) {
    return <div data-post-body><Markdown source={body} repo={repo} headingOffset={1} className="[overflow-wrap:anywhere]" /></div>;
  }
  return (
    <div data-post-body>
      {first.length <= 240 && first.split("\n").length <= 3 ? <Markdown source={first} repo={repo} headingOffset={1} className="[overflow-wrap:anywhere]" /> : null}
      <PostDisclosure label="More" openLabel="Less">
        <Markdown source={first.length <= 240 && first.split("\n").length <= 3 ? body.trim().slice(first.length).trim() : body} repo={repo} headingOffset={1} className="[overflow-wrap:anywhere]" />
      </PostDisclosure>
    </div>
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
      <div className="overflow-hidden rounded-xl bg-sunken" style={{ aspectRatio: ratioOf(video) }}>
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
        "grid gap-1 overflow-hidden rounded-xl",
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

function Footer({ post, signedIn, primary, standalone }: { post: Post; signedIn: boolean; primary: boolean; standalone: boolean }) {
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
    <div className="flex flex-wrap items-center gap-1 text-sm text-ink-2" data-testid="post-actions">
      <Adopt post={post} signedIn={signedIn} primary={primary} />
      <Action
        icon="heart"
        label="Like"
        count={counts.likes}
        on={viewer.liked}
        signedIn={signedIn}
        next={href}
        busy={pending}
        onClick={() => toggle("liked", "likes", setPostLike, { on: "Like", off: "Unlike" })}
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
      />
      {post.counts && (post.changeId ? (
        <Link href={href + "#comments"} className={ACTION} aria-label={plural(counts.comments, "comment") + " on the pull request"}>
          <Icon name="comment" size={18} />
          <span className="num">{counts.comments}</span>
        </Link>
      ) : (
        <span className="inline-flex min-h-6 items-center gap-1.5 px-2">
          <Icon name="comment" size={18} />
          <span className="sr-only">{plural(counts.comments, "comment")}</span>
          <span aria-hidden="true" className="num">{counts.comments}</span>
        </span>
      ))}
      <Button size="sm" variant="ghost" icon="link" onClick={async () => {
        try {
          await navigator.clipboard.writeText(window.location.origin + href);
          toast.push({ tone: "ok", title: "Link copied" });
        } catch {
          toast.push({ tone: "bad", title: "Could not copy the link", detail: window.location.origin + href });
        }
      }}>Share link</Button>
      {!standalone && counts.adoptions > 0 ? (
        <Link href={href + "#adoptions"} className="ml-auto inline-flex min-h-9 items-center px-2 text-ink-2 hover:text-ink hover:underline" data-testid="post-adoptions">
          Adopted by {plural(counts.adoptions, "repo")}
        </Link>
      ) : null}
    </div>
  );
}

const ACTION = buttonClass("ghost", "sm", "px-2");

function Action({ icon, label, count, on, signedIn, next, busy, onClick }: { icon: IconName; label: string; count: number; on: boolean; signedIn: boolean; next: string; busy: boolean; onClick: () => void }) {
  const body: ReactNode = (
    <>
      <Icon name={icon} size={18} weight={on ? "fill" : "regular"} />
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
    <Button type="button" variant="ghost" size="sm" aria-pressed={on} disabled={busy} onClick={onClick}>
      {body}
    </Button>
  );
}
