/**
 * The pure parts of a post card: splitting a post's plain text into text, links, `#tags` and
 * `@mentions`, and the words around the author and the media. No React, no clock, so the feed,
 * the permalink, the composer's preview and the tests all read a post the same way.
 */
import type { Post, PostMedia, Principal } from "@/lib/api/types";
import { isReservedOwner, ownerHref, repoHref } from "@/lib/routes";

export type PostToken = { kind: "text"; text: string } | { kind: "url"; text: string; href: string } | { kind: "tag"; text: string; tag: string; href: string } | { kind: "mention"; text: string; login: string; href: string };

/*
 * One pass over the text. A URL is http(s) up to whitespace; a tag is `#` and a letter, not glued
 * to a word (so `a#b` and `&#38;` stay text); a mention is `@` and a login, not glued to a word or
 * a dot (so an email address stays text).
 */
const TOKEN = /(https?:\/\/[^\s<>"]+)|(?<![\w&#])#([A-Za-z][\w-]{0,49})|(?<![\w.@])@([A-Za-z0-9][\w.-]{0,38})/g;

/** Punctuation that usually ends a sentence rather than a link: dropped from a URL's end. */
const TRAILING = /[.,;:!?'")\]]+$/;

export function tokenize(text: string): PostToken[] {
  const out: PostToken[] = [];
  let last = 0;
  const push = (t: string) => {
    if (!t) return;
    const prev = out.at(-1);
    if (prev?.kind === "text") prev.text += t;
    else out.push({ kind: "text", text: t });
  };
  for (const m of text.matchAll(TOKEN)) {
    const at = m.index ?? 0;
    push(text.slice(last, at));
    let whole = m[0];
    if (m[1]) {
      // Keep a closing bracket that has its opening one inside the URL (wiki-style links).
      let url = whole.replace(TRAILING, "");
      if (whole.slice(url.length).startsWith(")") && url.includes("(")) url += ")";
      whole = url;
      out.push({ kind: "url", text: url, href: url });
    } else if (m[2]) {
      out.push({ kind: "tag", text: whole, tag: m[2], href: "/search?q=" + encodeURIComponent("#" + m[2]) });
    } else if (m[3]) {
      const login = m[3].replace(/[.-]+$/, "");
      whole = "@" + login;
      if (isReservedOwner(login)) push(whole);
      else out.push({ kind: "mention", text: whole, login, href: ownerHref(login) });
    }
    last = at + whole.length;
  }
  push(text.slice(last));
  return out;
}

const RUNTIMES: Record<string, string> = {
  "claude-code": "Claude Code",
  codex: "Codex",
  "codex-cli": "Codex",
  cursor: "Cursor",
  aider: "Aider",
  devin: "Devin",
  "gemini-cli": "Gemini CLI",
};

/** "claude-code" → "Claude Code"; an unknown runtime is title-cased from its words. */
export function runtimeName(runtime: string): string {
  const known = RUNTIMES[runtime];
  if (known) return known;
  return runtime
    .split(/[-_\s]+/)
    .filter(Boolean)
    .map((w) => w.charAt(0).toUpperCase() + w.slice(1))
    .join(" ");
}

/** "via Claude Code" for an agent with a runtime, "via an agent" without one; empty for people. */
export function viaLine(p: Principal | undefined): string {
  if (p?.kind !== "PRINCIPAL_KIND_AGENT") return "";
  return p.runtime ? "via " + runtimeName(p.runtime) : "via an agent";
}

/** The repo's full name a post belongs to. */
export function postRepo(post: Pick<Post, "repo">): string {
  return post.repo?.fullName ?? "";
}

/** `/{owner}/{repo}/posts/{id}`: the permalink. */
export function postHref(post: Pick<Post, "repo" | "id">): string {
  return repoHref(postRepo(post), "posts/" + encodeURIComponent(post.id));
}

/**
 * `/{owner}/{repo}/posts/{id}/media/{mediaId}`: redirects to a fresh signed URL, so an `<img>`, a
 * shared link or an OpenGraph image never points at an address that expires.
 */
export function mediaHref(post: Pick<Post, "repo" | "id">, media: Pick<PostMedia, "id">): string {
  return postHref(post) + "/media/" + encodeURIComponent(media.id);
}

/** The change request a post is about. */
export function changeHref(post: Pick<Post, "repo" | "changeId">): string {
  return repoHref(postRepo(post), "changes/" + encodeURIComponent(post.changeId));
}

/** The layout a media grid takes for its count: one, two, three or four. */
export type MediaLayout = "one" | "two" | "three" | "four";

export function mediaLayout(n: number): MediaLayout {
  return n <= 1 ? "one" : n === 2 ? "two" : n === 3 ? "three" : "four";
}

/** Counts as numbers; a missing counts message is all zeros. */
export function countsOf(post: Pick<Post, "counts">): { likes: number; reposts: number; comments: number; adoptions: number } {
  const c = post.counts;
  return { likes: Number(c?.likes ?? 0), reposts: Number(c?.reposts ?? 0), comments: Number(c?.comments ?? 0), adoptions: Number(c?.adoptions ?? 0) };
}

/** The login a principal's display name is, when it is one (agents' always are). */
export function loginOf(p: Principal | undefined): string | null {
  const d = p?.display ?? "";
  return /^[\w.-]+$/.test(d) && !isReservedOwner(d) ? d : null;
}

/** The one-line summary used for the page title and OpenGraph: "<repo>: <change title>". */
export function postTitle(post: Pick<Post, "repo" | "changeTitle" | "article">): string {
  return postRepo(post) + ": " + (post.article?.title || post.changeTitle || "Post");
}

/** Short enough for a link preview: the first 200 characters, cut at a word. */
export function describePost(text: string): string {
  const t = text.replace(/\s+/g, " ").trim();
  if (t.length <= 200) return t;
  const cut = t.slice(0, 200);
  return cut.slice(0, Math.max(cut.lastIndexOf(" "), 160)).trimEnd() + "…";
}
