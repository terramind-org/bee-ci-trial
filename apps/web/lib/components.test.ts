import { createElement as h } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import { PrincipalMark, principalLabel } from "../components/domain";
import type { Principal } from "./api/types";
import { FollowButton } from "../components/follow-button";
import { Heatmap, buildHeatmap, heatmapSummary } from "../components/heatmap";
import { RepoCard } from "../components/repo-card";
import { StarButton } from "../components/star-button";

describe("repo card", () => {
  it("links the full name, prints visibility, language, stars and topics", () => {
    const html = renderToStaticMarkup(h(RepoCard, { fullName: "acme/api", description: "Invoicing service", language: "TypeScript", stars: 1234, visibility: "VISIBILITY_PUBLIC", topics: ["billing"], updatedAt: "2026-10-20T00:00:00Z" }));
    expect(html).toContain('href="/acme/api"');
    expect(html).toContain(">acme/api<");
    expect(html).toContain("Public");
    expect(html).toContain("Invoicing service");
    expect(html).toContain("TypeScript");
    expect(html).toContain('aria-label="1234 stars"');
    expect(html).toContain("1.2k");
    expect(html).toContain('href="/topics/billing"');
    expect(html).toContain('dateTime="2026-10-20T00:00:00Z"');
  });

  it("calls a private repo private and leaves out what it was not given", () => {
    const html = renderToStaticMarkup(h(RepoCard, { fullName: "acme/web", stars: 1, visibility: "VISIBILITY_PRIVATE" }));
    expect(html).toContain("Private");
    expect(html).toContain('aria-label="1 star"');
    expect(html).not.toContain("Updated");
    expect(html).not.toContain("Topics");
  });
});

describe("heatmap", () => {
  const days = [
    { date: "2026-10-27", count: 4 },
    { date: "2026-10-26", count: 1 },
    { date: "2026-03-03", count: 9 },
    { date: "2026-03-04", count: 2 },
    { date: "2024-01-01", count: 50 }, // outside the year: ignored
  ];

  it("lays 53 weeks by 7 days ending on the last day, with quartile steps", () => {
    const m = buildHeatmap(days);
    expect(m.weeks).toHaveLength(53);
    expect(m.weeks.every((w) => w.length === 7)).toBe(true);
    expect(m.total).toBe(16);
    const cells = m.weeks.flat().filter((c) => c !== null);
    expect(cells).toHaveLength(365);
    expect(cells.at(-1)?.date).toBe("2026-10-27");
    const step = (d: string) => cells.find((c) => c.date === d)?.step;
    expect(step("2026-03-03")).toBe(4);
    expect(step("2026-10-26")).toBe(1);
    expect(step("2026-10-25")).toBe(0);
    expect(m.busiestWeek).toBe("2026-03-01");
    expect(heatmapSummary(m)).toBe("16 contributions in the last year, busiest week of 1 March");
    expect(buildHeatmap(days)).toEqual(m);
  });

  it("renders a table with a hidden caption, per-day text and a legend", () => {
    const html = renderToStaticMarkup(h(Heatmap, { days }));
    expect(html).toContain("<table");
    expect(html).toContain('<caption class="sr-only">');
    expect(html).toContain('title="9 contributions on 3 March"');
    expect(html).toContain("Less");
    expect(html).toContain("More");
    expect((html.match(/<tr>/g) ?? []).length).toBe(7);
  });

  it("says so when there is nothing to show", () => {
    expect(heatmapSummary(buildHeatmap([]))).toBe("0 contributions in the last year");
  });
});

describe("star and follow buttons", () => {
  it("is a pressed toggle with its count and Copy as when signed in", () => {
    const html = renderToStaticMarkup(h(StarButton, { starred: true, count: 7, target: "acme/api" }));
    expect(html).toContain('aria-pressed="true"');
    expect(html).toContain('<span class="sr-only"> acme/api</span>');
    expect(html).toContain("Starred");
    expect(html).toContain(">7<");
    expect(html).toContain("Copy as");
  });

  it("asks for sign-in when signed out", () => {
    const html = renderToStaticMarkup(h(StarButton, { starred: false, count: 0, target: "acme/api", signedIn: false }));
    expect(html).toContain('href="/signin?next=');
    expect(html).toContain("(sign in first)");
  });

  it("is disabled with the reason when the server cannot do it", () => {
    const html = renderToStaticMarkup(h(FollowButton, { following: false, target: "mara", unavailable: "Follows are not available on this server (UNIMPLEMENTED)" }));
    expect(html).toContain("disabled");
    expect(html).toContain('title="Follows are not available on this server (UNIMPLEMENTED)"');
    expect(html).toContain('<span class="sr-only"> mara</span>');
    expect(html).not.toContain("Copy as");
  });
});

describe("principal names", () => {
  const agent: Principal = { id: "01M5ZKBWHC75WJ0TZ07KKNA7H4", kind: "PRINCIPAL_KIND_AGENT", display: "", ownerId: "", runtime: "", externalIds: [] };

  it("falls back to the id when the server sends no display name, so a lease holder is never unlabelled", () => {
    expect(principalLabel(agent)).toBe(agent.id);
    expect(principalLabel({ ...agent, display: "claude-code" })).toBe("claude-code");
    const html = renderToStaticMarkup(h(PrincipalMark, { principal: agent }));
    expect(html).toContain('aria-label="01M5ZKBWHC75WJ0TZ07KKNA7H4 (agent)"');
  });
});
