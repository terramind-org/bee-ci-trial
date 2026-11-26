import type { IconName } from "@/components/icons";
import { orgSettingsHref, repoHref } from "./routes";

/**
 * Where a nav item points. Repo-scoped items are a path under `/{owner}/{repo}`, org-scoped ones
 * a section of the org's settings, global ones an absolute path.
 */
export type NavTarget = { scope: "repo"; sub: string } | { scope: "org"; sub: string } | { scope: "global"; href: string };

export interface NavItem {
  id: string;
  target: NavTarget;
  label: string;
  icon: IconName;
  /** Second key of the `g` chord. */
  key: string;
  blurb: string;
  /** Only shown to callers who can write to the repo. */
  write?: boolean;
  /** Only shown signed in. */
  signedIn?: boolean;
}

/**
 * The information architecture (docs/spec/web-ui.md §2).
 *
 *   rail    Needs you · Feed · Repositories, then the organisation (People and teams · Agents
 *           and budgets · Record · Safety), then the repositories this browser opened last.
 *   repo    Code · Issues · Pull requests · Agents · CI · Safety · Insights · Settings, and a
 *           More menu: Releases · Packages · Pages · Security · Activity · Updates.
 *   palette everything above, plus the places with no link in the chrome (Explore, Trending,
 *           Leaderboards, Search, Import, New, the rest of the organisation's settings).
 *
 * The words are the familiar ones (pull request, issue, CI). The routes under them keep their
 * first names (`changes`, `work`, `runs`), so every old URL still opens. Order is deliberate.
 */
export const NAV: NavItem[] = [
  { id: "inbox", target: { scope: "global", href: "/" }, label: "Needs you", icon: "inbox", key: "i", blurb: "What is waiting for a person, across your repositories", signedIn: true },
  { id: "feed", target: { scope: "global", href: "/feed" }, label: "Feed", icon: "feed", key: "e", blurb: "Posts from people and repositories you can read", signedIn: true },
  { id: "repositories", target: { scope: "global", href: "/repositories" }, label: "Repositories", icon: "repo", key: "y", blurb: "Every repository you can read" },

  { id: "people", target: { scope: "org", sub: "members" }, label: "People and teams", icon: "team", key: "m", blurb: "Who is in the organization, their roles and teams", signedIn: true },
  { id: "budgets", target: { scope: "org", sub: "agents" }, label: "Agents and budgets", icon: "agents", key: "b", blurb: "Every agent, the person it answers to and what it may spend", signedIn: true },
  { id: "record", target: { scope: "org", sub: "audit" }, label: "Record", icon: "history", key: "d", blurb: "The sealed log of who did what, and which rule allowed it", signedIn: true },
  { id: "org-safety", target: { scope: "org", sub: "safety" }, label: "Safety", icon: "eye", key: "q", blurb: "Findings, tripwires and organisation-wide agent controls", signedIn: true },

  { id: "code", target: { scope: "repo", sub: "code" }, label: "Code", icon: "code", key: "o", blurb: "Files, commits, branches, blame and compare" },
  { id: "work", target: { scope: "repo", sub: "work" }, label: "Issues", icon: "work", key: "w", blurb: "What people and agents have asked for, and the board" },
  { id: "changes", target: { scope: "repo", sub: "changes" }, label: "Pull requests", icon: "changes", key: "c", blurb: "What agents want to merge, and the merge queue" },
  { id: "agents", target: { scope: "repo", sub: "agents" }, label: "Agents", icon: "agents", key: "a", blurb: "Every agent working in this repository, with its tokens" },
  { id: "runs", target: { scope: "repo", sub: "runs" }, label: "CI", icon: "runs", key: "r", blurb: "Workflow runs, checks and logs" },
  { id: "safety", target: { scope: "repo", sub: "safety" }, label: "Safety", icon: "eye", key: "v", blurb: "What was asked, against what was done" },
  { id: "insights", target: { scope: "repo", sub: "insights" }, label: "Insights", icon: "trend", key: "t", blurb: "How the repository is doing" },
  { id: "settings", target: { scope: "repo", sub: "settings" }, label: "Settings", icon: "settings", key: ",", blurb: "Rules, access and visibility", write: true },

  { id: "releases", target: { scope: "repo", sub: "releases" }, label: "Releases", icon: "releases", key: "l", blurb: "Tags, notes and assets" },
  { id: "packages", target: { scope: "repo", sub: "packages" }, label: "Packages", icon: "package", key: "p", blurb: "Containers, npm, Maven, NuGet and RubyGems" },
  { id: "pages", target: { scope: "repo", sub: "pages" }, label: "Pages", icon: "pages", key: "g", blurb: "The static site and its deployments" },
  { id: "security", target: { scope: "repo", sub: "security" }, label: "Security", icon: "security", key: "s", blurb: "Dependency, secret and code alerts" },
  { id: "activity", target: { scope: "repo", sub: "activity" }, label: "Activity", icon: "activity", key: "h", blurb: "Live stream of merges, reviews and leases" },
  { id: "updates", target: { scope: "repo", sub: "updates" }, label: "Updates", icon: "updates", key: "u", blurb: "Published posts in this repository" },

  { id: "explore", target: { scope: "global", href: "/explore" }, label: "Explore", icon: "compass", key: "x", blurb: "Repositories, topics and people worth a look" },
  { id: "search", target: { scope: "global", href: "/search" }, label: "Search", icon: "search", key: "f", blurb: "Code across every repository you can read" },
  { id: "org-settings", target: { scope: "org", sub: "" }, label: "Organization settings", icon: "settings", key: "z", blurb: "The organization, its repositories, teams, apps and billing", signedIn: true },
  { id: "import", target: { scope: "global", href: "/import" }, label: "Import", icon: "import", key: "n", blurb: "Bring a GitHub repository or organization across", signedIn: true },
];

/** The rail's first group, in order. Needs you and Feed are for signed-in people only. */
export const RAIL: readonly string[] = ["inbox", "feed", "repositories"];
/** The rail's organisation group, shown when the person belongs to one. */
export const ORG_RAIL: readonly string[] = ["people", "budgets", "record", "org-safety"];
/** The repository header's tabs, then its More menu, in reading order. */
export const REPO_TABS: readonly string[] = ["code", "work", "changes", "agents", "runs", "safety", "insights", "settings"];
export const REPO_MORE: readonly string[] = ["releases", "packages", "pages", "security", "activity", "updates"];

const byId = (ids: readonly string[]) => ids.map((id) => NAV.find((n) => n.id === id)).filter((n): n is NavItem => !!n);

/**
 * Resolves an item against the current repo (`owner/repo`) and its org. With no repository in
 * reach, Agents (the one repo section people ask for by name from anywhere) opens the agent
 * leaderboard instead.
 */
export function navHref(item: NavItem, repo: string, org: string = repo.split("/")[0] ?? ""): string {
  const t = item.target;
  if (t.scope === "repo") return item.id === "agents" && !repo.split("/")[1] ? AGENTS_FALLBACK : repoHref(repo, t.sub);
  if (t.scope === "org") return orgSettingsHref(org, t.sub);
  return t.href;
}

/** Where Agents goes when no repository is in scope. */
export const AGENTS_FALLBACK = "/leaderboards?board=agents-lands&since=week";

/** The repo tabs as nav items, in REPO_TABS order. Settings is for callers who can write. */
export function repoTabs(canWrite = true): NavItem[] {
  return byId(REPO_TABS).filter((n) => canWrite || !n.write);
}

/** The repo header's More menu, in REPO_MORE order. */
export function repoMore(): NavItem[] {
  return byId(REPO_MORE);
}

/** The rail's first group, in RAIL order. */
export function railItems(signedIn: boolean): NavItem[] {
  return byId(RAIL).filter((n) => signedIn || !n.signedIn);
}

/** The rail's organisation group: empty when signed out or in no organisation. */
export function orgRailItems(signedIn: boolean, org: string): NavItem[] {
  return signedIn && org ? byId(ORG_RAIL) : [];
}

/**
 * What the palette lists under "Go to" besides the rail: every global and organisation
 * destination with no link in the chrome, so nothing is reachable only by typing a URL.
 */
export function paletteExtras(signedIn: boolean, org: string): NavItem[] {
  const shown = new Set([...RAIL, ...ORG_RAIL]);
  return NAV.filter((n) => n.target.scope !== "repo" && !shown.has(n.id) && (signedIn || !n.signedIn) && (n.target.scope !== "org" || !!org));
}

/** Whether `path` is the item's page or below it. Needs you (`/`) is active only on `/` itself. */
export function isActive(item: NavItem, path: string, repo: string, org?: string): boolean {
  const href = navHref(item, repo, org).split("?")[0] as string;
  if (href === "/") return path === "/";
  // Permalinks and the composer live under posts, but belong to the existing Updates menu item.
  if (item.id === "updates" && repo) {
    const posts = repoHref(repo, "posts");
    if (path === posts || path.startsWith(posts + "/")) return true;
  }
  return path === href || path.startsWith(href + "/");
}

/** Up to five repositories, newest first, with `fullName` moved to the front. Pure. */
export function withRecent(list: readonly string[], fullName: string, max = 5): string[] {
  return [fullName, ...list.filter((r) => r !== fullName)].slice(0, max);
}

export const SHORTCUTS: { keys: string[]; label: string }[] = [
  { keys: ["⌘", "K"], label: "Command palette (Ctrl K elsewhere)" },
  { keys: ["/"], label: "Focus the filter on this page" },
  { keys: ["y"], label: "Copy this page's operation as CLI, HTTP or MCP" },
  { keys: ["j"], label: "Next item in a list" },
  { keys: ["k"], label: "Previous item in a list" },
  { keys: ["Enter"], label: "Open the focused item" },
  { keys: ["u"], label: "Pull request: toggle unified and split diff" },
  { keys: ["["], label: "Pull request: previous revision" },
  { keys: ["]"], label: "Pull request: next revision" },
  { keys: ["?"], label: "Show this list" },
  ...NAV.map((n) => ({ keys: ["g", n.key], label: "Go to " + n.label })),
];
