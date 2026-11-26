/**
 * Every URL the web app links to. Repos live at `/{owner}/{repo}/…`, org settings at
 * `/orgs/{org}/settings/…`; global pages own the first path segments in RESERVED_OWNERS, which
 * the `[owner]` route never matches. Shared by server components, client components and
 * `proxy.ts` (legacy redirects), so nothing here touches Next or the API.
 */

/** First path segments that are never an owner (design.md → Web routes). */
export const RESERVED_OWNERS: readonly string[] = [
  "api",
  "admin",
  "explore",
  "trending",
  "topics",
  "leaderboards",
  "repositories",
  "pricing",
  "new",
  "prompts",
  "signin",
  "orgs",
  "import",
  "search",
  "settings",
  "llms.txt",
  "manifest.webmanifest",
  "icon.svg",
  "_next",
  "feed.atom",
  "feed",
];

const RESERVED = new Set(RESERVED_OWNERS.map((s) => s.toLowerCase()));

/** Owner and repo names: letters, digits, `_`, `.` and `-` (the server's name rule). */
const NAME = /^[\w.-]+$/;

export function isReservedOwner(segment: string): boolean {
  return RESERVED.has(segment.toLowerCase());
}

/** A well-formed `owner/repo` whose owner is not a reserved segment. */
export function isRepoFullName(fullName: string): boolean {
  const [owner, repo, ...rest] = fullName.split("/");
  return !rest.length && !!owner && !!repo && NAME.test(owner) && NAME.test(repo) && !isReservedOwner(owner);
}

function join(base: string, sub?: string): string {
  const s = (sub ?? "").replace(/^\/+/, "");
  if (!s) return base;
  // A query or fragment hangs off the base itself; a path adds a segment.
  return /^[?#]/.test(s) ? base + s : base + "/" + s;
}

/** `/{owner}/{repo}`, or a page under it: `repoHref("acme/api", "changes/01H…")`. */
export function repoHref(fullName: string, sub?: string): string {
  return join("/" + fullName.replace(/^\/+|\/+$/g, ""), sub);
}

/** A user's, agent's or org's profile. */
export function ownerHref(owner: string): string {
  return "/" + owner;
}

/** `/orgs/{org}/settings`, or a section of it: `orgSettingsHref("acme", "members")`. */
export function orgSettingsHref(org: string, sub?: string): string {
  return join("/orgs/" + org + "/settings", sub);
}

/** The repo a pathname is under (`/acme/api/changes` → `acme/api`), or null for global pages. */
export function repoFromPath(pathname: string): string | null {
  const [owner, repo] = pathname.split("/").filter(Boolean);
  if (!owner || !repo) return null;
  const full = owner + "/" + repo;
  return isRepoFullName(full) ? full : null;
}

/** The path below the repo (`/acme/api/changes/1` → `changes/1`), or "" at the repo home. */
export function repoSubPath(pathname: string): string {
  return pathname.split("/").filter(Boolean).slice(2).join("/");
}

/** Sections that used to live at the root and are now repo-scoped. */
export const LEGACY_REPO_SECTIONS: readonly string[] = ["activity", "changes", "code", "agents", "runs", "work", "releases", "packages", "pages", "security"];

/**
 * Where a pre-`/{owner}/{repo}` URL now lives, or null when the path is not a legacy one. `repo`
 * is the repo picker's cookie; without a usable one the visitor picks a repo from the list.
 * `search` is the raw query string including its `?` (or "").
 */
export function legacyRedirect(pathname: string, search: string, repo: string | undefined): string | null {
  const segs = pathname.split("/").filter(Boolean);
  const first = segs[0];
  if (!first) return null;
  const known = repo && isRepoFullName(repo) ? repo : null;
  if (LEGACY_REPO_SECTIONS.includes(first)) {
    if (!known) return "/repositories";
    return repoHref(known, segs.join("/")) + search;
  }
  if (first === "settings") {
    if (!known) return "/repositories";
    return orgSettingsHref(known.split("/")[0] as string, segs.slice(1).join("/")) + search;
  }
  return null;
}

/** A same-origin path that is safe to send someone to after sign-in. */
export function safeNext(next: string | null | undefined): string | null {
  if (!next || !next.startsWith("/") || next.startsWith("//") || next.startsWith("/\\")) return null;
  return next;
}

/** Request header `proxy.ts` sets to the path and query being rendered (layouts cannot read the URL). */
export const PATH_HEADER = "x-bee-path";
