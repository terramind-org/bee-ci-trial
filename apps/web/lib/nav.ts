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
 * The information architecture (docs/spec/web-ui.md §2). The rail is global: what needs you, what
 * the people and repos you follow shipped, agents, repositories, discovery. A repo's sections are
 * its header's tabs; the rarely visited ones sit under More. Old routes (`/activity`, `/work`, …)
 * keep their URLs; only their labels and places changed. An item can sit in more than one list. Order is deliberate.
 */
export const NAV: NavItem[] = [
  { id: "inbox", target: { scope: "global", href: "/" }, label: "Inbox", icon: "inbox", key: "i", blurb: "What needs you, what is in progress, what merged today", signedIn: true },
  { id: "feed", target: { scope: "global", href: "/feed" }, label: "Feed", icon: "feed", key: "e", blurb: "Posts from the repos and people you follow", signedIn: true },
  { id: "agents", target: { scope: "repo", sub: "agents" }, label: "Agents", icon: "agents", key: "a", blurb: "Principals, tokens, provenance and leases" },
  { id: "repositories", target: { scope: "global", href: "/repositories" }, label: "Repositories", icon: "repo", key: "y", blurb: "Every repository you can read" },
  { id: "explore", target: { scope: "global", href: "/explore" }, label: "Explore", icon: "compass", key: "x", blurb: "Repositories, topics and people worth a look" },

  { id: "code", target: { scope: "repo", sub: "code" }, label: "Code", icon: "code", key: "o", blurb: "Tree, files, history, blame and compare" },
  { id: "changes", target: { scope: "repo", sub: "changes" }, label: "Change requests", icon: "changes", key: "c", blurb: "Change requests, risk and the merge queue" },
  { id: "work", target: { scope: "repo", sub: "work" }, label: "Issues", icon: "work", key: "w", blurb: "Issues and the project board" },
  { id: "runs", target: { scope: "repo", sub: "runs" }, label: "Runs", icon: "runs", key: "r", blurb: "Workflows, checks and logs" },
  { id: "updates", target: { scope: "repo", sub: "updates" }, label: "Updates", icon: "updates", key: "u", blurb: "Posts on this repository's merged change requests" },

  { id: "activity", target: { scope: "repo", sub: "activity" }, label: "Activity", icon: "activity", key: "h", blurb: "Live stream of landing, review and leases" },
  { id: "releases", target: { scope: "repo", sub: "releases" }, label: "Releases", icon: "releases", key: "l", blurb: "Tags, notes and assets" },
  { id: "packages", target: { scope: "repo", sub: "packages" }, label: "Packages", icon: "package", key: "p", blurb: "Containers, npm, Maven, NuGet and RubyGems" },
  { id: "pages", target: { scope: "repo", sub: "pages" }, label: "Pages", icon: "pages", key: "g", blurb: "The static site and its deployments" },
  { id: "security", target: { scope: "repo", sub: "security" }, label: "Security", icon: "security", key: "s", blurb: "Dependency, secret and code alerts" },
  { id: "settings", target: { scope: "repo", sub: "settings" }, label: "Settings", icon: "settings", key: ",", blurb: "This repository's policy as code", write: true },

  { id: "search", target: { scope: "global", href: "/search" }, label: "Search", icon: "search", key: "f", blurb: "Code across every repo you can read" },
  { id: "org-settings", target: { scope: "org", sub: "" }, label: "Org settings", icon: "team", key: "m", blurb: "Members, teams, apps and the audit log" },
  { id: "import", target: { scope: "global", href: "/import" }, label: "Import", icon: "import", key: "n", blurb: "Bring a GitHub repo or org across" },
];

/** The repo header's plain tabs, then its More menu, in reading order. */
export const REPO_TABS: readonly string[] = ["code", "changes", "work", "runs", "updates"];
export const REPO_MORE: readonly string[] = ["activity", "agents", "releases", "packages", "pages", "security", "settings"];

/** The rail's destinations, in order. Inbox and Feed are for signed-in people only. */
export const RAIL: readonly string[] = ["inbox", "feed", "agents", "repositories", "explore"];

const byId = (ids: readonly string[]) => ids.map((id) => NAV.find((n) => n.id === id)).filter((n): n is NavItem => !!n);

/**
 * Resolves an item against the current repo (`owner/repo`) and its org. The rail's Agents item is
 * repo-scoped; with no repo in reach it opens the agent leaderboard instead.
 */
export function navHref(item: NavItem, repo: string, org: string = repo.split("/")[0] ?? ""): string {
  const t = item.target;
  if (t.scope === "repo") return item.id === "agents" && !repo.split("/")[1] ? AGENTS_FALLBACK : repoHref(repo, t.sub);
  if (t.scope === "org") return orgSettingsHref(org, t.sub);
  return t.href;
}

/** Where the rail's Agents link goes when no repository is in scope. */
export const AGENTS_FALLBACK = "/leaderboards?board=agents-lands&since=week";

/** The repo tabs as nav items, in REPO_TABS order. */
export function repoTabs(): NavItem[] {
  return byId(REPO_TABS);
}

/** The repo header's More menu, in REPO_MORE order. */
export function repoMore(): NavItem[] {
  return byId(REPO_MORE);
}

/** The rail, in RAIL order. */
export function railItems(signedIn: boolean): NavItem[] {
  return byId(RAIL).filter((n) => signedIn || !n.signedIn);
}

/** Whether `path` is the item's page or below it. The Inbox (`/`) is active only on `/` itself. */
export function isActive(item: NavItem, path: string, repo: string, org?: string): boolean {
  const href = navHref(item, repo, org).split("?")[0] as string;
  if (href === "/") return path === "/";
  return path === href || path.startsWith(href + "/");
}

export const SHORTCUTS: { keys: string[]; label: string }[] = [
  { keys: ["⌘", "K"], label: "Command palette (Ctrl K elsewhere)" },
  { keys: ["/"], label: "Focus the filter on this page" },
  { keys: ["y"], label: "Copy this page's operation as CLI, HTTP or MCP" },
  { keys: ["j"], label: "Next item in a list" },
  { keys: ["k"], label: "Previous item in a list" },
  { keys: ["Enter"], label: "Open the focused item" },
  { keys: ["u"], label: "Change view: toggle unified and split diff" },
  { keys: ["["], label: "Change view: previous revision" },
  { keys: ["]"], label: "Change view: next revision" },
  { keys: ["?"], label: "Show this list" },
  ...NAV.map((n) => ({ keys: ["g", n.key], label: "Go to " + n.label })),
];
