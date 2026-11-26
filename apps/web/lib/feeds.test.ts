import { beforeAll, describe, expect, it } from "vitest";
import type { Post } from "./api/types";
import { buildAtom } from "./atom";

// The feeds read the fixture world through the same anonymous client a feed reader gets.
beforeAll(() => {
  process.env.BEE_ADAPTER = "fixture";
});

const post = (over: Partial<Post>): Post => ({
  id: "post1",
  repo: { fullName: "acme/api" },
  changeId: "cr1",
  changeNumber: "4",
  changeTitle: "Fix rounding",
  text: "Totals round once.",
  media: [],
  state: "POST_STATE_PUBLISHED",
  publishedAt: "2026-10-20T10:00:00Z",
  version: "1",
  withdrawnReason: "",
  ...over,
});

describe("repo feed posts", () => {
  it("turns a published post into an entry at its permalink and skips anything unpublished", async () => {
    const { postEntry } = await import("./feeds");
    expect(postEntry("acme/api", post({}))).toEqual({ kind: "post", id: "acme/api/post1", title: "Merged Fix rounding · acme/api", updated: "2026-10-20T10:00:00Z", path: "/acme/api/posts/post1", summary: "Totals round once.", author: undefined });
    expect(postEntry("acme/api", post({ media: [{} as never, {} as never] }))?.summary).toBe("Totals round once.\n\n(2 attachments)");
    for (const state of ["POST_STATE_DRAFT", "POST_STATE_PENDING_APPROVAL", "POST_STATE_WITHDRAWN", "POST_STATE_DISCARDED"] as const) expect(postEntry("acme/api", post({ state }))).toBeNull();
  });

  it("includes a public repo's published posts beside its lands", async () => {
    const { repoFeed } = await import("./feeds");
    const feed = await repoFeed("https://bee.example", "lattice/engine");
    const posts = feed?.entries.filter((e) => e.kind === "post") ?? [];
    expect(posts.length).toBeGreaterThan(2);
    expect(posts.every((e) => e.path.startsWith("/lattice/engine/posts/"))).toBe(true);
    expect(feed?.entries.some((e) => e.kind === "land")).toBe(true);
    const xml = buildAtom(feed!);
    expect(xml).toContain("<id>tag:bee.example,2026:post/lattice/engine/");
    expect(xml).toContain("Rebase conflicts now come back sorted by path.");
  });

  it("never lists a private repo's posts", async () => {
    const { repoFeed } = await import("./feeds");
    expect(await repoFeed("https://bee.example", "acme/api")).toBeNull();
  });
});
