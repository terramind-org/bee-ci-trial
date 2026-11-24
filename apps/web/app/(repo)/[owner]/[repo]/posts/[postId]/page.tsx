import type { Metadata } from "next";
import { headers } from "next/headers";
import Link from "next/link";
import { notFound } from "next/navigation";
import { cache } from "react";
import { Avatar, toAvatar } from "@/components/avatar";
import { PageMenu } from "@/components/menu";
import { PostCard } from "@/components/post-card";
import { countsOf, describePost, mediaHref, postHref, postTitle, viaLine } from "@/components/post-text";
import { Time } from "@/components/time";
import { Badge, PageHeader } from "@/components/ui";
import { scopeFromParams, serverClient, sessionState } from "@/lib/api/server";
import { BeeError } from "@/lib/api/transport";
import type { Post, PostAdoption } from "@/lib/api/types";
import { plural } from "@/lib/format";
import { guard } from "@/lib/page";
import { repoHref } from "@/lib/routes";

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
  if (post.state === "POST_STATE_WITHDRAWN") return { title, description: "This post was withdrawn.", robots: { index: false } };
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
  POST_STATE_DRAFT: "This is a draft. It publishes when the change request merges.",
  POST_STATE_PENDING_APPROVAL: "An agent wrote this post. It publishes once a person approves it.",
  POST_STATE_DISCARDED: "The change request closed without merging, so this post was never published.",
};

/** `/{owner}/{repo}/posts/{postId}`: one post, and the repos that adopted what it describes. */
export default async function PostPage({ params }: { params: Params }) {
  return guard(async () => {
    const { fullName } = await scopeFromParams(params);
    const { postId } = await params;
    const [post, { me }] = await Promise.all([loadPost(fullName, decodeURIComponent(postId)), sessionState()]);
    if (!post) notFound();
    const withdrawn = post.state === "POST_STATE_WITHDRAWN";
    const note = STATE_NOTE[post.state];
    const op = { service: "PostService" as const, method: "GetPost", request: { postId: post.id }, label: "Read this post" };
    return (
      <div className="max-w-[640px]">
        <PageHeader
          eyebrow={
            <Link href={repoHref(fullName, "updates")} className="hover:text-ink hover:underline">
              Updates
            </Link>
          }
          title={post.changeTitle || "Post"}
          actions={<PageMenu op={op} />}
        />
        {note ? <p className="-mt-6 mb-8 text-base text-ink-2">{note}</p> : null}
        <PostCard post={post} signedIn={!!me} standalone />
        {withdrawn ? null : <Adoptions post={post} signedIn={!!me} />}
      </div>
    );
  });
}

/**
 * Who adopted the upgrade: each repo with the issue or change request it opened, its author, and
 * an Agent badge when an agent opened it. Only repos the reader can read are listed (and counted),
 * so a private repo never shows through an adoption. Signed out the list cannot be read.
 */
async function Adoptions({ post, signedIn }: { post: Post; signedIn: boolean }) {
  const total = countsOf(post).adoptions;
  let adoptions: PostAdoption[] = [];
  if (signedIn) {
    try {
      adoptions = (await (await serverClient()).post.listPostAdoptions({ postId: post.id, page: { pageSize: 50 } })).adoptions;
    } catch (err) {
      if (!(err instanceof BeeError)) throw err;
    }
  }
  if (!adoptions.length && !total) return null;
  // An issue adoption names its issue by id (the fixture by number); the issue's page needs the number.
  const numbers = new Map<string, string>();
  const repos = [...new Set(adoptions.filter((a) => a.subject?.kind === "SUBJECT_KIND_ISSUE" && !/^\d+$/.test(a.subject.id)).map((a) => a.repo?.fullName ?? a.subject?.repo?.fullName ?? ""))].filter(Boolean);
  await Promise.all(
    repos.map(async (r) => {
      try {
        for (const i of (await (await serverClient()).issue.listIssues({ repo: { fullName: r } })).issues) numbers.set(r + ":" + i.id, String(i.number));
      } catch (err) {
        if (!(err instanceof BeeError)) throw err;
      }
    }),
  );
  return (
    <section id="adoptions" aria-labelledby="adoptions-title" className="mt-14 scroll-mt-20" data-testid="post-adoptions-section">
      <h2 id="adoptions-title" className="text-xl font-medium text-ink">
        Adoptions
      </h2>
      <p className="mt-1 text-base text-ink-2">{plural(Math.max(total, adoptions.length), "repo")} took this upgrade into their own work.</p>
      {adoptions.length ? (
        <ul className="mt-6 flex flex-col gap-6">
          {adoptions.map((a, i) => {
            const repo = a.repo?.fullName ?? a.subject?.repo?.fullName ?? "";
            const issue = a.subject?.kind === "SUBJECT_KIND_ISSUE";
            const issueNumber = issue ? (numbers.get(repo + ":" + a.subject?.id) ?? a.subject?.id ?? "") : "";
            const href = repoHref(repo, (issue ? "work/" + encodeURIComponent(issueNumber) : "changes/" + encodeURIComponent(a.subject?.id ?? "")));
            const agent = a.author?.kind === "PRINCIPAL_KIND_AGENT";
            return (
              <li key={repo + ":" + (a.subject?.id ?? i)} className="flex gap-3">
                {a.author ? <Avatar principal={toAvatar(a.author)} size={32} className="mt-0.5" /> : <span className="size-8 shrink-0" />}
                <div className="min-w-0 flex-1">
                  <p className="text-base text-ink-2">
                    <Link href={repoHref(repo)} className="font-medium text-ink hover:underline">
                      {repo}
                    </Link>{" "}
                    {issue ? "opened an issue" : "opened a change request"}
                    {a.author ? <> · {a.author.display}</> : null}
                    {agent ? (
                      <Badge className="ml-2 align-middle" title={viaLine(a.author)}>
                        Agent
                      </Badge>
                    ) : null}
                  </p>
                  <Link href={href} className="mt-0.5 block text-md text-ink hover:underline">
                    {a.title || (issue ? "Issue " + issueNumber : "Change request")}
                    {issue && /^\d+$/.test(issueNumber) ? <span className="num text-ink-3"> · #{issueNumber}</span> : null}
                  </Link>
                  {a.createdAt ? <Time iso={a.createdAt} className="text-sm text-ink-3" /> : null}
                </div>
              </li>
            );
          })}
        </ul>
      ) : (
        <p className="mt-4 text-base text-ink-2">
          <Link href={"/signin?next=" + encodeURIComponent(postHref(post) + "#adoptions")} className="link">
            Sign in
          </Link>{" "}
          to see which repos adopted it.
        </p>
      )}
    </section>
  );
}
