import Link from "next/link";
import type { FactGroup } from "@/components/bee/page-facts";
import { plural } from "@/lib/format";
import { attachmentLabel, attachmentState, postAttachments } from "./attachments";
import type { PostListItem } from "./post-list";
import { mediaHref, postHref, viaLine } from "./post-text";

const raw = (text: string) => <span className="whitespace-pre-wrap" data-post-content>{text}</span>;

export function postFacts({ post, ownerName, repostedBy, at }: PostListItem, index?: number): FactGroup {
  const author = [post.author?.display, viaLine(post.author), ownerName && "for " + ownerName].filter(Boolean).join(" · ");
  const attachments = postAttachments(post);
  const counts = post.counts;
  const withdrawn = post.state === "POST_STATE_WITHDRAWN";
  const publishedAt = at ?? post.publishedAt ?? post.createdAt;
  return {
    title: index === undefined ? "post" : "post " + (index + 1),
    facts: [
      author && { label: "author", value: author },
      repostedBy && { label: "reposted by", value: repostedBy.display },
      post.repo?.fullName && { label: "repository", value: post.repo.fullName },
      publishedAt && { label: "time", value: publishedAt },
      { label: "permalink", value: <Link href={postHref(post)} className="link">{postHref(post)}</Link> },
      withdrawn && { label: "state", value: "Withdrawn" + (post.withdrawnReason ? " · " + post.withdrawnReason : "") },
      !withdrawn && post.text && { label: "summary", value: raw(post.text) },
      !withdrawn && !!post.media.length && {
        label: "media", value: post.media.map((m) => (
          <Link className="link block" data-post-content key={m.id} href={mediaHref(post, m)}>{m.altText || (m.kind === "POST_MEDIA_KIND_VIDEO" ? "Video" : "Image")}</Link>
        )),
      },
      !withdrawn && !!attachments.length && {
        label: "attachments", value: attachments.map((a) => [attachmentLabel(a), attachmentState(a)?.[0]].filter(Boolean).join(" · ")).join("\n"),
      },
      !withdrawn && post.bodyMarkdown && { label: "body Markdown", value: raw(post.bodyMarkdown) },
      !withdrawn && post.article && {
        label: "article", value: index === undefined
          ? raw(post.article.title + "\n\n" + post.article.bodyMarkdown)
          : <Link href={postHref(post)} className="link">{post.article.title}</Link>,
      },
      !withdrawn && counts && {
        label: "engagement", value: [plural(Number(counts.likes), "like"), plural(Number(counts.reposts), "repost"), plural(Number(counts.comments), "comment"), plural(Number(counts.adoptions), "adoption")].join(" · "),
      },
    ].map((fact) => fact || null),
  };
}
