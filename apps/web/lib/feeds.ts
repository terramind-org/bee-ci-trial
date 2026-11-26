/**
 * The data behind the Atom routes. Feeds are public: they are fetched without the visitor's
 * token, so a private repo can never appear in one and a feed reader sees what a signed-out
 * visitor sees. Read-only, from existing RPCs (Log, ListReleases, ListRepoPosts, SearchRepos,
 * GetProfile).
 */
import { objectIdText } from "./api/ids";
import { createClient } from "./api/client";
import { rpcFor } from "./api/server";
import { BeeError } from "./api/transport";
import type { Changeset, Post, Release } from "./api/types";
import type { AtomEntry, AtomFeed } from "./atom";
import { ownerHref, repoHref } from "./routes";

const LANDS_PER_REPO = 20;
const POSTS_PER_REPO = 20;
const REPOS_PER_OWNER = 10;

function client() {
  return createClient(rpcFor(undefined));
}

const notFound = (err: unknown) => err instanceof BeeError && (err.code === "not_found" || err.code === "permission_denied" || err.code === "unauthenticated");

function landEntry(fullName: string, c: Changeset): AtomEntry | null {
  const time = c.meta?.committerTime?.time ?? c.meta?.authorTime?.time;
  const id = objectIdText(c.id);
  if (!time || !id) return null;
  const [first = "", ...rest] = (c.meta?.message ?? "").split("\n");
  return {
    kind: "land",
    id: fullName + "/" + id,
    title: (first.trim() || "Merged commit") + " · " + fullName,
    updated: time,
    path: repoHref(fullName, "activity"),
    summary: rest.join("\n").trim() || undefined,
    author: c.meta?.author || undefined,
  };
}

function releaseEntry(fullName: string, r: Release): AtomEntry | null {
  if (!r.createdAt) return null;
  return {
    kind: "release",
    id: fullName + "/" + r.id,
    title: "Release " + (r.name || r.tag) + " · " + fullName,
    updated: r.createdAt,
    path: repoHref(fullName, "releases"),
    summary: r.notes || undefined,
    author: r.author?.display || undefined,
  };
}

/** A published post: its words are the summary, the change it came from is the title. */
export function postEntry(fullName: string, p: Post): AtomEntry | null {
  const time = p.publishedAt;
  if (p.state !== "POST_STATE_PUBLISHED" || !time || !p.id) return null;
  const title = p.changeTitle ? "Merged " + p.changeTitle : "Update";
  const media = p.media.length ? (p.media.length === 1 ? "1 attachment" : p.media.length + " attachments") : "";
  return {
    kind: "post",
    id: fullName + "/" + p.id,
    title: title + " · " + fullName,
    updated: time,
    path: repoHref(fullName, "posts/" + encodeURIComponent(p.id)),
    summary: [p.text.trim(), media ? "(" + media + ")" : ""].filter(Boolean).join("\n\n") || undefined,
    author: p.author?.display || undefined,
  };
}

/** Published posts; a server without PostService (or one that hides it from anonymous callers) has none. */
async function postsOf(api: ReturnType<typeof client>, fullName: string): Promise<AtomEntry[]> {
  try {
    return (await api.post.listRepoPosts({ repo: { fullName }, page: { pageSize: POSTS_PER_REPO } })).posts.flatMap((p) => postEntry(fullName, p) ?? []);
  } catch (err) {
    if (notFound(err) || (err instanceof BeeError && err.code === "unimplemented")) return [];
    throw err;
  }
}

async function landsOf(api: ReturnType<typeof client>, fullName: string, defaultRef: string, by?: string): Promise<AtomEntry[]> {
  try {
    const log = await api.query.log({ repo: { fullName }, from: { expr: defaultRef || "main" }, firstParent: true, page: { pageSize: LANDS_PER_REPO } });
    return log.changesets.filter((c) => !by || c.meta?.creator === by).flatMap((c) => landEntry(fullName, c) ?? []);
  } catch (err) {
    // A repo with nothing landed yet has no tip to read.
    if (err instanceof BeeError && (err.code === "failed_precondition" || notFound(err))) return [];
    throw err;
  }
}

/** Releases are optional: a server that does not offer them to anonymous callers just has none here. */
async function releasesOf(api: ReturnType<typeof client>, fullName: string): Promise<AtomEntry[]> {
  try {
    return (await api.release.listReleases({ repo: { fullName } })).releases.flatMap((r) => releaseEntry(fullName, r) ?? []);
  } catch (err) {
    if (notFound(err) || (err instanceof BeeError && err.code === "unimplemented")) return [];
    throw err;
  }
}

/** Lands, releases and published posts of one public repo; null when the repo is not public or does not exist. */
export async function repoFeed(origin: string, fullName: string): Promise<AtomFeed | null> {
  const api = client();
  let repo;
  try {
    repo = await api.repo.getRepo({ repo: { fullName } });
  } catch (err) {
    if (notFound(err)) return null;
    throw err;
  }
  const [lands, releases, posts] = await Promise.all([landsOf(api, fullName, repo.defaultRef), releasesOf(api, fullName), postsOf(api, fullName)]);
  return {
    origin,
    path: repoHref(fullName, "feed.atom"),
    pagePath: repoHref(fullName),
    title: fullName + " · Beehive",
    subtitle: repo.description || undefined,
    entries: [...lands, ...releases, ...posts],
  };
}

/** Recent lands on a user's, agent's or org's public repos (an org's repos, or a person's own lands). */
export async function ownerFeed(origin: string, owner: string): Promise<AtomFeed | null> {
  const api = client();
  let profile;
  try {
    profile = await api.user.getProfile({ owner });
  } catch (err) {
    if (notFound(err)) return null;
    throw err;
  }
  const qualifier = profile.kind === "org" ? "org:" : "user:";
  const found = await api.search.searchRepos({ query: qualifier + profile.login + " is:public sort:updated", sort: "updated", page: { pageSize: REPOS_PER_OWNER } });
  const by = profile.kind === "org" ? undefined : profile.principalId;
  const lands = await Promise.all(found.hits.map((h) => landsOf(api, h.fullName, "main", by)));
  return {
    origin,
    path: ownerHref(profile.login) + "/feed.atom",
    pagePath: ownerHref(profile.login),
    title: (profile.name || profile.login) + " · Beehive",
    subtitle: profile.kind === "org" ? "Recent merges on public repositories" : "Recent merges by " + profile.login,
    entries: lands.flat(),
  };
}
