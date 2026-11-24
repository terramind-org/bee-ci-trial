/**
 * The pure parts of a change request's Post panel (components/post/post-panel.tsx): the text
 * counter, the media rule, the state line, who sees what, the "Use evidence" choices, the
 * same-origin upload address and the draft the live preview shows. No React and no browser, so
 * the panel and the tests read a post the same way.
 */
import type { ChangeRequest, Evidence, EvidenceAsset, Post, PostMedia, PostMediaKind, Principal } from "./api/types";
import { MAX_IMAGES, mediaKind } from "./post-media";

/** The server's text limit, in Unicode scalar values of the NFC text (design: Rules, Text). */
export const MAX_TEXT = 500;
/** The counter turns amber this many characters before the limit. */
export const NEAR_LIMIT = 50;

/** Characters as the server counts them: scalar values after NFC. */
export function textLength(text: string): number {
  return Array.from(text.normalize("NFC")).length;
}

export type CounterTone = "ok" | "near" | "over";

export function counterTone(length: number): CounterTone {
  if (length > MAX_TEXT) return "over";
  if (length > MAX_TEXT - NEAR_LIMIT) return "near";
  return "ok";
}

/** What the server refuses in text other than length: control characters except `\n`. */
export function badTextChar(text: string): boolean {
  for (const ch of text) {
    const c = ch.codePointAt(0) ?? 0;
    if (ch !== "\n" && (c < 0x20 || (c >= 0x7f && c <= 0x9f))) return true;
  }
  return false;
}

/** One attachment in the composer, by kind; `alt` is what the person typed or what it carries. */
export interface Attachment {
  kind: PostMediaKind;
  alt: string;
}

/**
 * Why the attachments cannot be saved as they are, in words, or null when they can: 1 to 4
 * images, or exactly one video and nothing beside it, and every image described.
 */
export function mediaProblem(items: Attachment[]): string | null {
  const videos = items.filter((m) => m.kind === "POST_MEDIA_KIND_VIDEO").length;
  const images = items.filter((m) => m.kind === "POST_MEDIA_KIND_IMAGE").length;
  if (videos > 1) return "A post takes one video.";
  if (videos && images) return "A video goes alone: remove the images, or the video.";
  if (images > MAX_IMAGES) return "A post takes at most " + MAX_IMAGES + " images.";
  const undescribed = items.filter((m) => m.kind === "POST_MEDIA_KIND_IMAGE" && !m.alt.trim()).length;
  if (undescribed) return undescribed === 1 ? "Describe the image for people who cannot see it." : "Describe each image for people who cannot see them.";
  return null;
}

/** Whether one more file of `kind` fits beside `items` (the picker greys out otherwise). */
export function canAdd(items: Pick<Attachment, "kind">[], kind: PostMediaKind): boolean {
  if (kind === "POST_MEDIA_KIND_VIDEO") return items.length === 0;
  if (items.some((m) => m.kind === "POST_MEDIA_KIND_VIDEO")) return false;
  return items.length < MAX_IMAGES;
}

/** Why the draft as a whole cannot be saved, or null: text rules first, then media. */
export function draftProblem(text: string, items: Attachment[]): string | null {
  const n = textLength(text);
  if (n > MAX_TEXT) return "That is " + (n - MAX_TEXT) + " characters over the " + MAX_TEXT + " limit.";
  if (badTextChar(text)) return "Remove the control characters; only line breaks are kept.";
  if (!text.trim() && !items.length) return "Write something, or attach an image or a video.";
  return mediaProblem(items);
}

export function isAgent(p: Pick<Principal, "kind"> | undefined): boolean {
  return !!p && p.kind !== "PRINCIPAL_KIND_HUMAN";
}

export type StateTone = "neutral" | "warn" | "ok" | "bad";

export interface StateLine {
  tone: StateTone;
  text: string;
  /** Who approved an agent's draft before it landed; shown after the line. */
  approvedBy?: string;
  /** The permalink, for a published post. */
  href?: string;
}

/**
 * The one sentence under the panel's heading that says where the post is in its life:
 * DRAFT publishes on merge (an agent's needs a person first), PENDING_APPROVAL waits for one,
 * PUBLISHED links to its permalink, WITHDRAWN gives the reason, DISCARDED says why it never ran.
 */
export function stateLine(post: Post | null | undefined, permalink: string): StateLine {
  if (!post) return { tone: "neutral", text: "No post yet. Write one and it publishes when this change merges." };
  switch (post.state) {
    case "POST_STATE_DRAFT":
      if (isAgent(post.author) && !post.approvedBy) return { tone: "warn", text: "Needs a person to approve before it publishes" };
      if (isAgent(post.author)) return { tone: "neutral", text: "Publishes when this change merges", approvedBy: post.approvedBy?.display };
      return { tone: "neutral", text: "Publishes when this change merges" };
    case "POST_STATE_PENDING_APPROVAL":
      return { tone: "warn", text: "Merged. Waiting for a person to approve this post" };
    case "POST_STATE_PUBLISHED":
      return { tone: "ok", text: "Published", href: permalink };
    case "POST_STATE_WITHDRAWN":
      return { tone: "bad", text: post.withdrawnReason ? "Withdrawn: " + post.withdrawnReason : "Withdrawn" };
    case "POST_STATE_DISCARDED":
      return { tone: "neutral", text: "Discarded: this change was closed without merging, so the post will not publish" };
    default:
      return { tone: "neutral", text: "This post is in a state this page does not know" };
  }
}

/** What the person looking at the panel may do. Built by the mount (post-panel-slot.tsx). */
export interface PanelViewer {
  signedIn: boolean;
  /** May write the post: the change's author or someone with Write, while it is still a draft. */
  canEdit: boolean;
  /** `PostViewer.can_approve`: a person with Write who did not write this agent's draft. */
  canApprove: boolean;
  /** `PostViewer.can_withdraw`: the post's author or a repo admin, once it is pending or published. */
  canWithdraw: boolean;
  /**
   * Wrote the change or may write the repo, whatever state the post is in: such a person still
   * sees what became of the post (pending, withdrawn with its reason, discarded) once they can no
   * longer edit it.
   */
  involved: boolean;
}

/**
 * The viewer from what the server said. A post carries its own `viewer`, which wins; with no post
 * yet, writing is up to whether the change is open and the caller wrote it or may write the repo.
 */
export function panelViewer(change: Pick<ChangeRequest, "state" | "author" | "post">, me: Pick<Principal, "id"> | null, canWrite: boolean): PanelViewer {
  const author = !!me && !!change.author && change.author.id === me.id;
  const involved = !!me && (author || canWrite);
  const v = change.post?.viewer;
  if (change.post && v) return { signedIn: !!me, canEdit: v.canEdit, canApprove: v.canApprove, canWithdraw: v.canWithdraw, involved };
  const open = change.state === "CHANGE_STATE_OPEN";
  return { signedIn: !!me, canEdit: involved && open, canApprove: false, canWithdraw: false, involved };
}

export type PanelMode = "compose" | "manage" | "card" | "hidden";

/**
 * What the panel shows. A writer gets the composer; someone who may approve or withdraw, or who
 * wrote the change, gets the post with its state and those actions; any other reader sees a
 * published post as a card and nothing otherwise.
 */
export function panelMode(post: Post | null | undefined, viewer: PanelViewer): PanelMode {
  if (viewer.canEdit) return "compose";
  if (post && (viewer.canApprove || viewer.canWithdraw || viewer.involved)) return "manage";
  if (post?.state === "POST_STATE_PUBLISHED") return "card";
  return "hidden";
}

/** An image or video of this change's evidence that a post can take. */
export interface EvidenceChoice {
  key: string;
  evidenceId: string;
  title: string;
  kind: PostMediaKind;
  url: string;
  contentType: string;
  alt: string;
  sizeBytes: number;
  width: number;
  height: number;
  stale: boolean;
}

const POSTABLE_ROLES = new Set(["EVIDENCE_ASSET_ROLE_MAIN", "EVIDENCE_ASSET_ROLE_BEFORE", "EVIDENCE_ASSET_ROLE_AFTER", "EVIDENCE_ASSET_ROLE_UNSPECIFIED"]);
const ROLE_WORD: Record<string, string> = { EVIDENCE_ASSET_ROLE_BEFORE: "before", EVIDENCE_ASSET_ROLE_AFTER: "after" };

/**
 * The change's ready evidence as post media: each main, before or after asset whose type a post
 * takes (png, jpeg, gif, webp, mp4, webm), in the order the server listed them. Posters and
 * captions are a video's parts, not media of their own, and are left out.
 */
export function evidenceChoices(items: Evidence[]): EvidenceChoice[] {
  const out: EvidenceChoice[] = [];
  for (const e of items) {
    if (e.state !== "EVIDENCE_STATE_READY") continue;
    const title = e.spec?.title.trim() || "Evidence";
    const postable = (a: EvidenceAsset) => !!mediaKind(a.contentType) && POSTABLE_ROLES.has(a.role) && !!a.url;
    // An image set names each of its images by place, so two pictures never read the same.
    const plain = e.assets.filter((a) => postable(a) && !ROLE_WORD[a.role]).length;
    let n = 0;
    e.assets.forEach((a: EvidenceAsset, i) => {
      const kind = mediaKind(a.contentType);
      if (!kind || !postable(a)) return;
      const role = ROLE_WORD[a.role];
      const place = role ? " (" + role + ")" : plain > 1 ? " (" + ++n + " of " + plain + ")" : "";
      out.push({
        key: e.id + ":" + i,
        evidenceId: e.id,
        title: title + place,
        kind,
        url: a.url,
        contentType: a.contentType,
        alt: a.alt || e.spec?.caption || "",
        sizeBytes: Number(a.sizeBytes) || 0,
        width: a.width,
        height: a.height,
        stale: e.stale,
      });
    });
  }
  return out;
}

/** The same-origin route that carries a post-media upload to the server (app/api/post-media). */
export const UPLOAD_ROUTE = "/api/post-media/";

/**
 * The address the browser PUTs a post's media to. `BeginPostMediaUpload` answers the server's
 * signed `/posts/<repo-id>/<media-id>?expires=…&signature=…` (absolute or not), or the fixture's
 * `/api/fixture/posts/<repo-id>/<media-id>`. Both become `/api/post-media/<repo-id>/<media-id>`
 * with the query kept, so the browser never sends bytes across origins. Anything else is refused.
 */
export function uploadPath(uploadUrl: string): string | null {
  let u: URL;
  try {
    u = new URL(uploadUrl, "http://same.origin");
  } catch {
    return null;
  }
  const m = /^(?:\/api\/fixture)?\/posts\/([\w.:-]{1,128})\/([\w.:-]{1,128})$/.exec(u.pathname);
  if (!m) return null;
  return UPLOAD_ROUTE + encodeURIComponent(m[1] ?? "") + "/" + encodeURIComponent(m[2] ?? "") + u.search;
}

/** The parts of a route path back: repo id and media id, or null when it is not one. */
export function parseUploadSegments(segments: string[]): { repoId: string; mediaId: string } | null {
  if (segments.length !== 2) return null;
  const [repoId = "", mediaId = ""] = segments;
  return /^[\w.:-]{1,128}$/.test(repoId) && /^[\w.:-]{1,128}$/.test(mediaId) ? { repoId, mediaId } : null;
}

/**
 * The post with its repo named. The panel always knows the change's repo by name, while the
 * server may identify a post's repo by id alone; links (`postHref`, `PostCard`) need the name.
 */
export function withRepo(post: Post, repo: string): Post {
  return post.repo?.fullName === repo ? post : { ...post, repo: { fullName: repo } };
}

/** One thing in the live preview: uploaded media as it is, or a local file by its object URL. */
export interface PreviewMedia {
  id: string;
  kind: PostMediaKind;
  url: string;
  alt: string;
  width: number;
  height: number;
  contentType: string;
}

/**
 * The post the live preview shows: the saved post (or a new one) with the composer's text and
 * media, by the person who would become its author, as it will read once published.
 */
export function previewPost(change: Pick<ChangeRequest, "id" | "number" | "title">, repo: string, saved: Post | null | undefined, text: string, media: PreviewMedia[], author: Principal | undefined): Post {
  const m: PostMedia[] = media.map((x) => ({
    id: x.id,
    kind: x.kind,
    contentType: x.contentType,
    sizeBytes: "0",
    width: x.width,
    height: x.height,
    durationMs: 0,
    altText: x.alt,
    sha256: "",
    url: x.url,
  }));
  return {
    id: saved?.id || "preview",
    repo: { fullName: repo },
    changeId: change.id,
    changeNumber: String(change.number ?? "0"),
    changeTitle: change.title,
    author: author ?? saved?.author,
    text: text.normalize("NFC"),
    media: m,
    state: "POST_STATE_PUBLISHED",
    createdAt: saved?.createdAt,
    updatedAt: saved?.updatedAt,
    publishedAt: saved?.publishedAt,
    approvedBy: saved?.approvedBy,
    counts: saved?.counts,
    version: saved?.version ?? "0",
    withdrawnReason: "",
  };
}
