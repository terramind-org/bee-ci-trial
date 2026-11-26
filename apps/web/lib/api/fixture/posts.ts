/**
 * Fixture handlers for posts and following: the PostService, repo follows in the UserService,
 * the AgentService feed trigger and the runs it started. The rules are the server's (design
 * "Posts on merged change requests"): states, who may write, approval, visibility, the Following
 * feed's order, dedupe and cursor, likes, reposts, adoptions and `inspired_by`; and posts of
 * their own (`CreatePost`): no change request, a Markdown body, an article and attachments.
 *
 * State lives in `PostsWorld` on the store. Time is the store's clock and ids are derived from
 * content, so a run of the fixture is reproducible.
 */
import { createHash } from "node:crypto";
import type { AgentRunJson, AgentService } from "../../gen/bee/v1/agents_pb";
import { PostService } from "../../gen/bee/v1/posts_pb";
import type { UserService } from "../../gen/bee/v1/users_pb";
import { BeeError } from "../transport";
import type * as T from "../types";
import { defineService, type Ctx, type Impl } from "./router";
import { ulid } from "./seed";
import type { FixtureStore, IssueRow, RepoState } from "./store";
import { invalid, page } from "./util";
import {
  ADOPT_RUNS,
  ADOPTIONS,
  LIKES,
  MEDIA_ALT,
  MEDIA_FILES,
  OWN_POST_SEEDS,
  POST_SEEDS,
  REPO_FOLLOWS,
  REPOSTS,
  TRIGGERS,
} from "./posts-world";
import { CRS } from "./world";

const MIN = 60_000;
const DAY = 86_400_000;
const MAX_TEXT = 500;
const MAX_BODY = 20_000;
const MAX_ARTICLE = 100_000;
const MAX_ARTICLE_TITLE = 200;
const MAX_ATTACHMENTS = 32;
const MAX_IMAGES = 4;
const IMAGE_TYPES = ["image/png", "image/jpeg", "image/gif", "image/webp"];
const VIDEO_TYPES = ["video/mp4", "video/webm"];
const MAX_IMAGE_BYTES = 8 * 1024 * 1024;
const MAX_VIDEO_BYTES = 64 * 1024 * 1024;
const UPLOAD_TTL = 15 * MIN;

type State = T.PostState;
const DRAFT: State = "POST_STATE_DRAFT";
const PENDING: State = "POST_STATE_PENDING_APPROVAL";
const PUBLISHED: State = "POST_STATE_PUBLISHED";
const WITHDRAWN: State = "POST_STATE_WITHDRAWN";
const DISCARDED: State = "POST_STATE_DISCARDED";

const A_CHANGE: T.PostAttachmentKind = "POST_ATTACHMENT_KIND_CHANGE_REQUEST";
const A_RELEASE: T.PostAttachmentKind = "POST_ATTACHMENT_KIND_RELEASE";
const A_ISSUE: T.PostAttachmentKind = "POST_ATTACHMENT_KIND_ISSUE";
const A_RUN: T.PostAttachmentKind = "POST_ATTACHMENT_KIND_AGENT_RUN";

/** An attachment as stored: what it points at, nothing of its card. */
export interface AttachmentRef {
  kind: T.PostAttachmentKind;
  id: string;
}

export interface PostRow {
  id: string;
  /** Store key of the repo. */
  repo: string;
  /** The change request that carries the post; "" for a post of its own. */
  crId: string;
  /** Handle of the author: whoever last set the text. */
  author: string;
  text: string;
  bodyMarkdown?: string;
  article?: T.PostArticle;
  /** Beside the change request that carries the post, which is always its first attachment. */
  attachments?: AttachmentRef[];
  mediaIds: string[];
  state: State;
  createdAt: number;
  updatedAt: number;
  publishedAt?: number;
  approvedBy?: string;
  version: number;
  withdrawnReason: string;
  comments: number;
}

export interface MediaRow {
  id: string;
  repo: string;
  /** "" when it was uploaded for a post of its own; then only `by` may finalize and attach it. */
  crId: string;
  by: string;
  kind: T.PostMediaKind;
  contentType: string;
  sizeBytes: number;
  width: number;
  height: number;
  durationMs: number;
  altText: string;
  sha256: string;
  /** A file under `public/fixture/posts/` for seeded media. */
  file?: string;
  /** What a client PUT, for media uploaded through the fixture. */
  bytes?: Uint8Array;
  finalized: boolean;
  attachedTo?: string;
  createdAt: number;
}

interface Engagement {
  post: string;
  who: string;
  at: number;
}

interface FollowRow {
  repo: string;
  who: string;
  at: number;
}

export interface AdoptionRow {
  post: string;
  /** Store key of the repo the adoption lives in. */
  repo: string;
  kind: "issue" | "change";
  /** The issue number, or the change request id. */
  ref: string;
  title: string;
  by: string;
  at: number;
}

export interface TriggerRow {
  repo: string;
  agent: string;
  enabled: boolean;
  dailyUsdBudget: number;
  maxRunsPerDay: number;
  sources: string[];
  createdAt: number;
  updatedAt: number;
}

export interface AdoptRunRow {
  id: string;
  repo: string;
  agent: string;
  post: string;
  startedAt: number;
  finishedAt: number;
  summary: string;
  usdMicros: number;
}

const lower = (x: string) => x.toLowerCase();
const nfc = (x: string) => x.normalize("NFC");

const notFound = (id: string) => new BeeError("not_found", { reason: "POST_NOT_FOUND", message: "no post " + id });
const invalidState = (message: string) => new BeeError("failed_precondition", { reason: "INVALID_STATE", message });
const denied = (reason: string, message: string) => new BeeError("permission_denied", { reason, message });

/** The signed-in caller of a request the gate has already authenticated. */
const me = (s: FixtureStore, ctx: Ctx): string => s.caller(ctx.token);

/** A cursor into the Following feed: "<at_ms>|<post_id>". */
function cursorOf(at: number, id: string): string {
  return at + "|" + id;
}

function parseCursor(token: string | undefined): { at: number; id: string } | undefined {
  if (!token) return undefined;
  const m = /^(\d{1,15})\|([0-9A-Z]{26})$/.exec(token);
  if (!m) throw new BeeError("invalid_argument", { reason: "bad_page_token", message: "the page token or cursor is not valid: " + token });
  return { at: Number(m[1]), id: m[2] as string };
}

/** Magic bytes of the formats a post accepts; anything else (SVG, HTML, scripts) is refused. */
export function sniff(bytes: Uint8Array): string | undefined {
  const b = bytes;
  const at = (i: number, ...v: number[]) => v.every((x, k) => b[i + k] === x);
  if (at(0, 0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a)) return "image/png";
  if (at(0, 0xff, 0xd8, 0xff)) return "image/jpeg";
  if (at(0, 0x47, 0x49, 0x46, 0x38)) return "image/gif";
  if (at(0, 0x52, 0x49, 0x46, 0x46) && at(8, 0x57, 0x45, 0x42, 0x50)) return "image/webp";
  if (at(4, 0x66, 0x74, 0x79, 0x70)) return "video/mp4";
  if (at(0, 0x1a, 0x45, 0xdf, 0xa3)) return "video/webm";
  return undefined;
}

export class PostsWorld {
  posts: PostRow[] = [];
  media = new Map<string, MediaRow>();
  likes: Engagement[] = [];
  reposts: Engagement[] = [];
  follows: FollowRow[] = [];
  adoptions: AdoptionRow[] = [];
  triggers: TriggerRow[] = [];
  runs: AdoptRunRow[] = [];

  // ------------------------------------------------------------------ lookups

  byId(id: string | undefined): PostRow | undefined {
    return this.posts.find((p) => p.id === id);
  }
  forChange(crId: string): PostRow | undefined {
    return crId ? this.posts.find((p) => p.crId === crId) : undefined;
  }

  // ------------------------------------------------------------------ who may do what

  /** Repo Write: a member of the owning org, or an agent that member owns. */
  canWrite(s: FixtureStore, who: string | undefined, repo: RepoState): boolean {
    return !!who && s.inOrg(who, repo.owner);
  }

  /** The change request's author, or anyone with repo Write. */
  canEditChange(s: FixtureStore, who: string | undefined, repo: RepoState, authorOfChange: string): boolean {
    return !!who && (who === authorOfChange || this.canWrite(s, who, repo));
  }

  private roleOf(s: FixtureStore, who: string, org: string): T.Role | undefined {
    const rec = s.orgs.get(org);
    const own = rec?.members.get(who);
    if (own) return own;
    const owner = s.p(who).ownerId;
    return owner ? rec?.members.get(s.handleOf(owner)) : undefined;
  }

  isAdmin(s: FixtureStore, who: string | undefined, repo: RepoState): boolean {
    if (!who) return false;
    const r = this.roleOf(s, who, repo.owner);
    return r === "ROLE_OWNER" || r === "ROLE_ADMIN";
  }

  /** Whether the viewer may see the post at all (repo Read, and drafts only to those who edit). */
  visible(s: FixtureStore, p: PostRow, viewer: string | undefined): boolean {
    const repo = s.repos.get(p.repo);
    if (!repo || !s.canRead(viewer, repo)) return false;
    if (p.state === PUBLISHED || p.state === WITHDRAWN) return true;
    // A post of its own that waits for approval: its author and those who may approve it.
    if (!p.crId) return !!viewer && (viewer === p.author || this.canWrite(s, viewer, repo));
    const cr = s.crs.find((c) => c.id === p.crId);
    return !!cr && this.canEditChange(s, viewer, repo, cr.author);
  }

  // ------------------------------------------------------------------ views

  mediaView(m: MediaRow): T.PostMedia {
    let url = "";
    if (m.file) url = "/fixture/posts/" + m.file;
    else if (m.bytes) url = "data:" + m.contentType + ";base64," + Buffer.from(m.bytes).toString("base64");
    return {
      id: m.id,
      kind: m.kind,
      contentType: m.contentType,
      sizeBytes: String(m.sizeBytes),
      width: m.width,
      height: m.height,
      durationMs: m.durationMs,
      altText: m.altText,
      sha256: m.sha256,
      url,
    };
  }

  repoRef(s: FixtureStore, key: string): T.PostRepo {
    const r = s.repos.get(key) as RepoState;
    // A RepoRef holds one of id and full_name, never both; screens link by name.
    return { fullName: r.repo.fullName };
  }

  /** The card of an attachment as it is now, or undefined when the repo has no such thing. */
  card(s: FixtureStore, repoKey: string, a: T.PostAttachment): T.PostAttachment | undefined {
    const repo = s.repos.get(repoKey) as RepoState;
    const id = (a.id ?? "").trim();
    const numbered = (n: number) => !id && !!a.number && a.number !== "0" && String(n) === String(a.number);
    if (a.kind === A_CHANGE) {
      const c = s.crs.find((x) => x.repo === repoKey && (x.id === id || numbered(x.number)));
      return c && { kind: a.kind, id: c.id, number: String(c.number), title: c.title, state: c.state.replace("CHANGE_STATE_", "").toLowerCase() };
    }
    if (a.kind === A_ISSUE) {
      const i = s.issues.find((x) => x.repoId === repo.repo.id && (x.id === id || numbered(x.number)));
      return i && { kind: a.kind, id: i.id, number: String(i.number), title: i.title, state: i.state };
    }
    if (a.kind === A_RELEASE) {
      const r = s.releases.find((x) => x.repoId === repo.repo.id && x.id === id);
      return r && { kind: a.kind, id: r.id, title: r.name || r.tag, state: r.prerelease ? "prerelease" : "published", tagName: r.tag };
    }
    if (a.kind === A_RUN) {
      const r = this.runs.find((x) => x.repo === repoKey && x.id === id);
      return r && { kind: a.kind, id: r.id, title: s.p(r.agent).display, state: "succeeded" };
    }
    return undefined;
  }

  /** `Post.attachments`: the change request that carries the post first, then the stored ones, as cards. */
  attachmentsView(s: FixtureStore, p: PostRow): T.PostAttachment[] {
    const refs: AttachmentRef[] = [...(p.crId ? [{ kind: A_CHANGE, id: p.crId }] : []), ...(p.attachments ?? []).filter((a) => !(a.kind === A_CHANGE && a.id === p.crId))];
    return refs.map((a) => this.card(s, p.repo, a) ?? { kind: a.kind, id: a.id });
  }

  /**
   * Checks the attachments a writer sent and returns them as stored, without the change request
   * that carries the post. Everything must be in the post's own repo.
   */
  resolve(s: FixtureStore, repoKey: string, input: T.PostAttachment[], ownChange: string): AttachmentRef[] {
    if (input.length > MAX_ATTACHMENTS) throw invalid("a post carries at most " + MAX_ATTACHMENTS + " attachments", "bad_attachment");
    const single = new Set<string>();
    const seen = new Set<string>();
    const out: AttachmentRef[] = [];
    for (const a of input) {
      if (a.kind === "POST_ATTACHMENT_KIND_SWARM") throw invalid("swarms cannot be attached to a post yet", "bad_attachment");
      if (a.kind !== A_CHANGE && a.kind !== A_RELEASE && a.kind !== A_ISSUE && a.kind !== A_RUN) throw invalid("an attachment is a change request, a release, an issue or an agent run", "bad_attachment");
      const byNumber = (a.kind === A_CHANGE || a.kind === A_ISSUE) && !!a.number && a.number !== "0";
      if (!(a.id ?? "").trim() && !byNumber) throw invalid("an attachment needs its id", "bad_attachment");
      if (a.kind !== A_CHANGE) {
        if (single.has(a.kind)) throw invalid("a post carries at most one release, one issue and one agent run", "bad_attachment");
        single.add(a.kind);
      }
      const card = this.card(s, repoKey, a);
      if (!card?.id) throw invalid("the attachment " + ((a.id ?? "").trim() || "#" + a.number) + " is not in this repository", "attachment_not_found");
      const key = card.kind + ":" + card.id;
      if (seen.has(key)) throw invalid("the attachment " + card.id + " is listed twice", "bad_attachment");
      seen.add(key);
      if (card.kind === A_CHANGE && card.id === ownChange) continue;
      out.push({ kind: card.kind, id: card.id });
    }
    return out;
  }

  /** Validates a Markdown body and an article and returns them as stored. */
  words(body: string | undefined, article: T.PostArticle | undefined): { bodyMarkdown?: string; article?: T.PostArticle } {
    const markdown = (x: string, max: number, code: string, what: string): string => {
      const t = nfc(x.replace(/\r\n/g, "\n"));
      if (/[\u0000-\u0008\u000b-\u001f\u007f-\u009f]/.test(t)) throw invalid(what + " cannot contain control characters other than a newline and a tab", "bad_text");
      if (Array.from(t).length > max) throw invalid(what + " is at most " + max + " characters", code);
      return t;
    };
    const b = markdown(body ?? "", MAX_BODY, "body_too_long", "a post body");
    const title = nfc((article?.title ?? "").trim());
    const long = article?.bodyMarkdown ?? "";
    let art: T.PostArticle | undefined;
    if (title || long.trim()) {
      if (!title || Array.from(title).length > MAX_ARTICLE_TITLE || /[\u0000-\u001f\u007f-\u009f]/.test(title)) throw invalid("an article needs a one-line title of 1 to " + MAX_ARTICLE_TITLE + " characters", "bad_article");
      const t = markdown(long, MAX_ARTICLE, "article_too_long", "an article");
      if (!t.trim()) throw invalid("an article needs a body", "bad_article");
      art = { title, bodyMarkdown: t };
    }
    return { bodyMarkdown: b.trim() ? b : undefined, article: art };
  }

  postView(s: FixtureStore, p: PostRow, viewer: string | undefined): T.Post {
    const repo = s.repos.get(p.repo) as RepoState;
    const cr = s.crs.find((c) => c.id === p.crId);
    const withdrawn = p.state === WITHDRAWN;
    const editable = p.state === DRAFT || p.state === PENDING;
    const agentWritten = s.kindOf(p.author) !== "human";
    const canEdit = editable && !!cr && this.canEditChange(s, viewer, repo, cr.author);
    return {
      id: p.id,
      repo: this.repoRef(s, p.repo),
      changeId: p.crId,
      changeNumber: String(cr?.number ?? 0),
      changeTitle: cr?.title ?? "",
      author: s.p(p.author),
      text: withdrawn ? "" : p.text,
      media: withdrawn ? [] : p.mediaIds.map((id) => this.mediaView(this.media.get(id) as MediaRow)),
      state: p.state,
      createdAt: s.iso(p.createdAt),
      updatedAt: s.iso(p.updatedAt),
      publishedAt: p.publishedAt === undefined ? undefined : s.iso(p.publishedAt),
      approvedBy: p.approvedBy ? s.p(p.approvedBy) : undefined,
      counts: {
        likes: String(this.likes.filter((l) => l.post === p.id).length),
        reposts: String(this.reposts.filter((l) => l.post === p.id).length),
        comments: String(p.comments),
        adoptions: String(this.adoptionsFor(s, p.id, viewer).length),
      },
      viewer: {
        liked: !!viewer && this.likes.some((l) => l.post === p.id && l.who === viewer),
        reposted: !!viewer && this.reposts.some((l) => l.post === p.id && l.who === viewer),
        canEdit,
        canApprove: editable && agentWritten && !p.approvedBy && !!viewer && s.kindOf(viewer) === "human" && viewer !== p.author && this.canWrite(s, viewer, repo),
        canWithdraw: !withdrawn && p.state !== DISCARDED && !!viewer && (viewer === p.author || this.isAdmin(s, viewer, repo)),
      },
      version: String(p.version),
      withdrawnReason: p.withdrawnReason,
      bodyMarkdown: withdrawn ? undefined : p.bodyMarkdown,
      article: withdrawn ? undefined : p.article,
      attachments: this.attachmentsView(s, p),
    };
  }

  /** `ChangeRequest.post`: the post as a summary, when the viewer may see it. */
  summaryFor(s: FixtureStore, crId: string, viewer: string | undefined): T.Post | undefined {
    const p = this.forChange(crId);
    return p && p.state !== DISCARDED && this.visible(s, p, viewer) ? this.postView(s, p, viewer) : undefined;
  }

  /** `inspired_by`: the post a change or issue names, when the viewer may read its repo. */
  refFor(s: FixtureStore, postId: string | undefined, viewer: string | undefined): T.PostRef | undefined {
    const p = this.byId(postId);
    const repo = p && s.repos.get(p.repo);
    if (!p || !repo || !s.canRead(viewer, repo)) return undefined;
    const cr = s.crs.find((c) => c.id === p.crId);
    return { postId: p.id, repo: this.repoRef(s, p.repo), changeNumber: String(cr?.number ?? 0), changeTitle: cr?.title ?? "" };
  }

  adoptionsFor(s: FixtureStore, postId: string, viewer: string | undefined): AdoptionRow[] {
    return this.adoptions
      .filter((a) => a.post === postId)
      .filter((a) => {
        const r = s.repos.get(a.repo);
        return !!r && s.canRead(viewer, r);
      })
      .sort((a, b) => b.at - a.at || a.ref.localeCompare(b.ref));
  }

  // ------------------------------------------------------------------ the post rules

  /** Validates text and media for a post and returns the normalised text. `words`: it has a body or an article. */
  validate(text: string, media: MediaRow[], words = false): string {
    const t = nfc(text);
    if (Array.from(t).length > MAX_TEXT) throw invalid("a post is at most " + MAX_TEXT + " characters", "text_too_long");
    if (/[\u0000-\u0009\u000b-\u001f\u007f-\u009f]/.test(t)) throw invalid("a post cannot contain control characters other than a newline", "bad_text");
    if (!t.trim() && media.length === 0 && !words) throw invalid("a post needs text, a body, an article or media", "empty_post");
    const images = media.filter((m) => m.kind === "POST_MEDIA_KIND_IMAGE");
    const videos = media.filter((m) => m.kind === "POST_MEDIA_KIND_VIDEO");
    if (images.length > MAX_IMAGES) throw invalid("a post has at most " + MAX_IMAGES + " images", "bad_media");
    if (videos.length > 1 || (videos.length === 1 && images.length > 0)) throw invalid("a post has one video, and no images beside it", "bad_media");
    for (const m of images) if (!m.altText.trim()) throw invalid("every image needs alt text", "alt_text_required");
    return t;
  }

  /** `post.adopted`: logged in the source repo, so the adopting repo never shows up in it. */
  adopted(s: FixtureStore, postId: string, actor: string): void {
    const p = this.byId(postId);
    if (p) this.announce(s, s.repos.get(p.repo) as RepoState, "post.adopted", actor, p);
  }

  /** Writes the draft a change opens with (`OpenChangeRequest.post_text`). */
  draft(s: FixtureStore, cr: { id: string; repo: string }, repo: RepoState, actor: string, text: string): PostRow {
    const now = s.now();
    const row: PostRow = { id: ulid(now, "post:new:" + cr.id), repo: cr.repo, crId: cr.id, author: actor, text: this.validate(text, []), mediaIds: [], state: DRAFT, createdAt: now, updatedAt: now, version: 1, withdrawnReason: "", comments: 0 };
    this.posts.push(row);
    this.announce(s, repo, "post.drafted", actor, row);
    return row;
  }

  /** Publishes the post of a change that just landed, in the same step as the landing. */
  onLanded(s: FixtureStore, crId: string): void {
    const p = this.forChange(crId);
    if (!p || p.state !== DRAFT) return;
    const repo = s.repos.get(p.repo) as RepoState;
    const now = s.now();
    p.updatedAt = now;
    if (s.kindOf(p.author) === "human" || p.approvedBy) {
      p.state = PUBLISHED;
      p.publishedAt = now;
      this.announce(s, repo, "post.published", p.author, p);
    } else {
      p.state = PENDING;
      this.announce(s, repo, "post.approval_requested", p.author, p);
    }
  }

  /** A change closed without landing discards its post. */
  onClosed(s: FixtureStore, crId: string): void {
    const p = this.forChange(crId);
    if (!p || (p.state !== DRAFT && p.state !== PENDING)) return;
    p.state = DISCARDED;
    p.updatedAt = s.now();
  }

  announce(s: FixtureStore, repo: RepoState, kind: string, actor: string, p: PostRow): void {
    s.emit(repo, kind, actor, { post: this.postView(s, p, undefined) });
  }

  // ------------------------------------------------------------------ the Following feed

  followedRepos(who: string): Set<string> {
    return new Set(this.follows.filter((f) => f.who === who).map((f) => f.repo));
  }

  /** Every published post the viewer sees in the Following feed, once, under its newest reason. */
  feed(s: FixtureStore, viewer: string): { post: PostRow; at: number; repostedBy?: string }[] {
    const login = s.loginByHandle.get(viewer) ?? viewer;
    const repos = this.followedRepos(viewer);
    const people = new Set(
      s.follows
        .filter((f) => lower(f.from) === lower(login))
        .map((f) => s.handleByLogin.get(lower(f.to)))
        .filter((h): h is string => !!h),
    );
    const best = new Map<string, { post: PostRow; at: number; repostedBy?: string }>();
    const offer = (post: PostRow, at: number, repostedBy?: string) => {
      const cur = best.get(post.id);
      if (!cur || at > cur.at) best.set(post.id, { post, at, repostedBy });
    };
    for (const p of this.posts) {
      if (p.state !== PUBLISHED || p.publishedAt === undefined || !this.visible(s, p, viewer)) continue;
      if (repos.has(p.repo) || people.has(p.author)) offer(p, p.publishedAt);
    }
    for (const r of this.reposts) {
      const p = this.byId(r.post);
      if (!p || p.state !== PUBLISHED || !people.has(r.who) || !this.visible(s, p, viewer)) continue;
      offer(p, r.at, r.who);
    }
    return [...best.values()].sort((a, b) => b.at - a.at || (a.post.id < b.post.id ? 1 : -1));
  }

  // ------------------------------------------------------------------ follows

  follow(s: FixtureStore, who: string, repoKey: string): void {
    if (!this.follows.some((f) => f.who === who && f.repo === repoKey)) this.follows.push({ who, repo: repoKey, at: s.now() });
  }

  // ------------------------------------------------------------------ feed trigger

  triggerView(s: FixtureStore, t: TriggerRow): T.AgentFeedTrigger {
    const since = s.now() - DAY;
    const today = this.runs.filter((r) => r.repo === t.repo && r.agent === t.agent && r.startedAt > since);
    return {
      repo: this.repoRef(s, t.repo),
      agent: s.p(t.agent),
      enabled: t.enabled,
      dailyUsdBudget: t.dailyUsdBudget,
      maxRunsPerDay: t.maxRunsPerDay,
      sources: t.sources.map((k) => this.repoRef(s, k)),
      checkIntervalSeconds: "300",
      runsToday: today.length,
      spentUsdToday: Math.round(today.reduce((n, r) => n + r.usdMicros, 0) / 10_000) / 100,
      createdAt: s.iso(t.createdAt),
      updatedAt: s.iso(t.updatedAt),
    };
  }

  // ------------------------------------------------------------------ the demo world

  seed(s: FixtureStore): void {
    const crSeed = (key: string) => CRS.find((c) => c.key === key);
    const landedAt = (key: string): number => {
      const seed = crSeed(key);
      const repo = s.repos.get(seed?.repo ?? "");
      const commit = seed?.landedAs !== undefined ? repo?.main[seed.landedAs] : undefined;
      if (!commit) throw new Error("post seed: " + key + " is not a landed change request");
      return commit.time;
    };
    for (const sp of POST_SEEDS) {
      const cr = s.crByKey(sp.cr);
      const landed = sp.state === "draft" ? undefined : landedAt(sp.cr);
      const createdAt = landed === undefined ? cr.createdAt + 20 * MIN : Math.max(cr.createdAt + MIN, landed - 30 * MIN);
      const id = ulid(landed ?? createdAt, "post:" + sp.cr);
      const publishedAt = sp.state === "published" || sp.state === "withdrawn" ? (landed as number) + (sp.after ?? 0) * MIN : undefined;
      const row: PostRow = {
        id,
        repo: cr.repo,
        crId: cr.id,
        author: sp.by,
        text: sp.text,
        mediaIds: [],
        state: sp.state === "published" ? PUBLISHED : sp.state === "pending" ? PENDING : sp.state === "withdrawn" ? WITHDRAWN : DRAFT,
        createdAt,
        updatedAt: sp.state === "withdrawn" ? (publishedAt as number) + 90 * MIN : (publishedAt ?? landed ?? createdAt),
        publishedAt,
        approvedBy: sp.approvedBy,
        version: 1 + sp.media.length,
        withdrawnReason: sp.withdrawnReason ?? "",
        comments: sp.comments ?? 0,
      };
      sp.media.forEach((key, i) => {
        const f = MEDIA_FILES[key];
        if (!f) throw new Error("post seed: no media " + key);
        const mid = ulid(createdAt, "media:" + sp.cr + ":" + i);
        this.media.set(mid, {
          id: mid,
          repo: cr.repo,
          crId: cr.id,
          by: sp.by,
          kind: f.kind === "image" ? "POST_MEDIA_KIND_IMAGE" : "POST_MEDIA_KIND_VIDEO",
          contentType: f.contentType,
          sizeBytes: f.sizeBytes,
          width: f.width,
          height: f.height,
          durationMs: f.durationMs ?? 0,
          altText: MEDIA_ALT[key] ?? "",
          sha256: f.sha256,
          file: f.file,
          finalized: true,
          attachedTo: id,
          createdAt,
        });
        row.mediaIds.push(mid);
      });
      this.posts.push(row);
    }

    // Posts of their own: no change request carries them.
    for (const sp of OWN_POST_SEEDS) {
      const publishedAt = s.ago(sp.ago);
      const createdAt = publishedAt - 20 * MIN;
      const id = ulid(publishedAt, "post:own:" + sp.key);
      const row: PostRow = {
        id,
        repo: sp.repo,
        crId: "",
        author: sp.by,
        text: sp.text,
        bodyMarkdown: sp.body,
        article: sp.article,
        attachments: sp.changes.map((key) => ({ kind: A_CHANGE, id: s.crByKey(key).id })),
        mediaIds: [],
        state: PUBLISHED,
        createdAt,
        updatedAt: publishedAt,
        publishedAt,
        approvedBy: sp.approvedBy,
        version: 1,
        withdrawnReason: "",
        comments: 0,
      };
      sp.media.forEach((key, i) => {
        const f = MEDIA_FILES[key];
        if (!f) throw new Error("post seed: no media " + key);
        const mid = ulid(createdAt, "media:own:" + sp.key + ":" + i);
        this.media.set(mid, {
          id: mid,
          repo: sp.repo,
          crId: "",
          by: sp.by,
          kind: f.kind === "image" ? "POST_MEDIA_KIND_IMAGE" : "POST_MEDIA_KIND_VIDEO",
          contentType: f.contentType,
          sizeBytes: f.sizeBytes,
          width: f.width,
          height: f.height,
          durationMs: f.durationMs ?? 0,
          altText: MEDIA_ALT[key] ?? "",
          sha256: f.sha256,
          file: f.file,
          finalized: true,
          attachedTo: id,
          createdAt,
        });
        row.mediaIds.push(mid);
      });
      this.posts.push(row);
    }

    const postOf = (key: string): PostRow => {
      const p = this.forChange(s.crByKey(key).id);
      if (!p) throw new Error("post seed: no post on " + key);
      return p;
    };
    const clamp = (at: number) => Math.min(at, s.anchor - 1_000);
    for (const l of LIKES) {
      const p = postOf(l.post);
      this.likes.push({ post: p.id, who: l.who, at: clamp((p.publishedAt ?? p.createdAt) + l.after * MIN) });
    }
    for (const r of REPOSTS) {
      const p = postOf(r.post);
      this.reposts.push({ post: p.id, who: r.who, at: clamp((p.publishedAt ?? p.createdAt) + r.after * MIN) });
    }
    for (const f of REPO_FOLLOWS) this.follows.push({ who: f.who, repo: f.repo, at: s.ago(f.ago) });

    // Adoptions are real issues in the adopting repos, so their pages open.
    for (const a of ADOPTIONS) {
      const repo = s.repos.get(a.repo) as RepoState;
      const number = Math.max(0, ...s.issues.filter((i) => i.repoId === repo.repo.id).map((i) => i.number)) + 1;
      const post = postOf(a.post);
      const at = s.ago(a.ago);
      const issue: IssueRow = {
        id: ulid(at, "issue:adopt:" + a.repo + ":" + number),
        repoId: repo.repo.id,
        number,
        title: a.title,
        body: a.body,
        state: "open",
        type: "feature",
        labels: a.labels,
        milestone: "",
        assignees: [],
        author: s.p(a.by),
        parentNumber: 0,
        subIssues: [],
        linkedChanges: [],
        comments: [],
        createdAt: s.iso(at),
        inspiredByPostId: post.id,
      };
      s.issues.push(issue);
      this.adoptions.push({ post: post.id, repo: a.repo, kind: "issue", ref: String(number), title: a.title, by: a.by, at });
    }

    for (const t of TRIGGERS) {
      this.triggers.push({ repo: t.repo, agent: t.agent, enabled: t.enabled, dailyUsdBudget: t.dailyUsdBudget, maxRunsPerDay: t.maxRunsPerDay, sources: [...t.sources], createdAt: s.ago(4_000), updatedAt: s.ago(3_000) });
    }
    for (const r of ADOPT_RUNS) {
      const at = s.ago(r.ago);
      const issue = s.issues.find((i) => i.inspiredByPostId === postOf(r.post).id && i.author?.id === s.p(r.agent).id);
      this.runs.push({
        id: ulid(at, "adopt-run:" + r.key),
        repo: r.repo,
        agent: r.agent,
        post: postOf(r.post).id,
        startedAt: at,
        finishedAt: at + r.tookMin * MIN,
        summary: r.notRelevant ? "not relevant: " + r.notRelevant : "opened issue #" + (issue?.number ?? 0) + ": " + (issue?.title ?? ""),
        usdMicros: r.costCents * 10_000,
      });
    }
  }

  /** Stores the bytes a client PUT to a media's upload URL, as the server's asset route does. */
  receive(mediaId: string, bytes: Uint8Array): void {
    const m = this.media.get(mediaId);
    if (!m) throw new BeeError("not_found", { reason: "MEDIA_NOT_FOUND", message: "no pending upload " + mediaId });
    if (m.finalized) throw invalidState("this upload is already finalized");
    const kind = sniff(bytes);
    if (!kind || kind !== m.contentType) throw invalid("the bytes are not " + m.contentType, "bad_media");
    if (bytes.length !== m.sizeBytes) throw invalid("the upload is " + bytes.length + " bytes, not the " + m.sizeBytes + " announced", "bad_media");
    m.bytes = bytes;
  }
}

// ------------------------------------------------------------------ helpers shared by handlers

function changeFor(s: FixtureStore, repo: RepoState, id: string | undefined) {
  return s.cr(repo, id ?? "");
}

function mediaFor(s: FixtureStore, id: string): MediaRow {
  const m = s.posts.media.get(id);
  if (!m) throw invalid("no media " + id, "bad_media");
  return m;
}

function readable(s: FixtureStore, viewer: string | undefined, id: string | undefined): PostRow {
  const p = s.posts.byId(id);
  if (!p || !s.posts.visible(s, p, viewer)) throw notFound(id ?? "");
  return p;
}

function engage(s: FixtureStore, rows: Engagement[], kind: string, ctx: Ctx, postId: string | undefined, on: boolean): T.Post {
  const who = me(s, ctx);
  const p = readable(s, who, postId);
  if (p.state !== PUBLISHED) throw invalidState("only a published post can be " + kind);
  const has = rows.some((r) => r.post === p.id && r.who === who);
  if (on && !has) {
    rows.push({ post: p.id, who, at: s.now() });
    s.posts.announce(s, s.repos.get(p.repo) as RepoState, "post." + kind, who, p);
  }
  if (!on && has) {
    const i = rows.findIndex((r) => r.post === p.id && r.who === who);
    rows.splice(i, 1);
  }
  return s.posts.postView(s, p, who);
}

const postService = defineService(PostService, {
  createPost: (s, req, ctx) => {
    const actor = me(s, ctx);
    const repo = s.repo(req.repo);
    if (!s.posts.canWrite(s, actor, repo)) throw denied("PERMISSION_DENIED", "writing a post needs write access to " + repo.repo.fullName);
    const ids = req.mediaIds ?? [];
    if (new Set(ids).size !== ids.length) throw invalid("a media is listed twice", "bad_media");
    const media = ids.map((id) => mediaFor(s, id));
    for (const m of media) {
      if (m.crId !== "" || m.by !== actor || m.repo !== repo.name || !m.finalized || m.attachedTo) throw invalid("media " + m.id + " is not ready for this post", "bad_media");
    }
    const words = s.posts.words(req.bodyMarkdown, req.article as T.PostArticle | undefined);
    const text = s.posts.validate(req.text ?? "", media, !!words.bodyMarkdown || !!words.article);
    const attachments = s.posts.resolve(s, repo.name, (req.attachments ?? []) as T.PostAttachment[], "");
    const now = s.now();
    // A person's post goes out at once; an agent's waits for a person, as on a change request.
    const human = s.kindOf(actor) === "human";
    const row: PostRow = {
      id: ulid(now, "post:own:new:" + repo.name + ":" + s.posts.posts.length),
      repo: repo.name,
      crId: "",
      author: actor,
      text,
      ...words,
      attachments,
      mediaIds: ids,
      state: human ? PUBLISHED : PENDING,
      createdAt: now,
      updatedAt: now,
      publishedAt: human ? now : undefined,
      version: 1,
      withdrawnReason: "",
      comments: 0,
    };
    for (const m of media) m.attachedTo = row.id;
    s.posts.posts.push(row);
    s.posts.announce(s, repo, human ? "post.published" : "post.approval_requested", actor, row);
    return s.posts.postView(s, row, actor);
  },
  setChangePost: (s, req, ctx) => {
    const actor = me(s, ctx);
    const repo = s.repo(req.repo);
    const cr = changeFor(s, repo, req.changeId);
    if (!s.posts.canEditChange(s, actor, repo, cr.author)) throw denied("PERMISSION_DENIED", "only the author of the change, or someone with write access, can write its post");
    const existing = s.posts.forChange(cr.id);
    if (existing && existing.state !== DRAFT && existing.state !== PENDING) throw invalidState("this post is " + existing.state.replace("POST_STATE_", "").toLowerCase() + " and can no longer be edited");
    if (!existing && cr.state !== "CHANGE_STATE_OPEN") throw invalidState("a post is written while the change is open");
    // An unset uint64 reads as "0", which means "do not check".
    if (req.expectedVersion && req.expectedVersion !== "0" && req.expectedVersion !== String(existing?.version ?? 0)) {
      throw new BeeError("failed_precondition", { reason: "VERSION_MISMATCH", message: "the post is at version " + (existing?.version ?? 0) + ", not " + req.expectedVersion });
    }
    const ids = req.mediaIds ?? [];
    if (new Set(ids).size !== ids.length) throw invalid("a media is listed twice", "bad_media");
    const media = ids.map((id) => mediaFor(s, id));
    for (const m of media) {
      if (m.crId !== cr.id || !m.finalized || (m.attachedTo && m.attachedTo !== existing?.id)) throw invalid("media " + m.id + " is not ready for this post", "bad_media");
    }
    const words = s.posts.words(req.bodyMarkdown, req.article as T.PostArticle | undefined);
    const text = s.posts.validate(req.text ?? "", media, !!words.bodyMarkdown || !!words.article);
    const attachments = s.posts.resolve(s, repo.name, (req.attachments ?? []) as T.PostAttachment[], cr.id);
    const now = s.now();
    const row: PostRow = existing ?? { id: ulid(now, "post:new:" + cr.id), repo: cr.repo, crId: cr.id, author: actor, text: "", mediaIds: [], state: DRAFT, createdAt: now, updatedAt: now, version: 0, withdrawnReason: "", comments: 0 };
    for (const id of row.mediaIds) if (!ids.includes(id)) delete (s.posts.media.get(id) as MediaRow).attachedTo;
    for (const m of media) m.attachedTo = row.id;
    // Whoever last sets the text is the author; a person's sign-off covered the text they read.
    if (row.text !== text || row.mediaIds.join() !== ids.join()) row.approvedBy = undefined;
    row.author = actor;
    row.text = text;
    row.bodyMarkdown = words.bodyMarkdown;
    row.article = words.article;
    row.attachments = attachments;
    row.mediaIds = ids;
    row.version += 1;
    row.updatedAt = now;
    if (!existing) {
      s.posts.posts.push(row);
      s.posts.announce(s, repo, "post.drafted", actor, row);
    }
    return s.posts.postView(s, row, actor);
  },
  clearChangePost: (s, req, ctx) => {
    const actor = me(s, ctx);
    const repo = s.repo(req.repo);
    const cr = changeFor(s, repo, req.changeId);
    if (!s.posts.canEditChange(s, actor, repo, cr.author)) throw denied("PERMISSION_DENIED", "only the author of the change, or someone with write access, can clear its post");
    const p = s.posts.forChange(cr.id);
    if (!p) return {};
    if (p.state !== DRAFT && p.state !== PENDING) throw invalidState("this post is " + p.state.replace("POST_STATE_", "").toLowerCase() + " and can no longer be cleared");
    for (const id of p.mediaIds) s.posts.media.delete(id);
    s.posts.posts = s.posts.posts.filter((x) => x !== p);
    return {};
  },
  getChangePost: (s, req, ctx) => {
    const viewer = s.viewer(ctx.token);
    const repo = s.repo(req.repo);
    const cr = changeFor(s, repo, req.changeId);
    const p = s.posts.forChange(cr.id);
    if (!p || !s.posts.visible(s, p, viewer)) throw notFound("on change " + cr.id);
    return s.posts.postView(s, p, viewer);
  },
  beginPostMediaUpload: (s, req, ctx) => {
    const actor = me(s, ctx);
    const repo = s.repo(req.repo);
    // Without a change request the upload is for a post of its own and belongs to its uploader.
    const cr = req.changeId ? changeFor(s, repo, req.changeId) : undefined;
    if (cr) {
      if (!s.posts.canEditChange(s, actor, repo, cr.author)) throw denied("PERMISSION_DENIED", "only the author of the change, or someone with write access, can add media to its post");
      const existing = s.posts.forChange(cr.id);
      if (existing && existing.state !== DRAFT && existing.state !== PENDING) throw invalidState("this post can no longer be edited");
      if (!existing && cr.state !== "CHANGE_STATE_OPEN") throw invalidState("a post is written while the change is open");
    } else if (!s.posts.canWrite(s, actor, repo)) {
      throw denied("PERMISSION_DENIED", "adding media to a post needs write access to " + repo.repo.fullName);
    }
    const kind = req.kind;
    const type = req.contentType ?? "";
    const size = Number(req.sizeBytes ?? 0);
    if (kind !== "POST_MEDIA_KIND_IMAGE" && kind !== "POST_MEDIA_KIND_VIDEO") throw invalid("say whether the media is an image or a video", "bad_media");
    if (!(kind === "POST_MEDIA_KIND_IMAGE" ? IMAGE_TYPES : VIDEO_TYPES).includes(type)) throw invalid(type + " is not an accepted " + (kind === "POST_MEDIA_KIND_IMAGE" ? "image" : "video") + " type", "bad_media");
    if (!(size > 0)) throw invalid("say how many bytes you will upload", "bad_media");
    if (size > (kind === "POST_MEDIA_KIND_IMAGE" ? MAX_IMAGE_BYTES : MAX_VIDEO_BYTES)) throw new BeeError("resource_exhausted", { reason: "TOO_LARGE", message: "an image is at most 8 MiB and a video 64 MiB" });
    const now = s.now();
    const id = ulid(now, "media:new:" + (cr?.id ?? "own:" + actor) + ":" + s.posts.media.size);
    s.posts.media.set(id, { id, repo: repo.name, crId: cr?.id ?? "", by: actor, kind, contentType: type, sizeBytes: size, width: 0, height: 0, durationMs: 0, altText: "", sha256: "", finalized: false, createdAt: now });
    return { mediaId: id, uploadUrl: "/api/fixture/posts/" + repo.repo.id + "/" + id, uploadHeaders: { "content-type": type }, expiresAt: s.iso(now + UPLOAD_TTL) };
  },
  finalizePostMedia: (s, req, ctx) => {
    const actor = me(s, ctx);
    const repo = s.repo(req.repo);
    const m = mediaFor(s, req.mediaId ?? "");
    const cr = s.crs.find((c) => c.id === m.crId);
    const mine = m.crId === "" ? m.by === actor && s.posts.canWrite(s, actor, repo) : !!cr && s.posts.canEditChange(s, actor, repo, cr.author);
    if (m.repo !== repo.name || !mine) throw notFound(req.mediaId ?? "");
    if (m.finalized) throw invalidState("this media is already finalized");
    if (s.now() > m.createdAt + UPLOAD_TTL) throw invalidState("this upload expired after 15 minutes");
    if (!/^[0-9a-f]{64}$/.test(req.sha256 ?? "")) throw invalid("sha256 is 64 lower-case hex digits", "bad_media");
    if (m.bytes) {
      const got = createHash("sha256").update(m.bytes).digest("hex");
      if (got !== req.sha256) throw new BeeError("data_loss", { reason: "INTEGRITY", message: "the uploaded bytes hash to " + got + ", not " + req.sha256 });
    }
    const alt = (req.altText ?? "").trim();
    if (m.kind === "POST_MEDIA_KIND_IMAGE" && (alt.length < 1 || Array.from(alt).length > 1000)) throw invalid("an image needs alt text of 1 to 1000 characters", "alt_text_required");
    if (Array.from(alt).length > 1000) throw invalid("alt text is at most 1000 characters", "bad_media");
    m.sha256 = req.sha256 as string;
    m.altText = alt;
    m.width = req.width ?? 0;
    m.height = req.height ?? 0;
    m.durationMs = m.kind === "POST_MEDIA_KIND_VIDEO" ? (req.durationMs ?? 0) : 0;
    m.finalized = true;
    return s.posts.mediaView(m);
  },
  approvePost: (s, req, ctx) => {
    const actor = me(s, ctx);
    const p = readable(s, actor, req.postId);
    const repo = s.repos.get(p.repo) as RepoState;
    if (p.state !== DRAFT && p.state !== PENDING) throw invalidState("only a draft or a post waiting for approval can be approved");
    if (s.kindOf(p.author) === "human") throw invalidState("only a post written by an agent needs approval");
    if (s.kindOf(actor) !== "human" || !s.posts.canWrite(s, actor, repo)) throw denied("PERMISSION_DENIED", "approving needs a person with write access to " + repo.repo.fullName);
    if (actor === p.author) throw denied("PERMISSION_DENIED", "a post cannot be approved by its author");
    const now = s.now();
    p.approvedBy = actor;
    p.updatedAt = now;
    p.version += 1;
    if (p.state === PENDING) {
      p.state = PUBLISHED;
      p.publishedAt = now;
      s.posts.announce(s, repo, "post.published", p.author, p);
    }
    return s.posts.postView(s, p, actor);
  },
  withdrawPost: (s, req, ctx) => {
    const actor = me(s, ctx);
    const p = readable(s, actor, req.postId);
    const repo = s.repos.get(p.repo) as RepoState;
    if (actor !== p.author && !s.posts.isAdmin(s, actor, repo)) throw denied("PERMISSION_DENIED", "only the author or an admin of " + repo.repo.fullName + " can withdraw a post");
    if (p.state === WITHDRAWN || p.state === DISCARDED) throw invalidState("this post is already " + p.state.replace("POST_STATE_", "").toLowerCase());
    p.state = WITHDRAWN;
    p.withdrawnReason = (req.reason ?? "").trim();
    p.updatedAt = s.now();
    p.version += 1;
    s.posts.announce(s, repo, "post.withdrawn", actor, p);
    return s.posts.postView(s, p, actor);
  },
  getPost: (s, req, ctx) => {
    const viewer = s.viewer(ctx.token);
    return s.posts.postView(s, readable(s, viewer, req.postId), viewer);
  },
  listRepoPosts: (s, req, ctx) => {
    const viewer = s.viewer(ctx.token);
    const repo = s.repo(req.repo);
    const rows = s.posts.posts
      .filter((p) => p.repo === repo.name && p.state === PUBLISHED && s.posts.visible(s, p, viewer))
      .sort((a, b) => (b.publishedAt ?? 0) - (a.publishedAt ?? 0) || (a.id < b.id ? 1 : -1));
    const pg = page(rows, req.page, 20);
    return { posts: pg.items.map((p) => s.posts.postView(s, p, viewer)), page: { nextPageToken: pg.next } };
  },
  likePost: (s, req, ctx) => engage(s, s.posts.likes, "liked", ctx, req.postId, true),
  unlikePost: (s, req, ctx) => engage(s, s.posts.likes, "liked", ctx, req.postId, false),
  repostPost: (s, req, ctx) => engage(s, s.posts.reposts, "reposted", ctx, req.postId, true),
  undoRepost: (s, req, ctx) => engage(s, s.posts.reposts, "reposted", ctx, req.postId, false),
  listFollowingFeed: (s, req, ctx) => {
    const viewer = me(s, ctx);
    const all = s.posts.feed(s, viewer);
    const size = Math.max(1, Math.min(100, req.page?.pageSize || 20));
    const item = (e: (typeof all)[number]) => ({ post: s.posts.postView(s, e.post, viewer), repostedBy: e.repostedBy ? s.p(e.repostedBy) : undefined, at: s.iso(e.at) });
    const after = parseCursor(req.after);
    if (after) {
      // Poll mode: everything newer than the cursor, oldest first.
      const newer = all.filter((e) => e.at > after.at || (e.at === after.at && e.post.id > after.id)).reverse();
      const out = newer.slice(0, size);
      const last = out[out.length - 1];
      return {
        items: out.map(item),
        page: { nextPageToken: out.length < newer.length && last ? cursorOf(last.at, last.post.id) : "" },
        newestCursor: last ? cursorOf(last.at, last.post.id) : (req.after ?? ""),
      };
    }
    const from = parseCursor(req.page?.pageToken);
    const rest = from ? all.filter((e) => e.at < from.at || (e.at === from.at && e.post.id < from.id)) : all;
    const out = rest.slice(0, size);
    const last = out[out.length - 1];
    const first = all[0];
    return {
      items: out.map(item),
      page: { nextPageToken: out.length < rest.length && last ? cursorOf(last.at, last.post.id) : "" },
      newestCursor: first ? cursorOf(first.at, first.post.id) : "",
    };
  },
  listPostAdoptions: (s, req, ctx) => {
    const viewer = s.viewer(ctx.token);
    const p = readable(s, viewer, req.postId);
    const rows = s.posts.adoptionsFor(s, p.id, viewer);
    const pg = page(rows, req.page, 50);
    return {
      adoptions: pg.items.map((a) => ({
        repo: s.posts.repoRef(s, a.repo),
        subject: { kind: a.kind === "issue" ? ("SUBJECT_KIND_ISSUE" as const) : ("SUBJECT_KIND_CHANGE_REQUEST" as const), repo: s.posts.repoRef(s, a.repo), id: a.ref },
        title: a.title,
        author: s.p(a.by),
        createdAt: s.iso(a.at),
      })),
      page: { nextPageToken: pg.next },
    };
  },
});

export const POST_SERVICES = [postService];

// ------------------------------------------------------------------ following repos (UserService)

export const repoFollowImpl: Pick<Impl<typeof UserService>, "followRepo" | "unfollowRepo" | "isFollowingRepo" | "listFollowedRepos" | "listRepoFollowers"> = {
  followRepo: (s, req, ctx) => {
    s.posts.follow(s, me(s, ctx), s.repo(req.repo).name);
    return {};
  },
  unfollowRepo: (s, req, ctx) => {
    const who = me(s, ctx);
    const key = s.repo(req.repo).name;
    s.posts.follows = s.posts.follows.filter((f) => !(f.who === who && f.repo === key));
    return {};
  },
  isFollowingRepo: (s, req, ctx) => {
    const who = me(s, ctx);
    const key = s.repo(req.repo).name;
    return { following: s.posts.follows.some((f) => f.who === who && f.repo === key) };
  },
  listFollowedRepos: (s, req, ctx) => {
    const viewer = s.viewer(ctx.token);
    const handle = req.user ? s.handleByLogin.get(lower(req.user)) : viewer;
    if (!handle) throw new BeeError("not_found", { reason: "USER_NOT_FOUND", message: "no user or agent named " + (req.user ?? "") });
    const rows = s.posts.follows
      .filter((f) => f.who === handle)
      .map((f) => ({ f, repo: s.repos.get(f.repo) }))
      .filter((x): x is { f: FollowRow; repo: RepoState } => !!x.repo && s.canRead(viewer, x.repo))
      .sort((a, b) => b.f.at - a.f.at || a.repo.repo.fullName.localeCompare(b.repo.repo.fullName));
    const pg = page(rows, req.page, 50);
    return { repos: pg.items.map((x) => s.repoView(x.repo)), page: { nextPageToken: pg.next } };
  },
  listRepoFollowers: (s, req) => {
    const repo = s.repo(req.repo);
    const rows = s.posts.follows.filter((f) => f.repo === repo.name).sort((a, b) => b.at - a.at || a.who.localeCompare(b.who));
    const pg = page(rows, req.page, 50);
    return { followers: pg.items.map((f) => s.p(f.who)), page: { nextPageToken: pg.next } };
  },
};

// ------------------------------------------------------------------ the feed trigger (AgentService)

function maintainer(s: FixtureStore, who: string, repo: RepoState): void {
  if (!s.posts.isAdmin(s, who, repo)) throw denied("PERMISSION_DENIED", "only a maintainer of " + repo.repo.fullName + " can change its feed triggers");
}

function triggerOf(s: FixtureStore, repo: RepoState, agentId: string | undefined): TriggerRow | undefined {
  const handle = s.handleOf(agentId ?? "");
  return s.posts.triggers.find((t) => t.repo === repo.name && t.agent === handle);
}

export function feedAgentRun(s: FixtureStore, r: AdoptRunRow): AgentRunJson {
  const repo = s.repos.get(r.repo) as RepoState;
  return {
    id: r.id,
    repo: { fullName: repo.repo.fullName },
    task: { kind: "AGENT_TASK_KIND_ADOPT_EVALUATE", postId: r.post },
    agent: s.p(r.agent),
    startedBy: s.p(r.agent),
    state: "AGENT_RUN_STATE_SUCCEEDED",
    summary: r.summary,
    cost: { inputTokens: String(Math.round(r.usdMicros / 8)), outputTokens: String(Math.round(r.usdMicros / 40)), usdMicros: String(r.usdMicros) },
    createdAt: s.iso(r.startedAt),
    startedAt: s.iso(r.startedAt),
    finishedAt: s.iso(r.finishedAt),
    trigger: "feed",
    triggerRef: r.post,
  };
}

export const feedTriggerImpl: Pick<Impl<typeof AgentService>, "setAgentFeedTrigger" | "getAgentFeedTrigger" | "listAgentFeedTriggers"> = {
  setAgentFeedTrigger: (s, req, ctx) => {
    const actor = me(s, ctx);
    const repo = s.repo(req.repo);
    maintainer(s, actor, repo);
    const agent = s.handleOf(req.agent ?? "");
    if (s.kindOf(agent) !== "agent" || !s.principals.has(agent) || !s.inOrg(agent, repo.owner)) throw new BeeError("not_found", { reason: "AGENT_NOT_FOUND", message: "no agent " + (req.agent ?? "") + " in " + repo.owner });
    const budget = req.dailyUsdBudget === undefined || req.dailyUsdBudget === "NaN" ? 0 : Number(req.dailyUsdBudget);
    if (!(budget >= 0) || !Number.isFinite(budget)) throw invalid("the daily budget is zero or more dollars", "INVALID_ARGUMENT");
    const max = req.maxRunsPerDay ?? 0;
    if (max < 0) throw invalid("runs per day is zero or more", "INVALID_ARGUMENT");
    const sources: string[] = [];
    for (const ref of req.sources ?? []) {
      const src = s.repo(ref);
      if (!s.canRead(agent, src)) throw new BeeError("not_found", { reason: "REPO_NOT_FOUND", message: "the agent cannot read " + src.repo.fullName });
      if (!sources.includes(src.name)) sources.push(src.name);
    }
    const now = s.now();
    const prev = triggerOf(s, repo, req.agent);
    // The agent follows its sources; a source it no longer lists is unfollowed.
    for (const k of sources) s.posts.follow(s, agent, k);
    for (const k of prev?.sources ?? []) if (!sources.includes(k)) s.posts.follows = s.posts.follows.filter((f) => !(f.who === agent && f.repo === k));
    const row: TriggerRow = { repo: repo.name, agent, enabled: !!req.enabled, dailyUsdBudget: budget, maxRunsPerDay: max, sources, createdAt: prev?.createdAt ?? now, updatedAt: now };
    s.posts.triggers = s.posts.triggers.filter((t) => t !== prev).concat(row);
    return s.posts.triggerView(s, row);
  },
  getAgentFeedTrigger: (s, req, ctx) => {
    const actor = me(s, ctx);
    const repo = s.repo(req.repo);
    if (!s.posts.canWrite(s, actor, repo)) throw denied("PERMISSION_DENIED", "feed triggers are visible to members of " + repo.owner);
    const t = triggerOf(s, repo, req.agent);
    if (!t) throw new BeeError("not_found", { reason: "FEED_TRIGGER_NOT_FOUND", message: "no feed trigger for that agent in " + repo.repo.fullName });
    return s.posts.triggerView(s, t);
  },
  listAgentFeedTriggers: (s, req, ctx) => {
    const actor = me(s, ctx);
    const repo = s.repo(req.repo);
    if (!s.posts.canWrite(s, actor, repo)) throw denied("PERMISSION_DENIED", "feed triggers are visible to members of " + repo.owner);
    const rows = s.posts.triggers.filter((t) => t.repo === repo.name).sort((a, b) => a.agent.localeCompare(b.agent));
    const pg = page(rows, req.page, 50);
    return { triggers: pg.items.map((t) => s.posts.triggerView(s, t)), page: { nextPageToken: pg.next } };
  },
};

