import { describe, expect, it } from "vitest";
import { ATOM_TYPE, buildAtom, escapeXml, feedAlternates, originOf, rfc3339, tagId, type AtomFeed } from "./atom";

const feed: AtomFeed = {
  origin: "https://bee.example",
  path: "/acme/api/feed.atom",
  pagePath: "/acme/api",
  title: "acme/api · Beehive",
  subtitle: "Invoices & <taxes>",
  entries: [
    { kind: "land", id: "acme/api/01A", title: "Fix \"rounding\" & drift", updated: "2026-10-20T10:00:00.000Z", path: "/acme/api/activity", summary: "line one\nline <two>", author: "Mara" },
    { kind: "release", id: "acme/api/rel1", title: "Release v1.2.0", updated: "2026-10-25T08:30:00Z", path: "/acme/api/releases" },
    { kind: "land", id: "acme/api/01B", title: "Newer land", updated: "2026-10-26T00:00:00Z", path: "/acme/api/activity" },
  ],
};

describe("atom", () => {
  it("escapes markup and drops characters XML cannot carry", () => {
    expect(escapeXml(`a & b < c > "d" 'e'`)).toBe("a &amp; b &lt; c &gt; &quot;d&quot; &apos;e&apos;");
    expect(escapeXml("x\u0000y\u0008z\u000bw")).toBe("xyzw");
    expect(escapeXml("tab\tnewline\nok 😀")).toBe("tab\tnewline\nok 😀");
  });

  it("builds tag ids from host, kind and id", () => {
    expect(tagId("bee.example", "land", "acme/api/01A")).toBe("tag:bee.example,2026:land/acme/api/01A");
  });

  it("writes a feed with entries newest first and escaped text", () => {
    const xml = buildAtom(feed);
    expect(xml.startsWith('<?xml version="1.0" encoding="utf-8"?>\n<feed xmlns="http://www.w3.org/2005/Atom">')).toBe(true);
    expect(xml).toContain("<title>acme/api · Beehive</title>");
    expect(xml).toContain("<subtitle>Invoices &amp; &lt;taxes&gt;</subtitle>");
    expect(xml).toContain("<id>tag:bee.example,2026:feed/acme/api</id>");
    expect(xml).toContain('<link rel="self" type="application/atom+xml" href="https://bee.example/acme/api/feed.atom"/>');
    expect(xml).toContain("<updated>2026-10-26T00:00:00Z</updated>");
    expect(xml).toContain("<title>Fix &quot;rounding&quot; &amp; drift</title>");
    expect(xml).toContain('<summary type="text">line one\nline &lt;two&gt;</summary>');
    expect(xml).toContain("<author><name>Mara</name></author>");
    expect(xml).toContain("<id>tag:bee.example,2026:release/acme/api/rel1</id>");
    expect(xml).toContain("<updated>2026-10-20T10:00:00Z</updated>");
    const order = [...xml.matchAll(/<entry>[\s\S]*?<id>(.*?)<\/id>/g)].map((m) => m[1]);
    expect(order).toEqual(["tag:bee.example,2026:land/acme/api/01B", "tag:bee.example,2026:release/acme/api/rel1", "tag:bee.example,2026:land/acme/api/01A"]);
  });

  it("is a pure function of its input", () => {
    expect(buildAtom(feed)).toBe(buildAtom({ ...feed, entries: [...feed.entries].reverse() }));
  });

  it("opens and closes every element it writes", () => {
    const xml = buildAtom(feed);
    for (const tag of ["feed", "entry", "title", "id", "updated", "author", "name", "summary"]) {
      expect([...xml.matchAll(new RegExp("<" + tag + "[ >]", "g"))].length, tag).toBe([...xml.matchAll(new RegExp("</" + tag + ">", "g"))].length);
    }
  });

  it("answers an empty feed with the epoch rather than the clock", () => {
    const xml = buildAtom({ ...feed, entries: [] });
    expect(xml).toContain("<updated>1970-01-01T00:00:00Z</updated>");
    expect(xml).not.toContain("<entry>");
  });

  it("reads the origin from forwarded headers first", () => {
    const h = (m: Record<string, string>) => ({ get: (k: string) => m[k] ?? null });
    expect(originOf("http://127.0.0.1:3100/x", h({}))).toBe("http://127.0.0.1:3100");
    expect(originOf("http://127.0.0.1:3100/x", h({ host: "bee.example", "x-forwarded-proto": "https" }))).toBe("https://bee.example");
    expect(originOf("http://127.0.0.1:3100/x", h({ "x-forwarded-host": "hub.example", host: "internal" }))).toBe("http://hub.example");
  });

  it("formats times as RFC 3339 UTC", () => {
    expect(rfc3339("2026-10-20T10:00:00.123Z")).toBe("2026-10-20T10:00:00Z");
    expect(rfc3339("nonsense")).toBe("1970-01-01T00:00:00Z");
  });

  it("offers the feed to pages through metadata alternates", () => {
    expect(feedAlternates("/acme/api")).toEqual({ types: { [ATOM_TYPE]: [{ url: "/acme/api/feed.atom", title: "Activity" }] } });
    expect(feedAlternates("/mara/", "Mara")).toEqual({ types: { [ATOM_TYPE]: [{ url: "/mara/feed.atom", title: "Mara" }] } });
  });
});
