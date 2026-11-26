import type { Metadata } from "next";
import { headers } from "next/headers";
import Link from "next/link";
import { notFound } from "next/navigation";
import { cache } from "react";
import { requestMode } from "@/app/request-mode";
import { PageFacts } from "@/components/bee/page-facts";
import { State } from "@/components/parts";
import { ApprovePost } from "@/components/posts/approve";
import { AdoptionLine, Adoptions, PostComments, loadAdoptions } from "@/components/posts/conversation";
import { optionalRead, postOwner } from "@/components/posts/post-data";
import { postFacts } from "@/components/posts/post-facts";
import { PageMenu } from "@/components/menu";
import { PostCard } from "@/components/posts/post-card";
import { describePost, mediaHref, postHref, postTitle } from "@/components/posts/post-text";
import { PageHeader } from "@/components/ui";
import { scopeFromParams, serverClient, sessionState } from "@/lib/api/server";
import { BeeError } from "@/lib/api/transport";
import type { Post } from "@/lib/api/types";
import { guard } from "@/lib/page";
import { repoHref } from "@/lib/routes";
import { pageTools } from "@/lib/tools";

type Params = Promise<{ owner: string; repo: string; postId: string }>;

/**
 * The post, once per request (metadata and page both read it), as the visitor may see it. Null
 * when it does not exist, the visitor cannot read its repo, or it belongs to another repo than the
 * one in the URL: all three are a plain 404, so a private post never leaks through its id.
 */
const loadPost = cache(async (fullName: string, postId: string): Promise<Post | null> => {
  try {
    const post = await (await serverClient()).post.getPost({ postId });
    return post.repo?.fullName === fullName ? post : null;
  } catch (err) {
    if (err instanceof BeeError && ["not_found", "permission_denied", "unauthenticated", "invalid_argument"].includes(err.code)) return null;
    throw err;
  }
});

async function origin(): Promise<string> {
  const h = await headers();
  const host = h.get("x-forwarded-host") ?? h.get("host") ?? "localhost";
  const proto = h.get("x-forwarded-proto") ?? (host.startsWith("localhost") || host.startsWith("127.") ? "http" : "https");
  return proto + "://" + host;
}

export async function generateMetadata({ params }: { params: Params }): Promise<Metadata> {
  const { fullName } = await scopeFromParams(params);
  const { postId } = await params;
  const post = await loadPost(fullName, decodeURIComponent(postId));
  if (!post) return { title: "Not found" };
  const title = postTitle(post);
  if (post.state !== "POST_STATE_PUBLISHED") return { title: "Post", robots: { index: false } };
  const base = await origin();
  const url = base + postHref(post);
  const description = describePost(post.text) || "A post on " + post.repo?.fullName + ".";
  const image = post.media.find((m) => m.kind === "POST_MEDIA_KIND_IMAGE");
  const video = post.media.find((m) => m.kind === "POST_MEDIA_KIND_VIDEO");
  const images = image ? [{ url: base + mediaHref(post, image), width: image.width || undefined, height: image.height || undefined, alt: image.altText }] : undefined;
  return {
    title,
    description,
    alternates: { canonical: url },
    openGraph: {
      type: "article",
      url,
      title,
      description,
      siteName: "Beehive",
      publishedTime: post.publishedAt,
      authors: post.author?.display ? [post.author.display] : undefined,
      images,
      videos: video ? [{ url: base + mediaHref(post, video), type: video.contentType, width: video.width || undefined, height: video.height || undefined }] : undefined,
    },
    twitter: { card: image ? "summary_large_image" : "summary", title, description, images: images?.map((i) => ({ url: i.url, alt: i.alt })) },
  };
}

const STATE_NOTE: Partial<Record<Post["state"], string>> = {
  POST_STATE_DRAFT: "This is a draft. It publishes when the pull request merges.",
  POST_STATE_DISCARDED: "The pull request closed without merging, so this post was never published.",
};

/** `/{owner}/{repo}/posts/{postId}`: one post, and the repos that adopted what it describes. */
export default async function PostPage({ params }: { params: Params }) {
  return guard(async () => {
    const { fullName, ref } = await scopeFromParams(params);
    const { postId } = await params;
    const [post, { me }, { mode }, api] = await Promise.all([loadPost(fullName, postId), sessionState(), requestMode(), serverClient()]);
    if (!post) notFound();

    const published = post.state === "POST_STATE_PUBLISHED";
    const pending = post.state === "POST_STATE_PENDING_APPROVAL";
    const [ownerName, adoptions, conversation] = await Promise.all([
      postOwner(post, me),
      published && me ? loadAdoptions(api, post.id) : undefined,
      published && post.changeId ? optionalRead(api.change.listComments({ repo: ref, changeRequestId: post.changeId, page: { pageSize: 200 } })) : undefined,
    ]);
    const ownerId = post.author?.ownerId;
    const canApprove = pending && post.viewer?.canApprove && me?.kind === "PRINCIPAL_KIND_HUMAN" && (!ownerId || ownerId === me.id);
    const note = pending ? "Waiting for " + (ownerName || "a person with write access") + " to approve" : STATE_NOTE[post.state];
    const title = post.article?.title || post.changeTitle || "Post";
    const tools = pageTools("post").filter(({ op }) => {
      if (op === "PostService/GetPost") return true;
      if (op === "PostService/ApprovePost") return pending;
      if (op === "ChangeService/ListComments") return published && !!post.changeId;
      return published;
    });
    if (mode === "bee") {
      const facts = postFacts({ post, ownerName });
      if (note) facts.facts?.unshift({ label: "state", value: note });
      return <PageFacts page="post" title={title} groups={[
        facts,
        ...(adoptions?.length ? [{ title: "adoptions", lines: adoptions.map((item, i) => <AdoptionLine key={i} item={item} />) }] : []),
        ...(conversation ? [{
          title: "comments on the pull request",
          lines: conversation.comments.map((c) => <span key={c.id} className="whitespace-pre-wrap">{[c.author?.display, c.createdAt, c.body].filter(Boolean).join("\n")}</span>),
          empty: "There are no comments yet.",
        }] : []),
      ]} tools={tools} />;
    }
    const op = { service: "PostService" as const, method: "GetPost", request: { postId: post.id }, label: "Read this post" };
    return (
      <div className="mx-auto w-full max-w-[640px]">
        <PageHeader
          eyebrow={<Link href={repoHref(fullName, "updates")} className="link">Updates</Link>}
          title={title}
          actions={<PageMenu op={op} />}
        />
        {note ? <div className="mb-6 flex flex-wrap items-center gap-3">
          <State shape={pending ? "waiting" : post.state === "POST_STATE_DRAFT" ? "open" : "closed"}>{note}</State>
          {canApprove ? <ApprovePost postId={post.id} fullName={fullName} /> : null}
        </div> : null}
        <PostCard post={post} signedIn={!!me} ownerName={ownerName} standalone primary={!!me && published} />
        {published ? <Adoptions post={post} items={adoptions} /> : null}
        {conversation ? <PostComments repo={fullName} comments={conversation.comments} /> : null}
      </div>
    );
  });
}
