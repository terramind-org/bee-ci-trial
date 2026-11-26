import { createElement as h } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import type { Achievement, Changeset, Member, Profile, SearchRepoHit, UserSummary } from "../../lib/api/types";
import { Achievements } from "./achievements";
import { newestLands } from "./activity";
import { People, topTopics } from "./org-people";
import { ProfileCard, profilePrincipal } from "./profile-card";
import { ProfileReadme } from "./readme";
import { UserList } from "./user-list";

const base: Profile = {
  kind: "user", principalId: "p1", login: "mara", name: "Mara Okafor", bio: "Engineering lead", company: "Acme", location: "Rotterdam", website: "https://mara.example",
  orgs: ["acme"], followers: 5, following: 2, followingByMe: false, publicRepos: 3,
};

describe("profile card", () => {
  it("shows name, login, counts that link to their tabs, and the orgs", () => {
    const html = renderToStaticMarkup(h(ProfileCard, { profile: base, signedIn: false, isSelf: false }));
    expect(html).toContain("Mara Okafor");
    expect(html).toContain('href="/mara?tab=followers"');
    expect(html).toContain('href="/mara?tab=following"');
    expect(html).toContain('href="/acme"');
    expect(html).toContain("Follow");
    expect(html).toContain("sign in first");
  });
  it("shows an agent's runtime and owner, and no Follow button for yourself", () => {
    const agent: Profile = { ...base, kind: "agent", login: "patcher", name: "patcher", runtime: "codex", ownerLogin: "rhea" };
    const html = renderToStaticMarkup(h(ProfileCard, { profile: agent, signedIn: true, isSelf: false }));
    expect(html).toContain("agent");
    expect(html).toContain("codex");
    expect(html).toContain('href="/rhea"');
    expect(profilePrincipal(agent).kind).toBe("PRINCIPAL_KIND_AGENT");
    expect(renderToStaticMarkup(h(ProfileCard, { profile: base, signedIn: true, isSelf: true }))).not.toContain("Follow");
  });
});

describe("readme, achievements, lists", () => {
  it("renders the server's sanitized README as is", () => {
    expect(renderToStaticMarkup(h(ProfileReadme, { html: "<h2>Hi</h2>", login: "mara" }))).toContain("<h2>Hi</h2>");
  });
  it("lists achievements with tier and count, and nothing when there are none", () => {
    const a: Achievement = { slug: "s", name: "Stargazer", description: "Starred repositories.", tier: "bronze", count: 4, imageUrl: "" };
    const html = renderToStaticMarkup(h(Achievements, { achievements: [a] }));
    expect(html).toContain("Stargazer");
    expect(html).toContain("×4");
    expect(renderToStaticMarkup(h(Achievements, { achievements: [] }))).toBe("");
  });
  it("links each follower to their profile, with an empty state", () => {
    const u: UserSummary = { login: "dev", name: "Dev Patel", avatarUrl: "", bio: "Billing" };
    expect(renderToStaticMarkup(h(UserList, { users: [u], empty: "none" }))).toContain('href="/dev"');
    expect(renderToStaticMarkup(h(UserList, { users: [], empty: "No followers yet" }))).toContain("No followers yet");
  });
});

describe("org pieces", () => {
  it("ranks topics by repo count then name", () => {
    const hit = (topics: string[]) => ({ topics }) as SearchRepoHit;
    expect(topTopics([hit(["rust", "cli"]), hit(["rust"]), hit(["agents"])])).toEqual([
      { name: "rust", count: 2 },
      { name: "agents", count: 1 },
      { name: "cli", count: 1 },
    ]);
  });
  it("says so when members are not shown to the viewer", () => {
    expect(renderToStaticMarkup(h(People, { members: null }))).toContain("signed-in visitors");
    const m: Member = { role: "ROLE_OWNER", principal: { id: "1", kind: "PRINCIPAL_KIND_HUMAN", display: "mara", ownerId: "", runtime: "", externalIds: [] } };
    expect(renderToStaticMarkup(h(People, { members: [m] }))).toContain("owner");
  });
  it("orders lands newest first, ties by repo", () => {
    const c = (t: string) => ({ meta: { committerTime: { time: t } } }) as unknown as Changeset;
    const out = newestLands([{ repo: "b", changeset: c("2026-01-01T00:00:00Z") }, { repo: "a", changeset: c("2026-01-01T00:00:00Z") }, { repo: "c", changeset: c("2026-02-01T00:00:00Z") }], 2);
    expect(out.map((l) => l.repo)).toEqual(["c", "a"]);
  });
});
