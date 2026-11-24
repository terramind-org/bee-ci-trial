import { createElement as h } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import { pathBytes } from "../../lib/api/ids";
import type { LanguageStat, Lease, Repo, SearchRepoHit, TreeEntry } from "../../lib/api/types";
import { archiveRefusal } from "./archive-download";
import { cloneTabs } from "./clone-panel";
import { FileTreeRoot, ROOT_ROWS, orderTree } from "./file-tree-root";
import { LanguageBar, percentText } from "./language-bar";
import { RepoNote } from "./note";
import { editingSentence, holders, leaseTouches } from "./presence";
import { hitOfRepo, listHref, ownerQuery, parseListParams, refine, searchQuery, visibilityOf } from "./query";

const repo: Repo = {
  id: "r1", orgId: "o1", name: "api", fullName: "acme/api", visibility: "VISIBILITY_PUBLIC", defaultRef: "main", description: "Invoicing service", archived: false,
  topics: ["billing", "rust"], homepage: "https://acme.example/api", stars: "12", forks: "3", openIssues: "4", openChanges: "2", licenseSpdx: "MIT", primaryLanguage: "Rust",
};
const langs: LanguageStat[] = [
  { name: "Rust", byteCount: "900", fileCount: 9, percentage: 90, color: "#dea584" },
  { name: "Shell", byteCount: "100", fileCount: 1, percentage: 10, color: "" },
];

describe("clone panel tabs", () => {
  it("offers HTTPS, SSH, CLI and Agent when the server gives both URLs", () => {
    const tabs = cloneTabs({ fullName: "acme/api", https: "https://h/acme/api.git", ssh: "git@h:acme/api.git" });
    expect(tabs.map((t) => t.label)).toEqual(["HTTPS", "SSH", "Bee CLI", "Agent"]);
    expect(tabs[0]?.command).toBe("git clone https://h/acme/api.git");
    expect(tabs[2]?.command).toBe("bee git setup --repo acme/api");
    expect(tabs[3]?.command).toBeUndefined();
  });
  it("leaves out what the server did not advertise", () => {
    expect(cloneTabs({ fullName: "acme/api", https: "" }).map((t) => t.id)).toEqual(["cli", "agent"]);
  });
});

describe("archive refusals", () => {
  it("names the redacted and no-access cases and keeps others generic", () => {
    expect(archiveRefusal("REDACTED")).toBe("Refused: this tree contains redacted content");
    expect(archiveRefusal("POLICY_DENIED")).toBe("You lack read access to some paths");
    expect(archiveRefusal("UNAUTHENTICATED")).toMatch(/Sign in/);
    expect(archiveRefusal("UNIMPLEMENTED")).toMatch(/not available/);
    expect(archiveRefusal("INTERNAL")).toBe("Could not prepare the archive");
  });
});

describe("language bar", () => {
  it("prints every share and uses the server's colour", () => {
    const html = renderToStaticMarkup(h(LanguageBar, { languages: langs }));
    expect(html).toContain("Languages: Rust 90%, Shell 10%");
    expect(html).toContain("#dea584");
    expect(percentText(0.04)).toBe("<0.1%");
  });
  it("renders nothing without languages", () => {
    expect(renderToStaticMarkup(h(LanguageBar, { languages: [] }))).toBe("");
  });
});

describe("repo note", () => {
  const holder = { id: "p1", kind: "PRINCIPAL_KIND_AGENT" as const, display: "triage", ownerId: "", runtime: "claude-code", externalIds: [] };
  const lease = { id: "l1", holder, paths: [pathBytes("src/auth")], mode: "LEASE_MODE_EXCLUSIVE", note: "Rotating session ids" } as unknown as Lease;
  it("folds topics, license, languages and who is editing into one note", () => {
    const html = renderToStaticMarkup(h(RepoNote, { repo, languages: langs, leases: [lease], leasesVisible: true, agents: [], signedIn: true }));
    expect(html).toContain('href="/topics/billing"');
    expect(html).toContain("MIT");
    expect(html).toContain("Languages: Rust 90%, Shell 10%");
    expect(html).toContain("triage is editing src/auth (Rotating session ids).");
    expect(html).not.toContain("rounded-lg border");
  });
  it("says so when nobody is editing, and asks anonymous visitors to sign in", () => {
    const quiet = renderToStaticMarkup(h(RepoNote, { repo, languages: [], leases: [], leasesVisible: true, agents: [], signedIn: false }));
    expect(quiet).toContain("Nobody is editing right now.");
    const hidden = renderToStaticMarkup(h(RepoNote, { repo, languages: [], leases: [], leasesVisible: false, agents: [], signedIn: false }));
    expect(hidden).toContain("Sign in to see who is editing here.");
  });
});

describe("presence", () => {
  const holder = { id: "p1", kind: "PRINCIPAL_KIND_AGENT" as const, display: "triage", ownerId: "", runtime: "", externalIds: [] };
  const lease = (paths: string[]) => ({ id: "l", holder, paths: paths.map(pathBytes), mode: "LEASE_MODE_ADVISORY", note: "" }) as unknown as Lease;
  it("matches a lease to a path above or below it", () => {
    expect(leaseTouches("src/auth/", "src")).toBe(true);
    expect(leaseTouches("src", "src/auth/login.ts")).toBe(true);
    expect(leaseTouches("src/auth", "src/billing")).toBe(false);
  });
  it("words the sentence from the paths under the row and names each holder once", () => {
    expect(editingSentence([lease(["src/auth", "docs"])], "src")).toBe("triage is editing src/auth.");
    expect(holders([lease(["a"]), lease(["b"])])).toHaveLength(1);
  });
});

describe("file tree root", () => {
  const e = (name: string, mode: TreeEntry["mode"]): TreeEntry => ({ path: pathBytes(name), mode, size: "10", restricted: false });
  it("lists directories first and links into the code browser", () => {
    const entries = [e("b.txt", "MODE_FILE"), e("src", "MODE_TREE"), e("a.txt", "MODE_FILE")];
    expect(orderTree(entries).map((x) => x.mode === "MODE_TREE")).toEqual([true, false, false]);
    const html = renderToStaticMarkup(h(FileTreeRoot, { fullName: "acme/api", entries }));
    expect(html).toContain('href="/acme/api/code?path=src"');
    expect(html.indexOf("src/")).toBeLessThan(html.indexOf("a.txt"));
  });
  it("caps long roots with a link to the rest", () => {
    const entries = Array.from({ length: ROOT_ROWS + 3 }, (_, i) => e("f" + String(i).padStart(2, "0"), "MODE_FILE"));
    expect(renderToStaticMarkup(h(FileTreeRoot, { fullName: "acme/api", entries }))).toContain("Show all 15 entries");
  });
});

describe("list query", () => {
  it("reads the URL and signed-out visitors only get the public tab", () => {
    expect(parseListParams({ tab: "starred", sort: "stars", language: "Rust" }, true)).toMatchObject({ tab: "starred", sort: "stars", language: "Rust" });
    expect(parseListParams({ tab: "starred" }, false).tab).toBe("public");
    expect(parseListParams({}, true).tab).toBe("yours");
    expect(parseListParams({ page: "-3", sort: "bogus", visibility: "x" }, true)).toMatchObject({ page: 1, sort: "updated", visibility: "" });
  });
  it("builds qualifiers and cannot be made to add more", () => {
    const p = parseListParams({ tab: "public", q: "billing api", language: "Rust is:private", topic: "Agents" }, true);
    expect(searchQuery(p, "")).toBe("is:public language:Rustisprivate topic:agents billing api");
    expect(searchQuery(parseListParams({ visibility: "private" }, true), "mara")).toBe("user:mara is:private");
    expect(ownerQuery(p, "org", "acme")).toBe("org:acme language:Rustisprivate topic:agents billing api");
  });
  it("round-trips defaults away", () => {
    const base = parseListParams({}, true);
    expect(listHref({}, base, true)).toBe("/repositories");
    expect(listHref({ tab: "public", q: "x" }, base, true)).toBe("/repositories?tab=public&q=x");
    expect(listHref({ q: "x" }, base, false, { path: "/acme", fixed: { tab: "repositories" } })).toBe("/acme?tab=repositories&q=x");
  });
  it("refines in memory the way the server orders", () => {
    const hit = (n: string, stars: string, pushed: string, vis = "public"): SearchRepoHit => ({ ...hitOfRepo({ ...repo, fullName: n, stars, pushedAt: pushed }), visibility: vis });
    const hits = [hit("b/x", "5", "2026-01-01T00:00:00Z"), hit("a/y", "9", "2026-02-01T00:00:00Z", "private"), hit("c/z", "5", "2026-03-01T00:00:00Z")];
    const base = { q: "", visibility: "" as const, language: "", topic: "" };
    expect(refine(hits, { ...base, sort: "name" }).map((h) => h.fullName)).toEqual(["a/y", "b/x", "c/z"]);
    expect(refine(hits, { ...base, sort: "stars" }).map((h) => h.fullName)).toEqual(["a/y", "b/x", "c/z"]);
    expect(refine(hits, { ...base, sort: "updated" }).map((h) => h.fullName)).toEqual(["c/z", "a/y", "b/x"]);
    expect(refine(hits, { ...base, visibility: "private", sort: "name" }).map((h) => h.fullName)).toEqual(["a/y"]);
    expect(refine(hits, { ...base, topic: "rust", language: "rust", q: "Y", sort: "name" }).map((h) => h.fullName)).toEqual(["a/y"]);
    expect(visibilityOf("internal")).toBe("VISIBILITY_INTERNAL");
  });
});
