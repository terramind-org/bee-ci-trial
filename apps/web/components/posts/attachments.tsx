import Link from "next/link";
import { State, type Shape } from "@/components/parts";
import type { Post, PostAttachment, PostAttachmentKind } from "@/lib/api/types";
import { plural } from "@/lib/format";
import { repoHref } from "@/lib/routes";
import { PostDisclosure } from "./disclosure";

const LABELS: Partial<Record<PostAttachmentKind, string>> = {
  POST_ATTACHMENT_KIND_CHANGE_REQUEST: "pull request",
  POST_ATTACHMENT_KIND_RELEASE: "release",
  POST_ATTACHMENT_KIND_ISSUE: "issue",
  POST_ATTACHMENT_KIND_AGENT_RUN: "agent run",
};

const STATES: Record<string, [string, Shape]> = {
  open: ["Open", "open"],
  landed: ["Merged", "merged"],
  closed: ["Closed", "closed"],
  published: ["Published", "passed"],
  prerelease: ["Prerelease", "open"],
  queued: ["Queued", "waiting"],
  provisioning: ["Provisioning", "running"],
  running: ["Running", "running"],
  waiting_for_review: ["Waiting for review", "waiting"],
  succeeded: ["Succeeded", "passed"],
  failed: ["Failed", "failed"],
  cancelled: ["Cancelled", "closed"],
};

export function postAttachments(post: Post): PostAttachment[] {
  if (post.attachments?.length) return post.attachments.filter((a) => LABELS[a.kind]);
  if (!post.changeId) return [];
  // Older servers supplied the carrying pull request only in the legacy fields.
  return [{
    kind: "POST_ATTACHMENT_KIND_CHANGE_REQUEST", id: post.changeId, number: post.changeNumber, title: post.changeTitle,
    state: post.state === "POST_STATE_PUBLISHED" ? "landed" : undefined,
  }];
}

export function attachmentLabel(a: PostAttachment): string {
  return (a.number && a.number !== "0" ? "#" + a.number + " " : "") + (a.title || LABELS[a.kind] || "");
}

export function attachmentState(a: PostAttachment) {
  return a.state ? STATES[a.state] : undefined;
}

function attachmentHref(repo: string, a: PostAttachment) {
  if (!a.title) return null;
  switch (a.kind) {
    case "POST_ATTACHMENT_KIND_CHANGE_REQUEST": return a.id ? repoHref(repo, "changes/" + encodeURIComponent(a.id)) : null;
    case "POST_ATTACHMENT_KIND_ISSUE": return a.number && a.number !== "0" ? repoHref(repo, "work/" + a.number) : null;
    case "POST_ATTACHMENT_KIND_RELEASE": return repoHref(repo, "releases") + (a.tagName ? "#" + encodeURIComponent(a.tagName) : "");
    default: return null;
  }
}

export function Attachments({ post, expanded }: { post: Post; expanded: boolean }) {
  const attachments = postAttachments(post);
  if (!attachments.length) return null;
  const summary = Object.entries(LABELS).flatMap(([kind, label]) => {
    const count = attachments.filter((a) => a.kind === kind).length;
    return count ? [plural(count, label)] : [];
  }).join(" · ");
  const cards = (
    <ul aria-label="Attachments" className="divide-y divide-line" data-testid="post-attachments">
      {attachments.map((a) => {
        const href = attachmentHref(post.repo?.fullName ?? "", a);
        const state = attachmentState(a);
        return (
          <li key={a.kind + ":" + (a.id || a.number)} className="flex flex-wrap items-center justify-between gap-x-4 gap-y-1 py-2 text-base">
            {href ? <Link href={href} className="inline-flex min-h-6 items-center hover:underline">{attachmentLabel(a)}</Link> : <span>{attachmentLabel(a)}</span>}
            {state ? <State shape={state[1]} className="text-xs">{state[0]}</State> : null}
          </li>
        );
      })}
    </ul>
  );
  return <div data-testid="post-attachment-row">{expanded ? cards : <PostDisclosure label={summary}>{cards}</PostDisclosure>}</div>;
}
