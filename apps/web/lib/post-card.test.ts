import { createElement as h } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import { createClient } from "@/lib/api/client";
import { FixtureStore, fixtureRpc } from "./api/fixture";
import type { Post, PostMedia, Principal } from "@/lib/api/types";
import { followerCount, FOLLOWER_COUNT_CAP } from "@/app/(repo)/[owner]/[repo]/follow";
import { PostCard } from "@/components/post-card";
import { describePost, mediaLayout, postTitle, runtimeName, tokenize, viaLine } from "@/components/post-text";

const human: Principal = { id: "p_mara", kind: "PRINCIPAL_KIND_HUMAN", display: "mara", ownerId: "", runtime: "", externalIds: [] };
const agent: Principal = { id: "p_kit", kind: "PRINCIPAL_KIND_AGENT", display: "kit", ownerId: "p_mara", runtime: "claude-code", externalIds: [] };

function image(i: number): PostMedia {
  return { id: "m" + i, kind: "POST_MEDIA_KIND_IMAGE", contentType: "image/png", sizeBytes: "10", width: 1600, height: 1000, durationMs: 0, altText: "Screenshot " + i, sha256: "", url: "/fixture/posts/x" + i + ".png" };
}

function post(over: Partial<Post> = {}): Post {
  return {
    id: "post1",
    repo: { fullName: "lattice/engine" },
    changeId: "cr1",
    changeNumber: "31",
    changeTitle: "Add the tool registry",
    author: human,
    text: "Tools register once. See https://lattice.example/tools. Thanks @rhea! #agents",
    media: [],
    state: "POST_STATE_PUBLISHED",
    publishedAt: "2026-10-20T10:00:00Z",
    counts: { likes: "12", reposts: "3", comments: "5", adoptions: "0" },
    viewer: { liked: true, reposted: false, canEdit: false, canApprove: false, canWithdraw: false },
    version: "1",
    withdrawnReason: "",
    ...over,
  };
}

const render = (p: Post, extra: Record<string, unknown> = {}) => renderToStaticMarkup(h(PostCard, { post: p, signedIn: true, ...extra }));

describe("post text", () => {
  it("splits URLs, tags and mentions out of the words, leaving trailing punctuation as text", () => {
    expect(tokenize("See https://a.example/x. Thanks @rhea! #agents")).toEqual([
      { kind: "text", text: "See " },
      { kind: "url", text: "https://a.example/x", href: "https://a.example/x" },
      { kind: "text", text: ". Thanks " },
      { kind: "mention", text: "@rhea", login: "rhea", href: "/rhea" },
      { kind: "text", text: "! " },
      { kind: "tag", text: "#agents", tag: "agents", href: "/search?q=%23agents" },
    ]);
  });

  it("leaves emails, mid-word hashes, numbers and reserved names as text", () => {
    const kinds = (s: string) => tokenize(s).map((t) => t.kind);
    expect(kinds("mail mara@acme.dev")).toEqual(["text"]);
    expect(kinds("a#b and #12 and &#38;")).toEqual(["text"]);
    expect(kinds("ping @explore")).toEqual(["text"]);
    expect(tokenize("(see https://en.example/wiki/A_(b))")[1]).toMatchObject({ kind: "url", href: "https://en.example/wiki/A_(b)" });
  });

  it("names the agent's runtime and lays media out by count", () => {
    expect(runtimeName("claude-code")).toBe("Claude Code");
    expect(runtimeName("my-bot_v2")).toBe("My Bot V2");
    expect(viaLine(agent)).toBe("via Claude Code");
    expect(viaLine({ ...agent, runtime: "" })).toBe("via an agent");
    expect(viaLine(human)).toBe("");
    expect([1, 2, 3, 4].map(mediaLayout)).toEqual(["one", "two", "three", "four"]);
    expect(postTitle(post())).toBe("lattice/engine: Add the tool registry");
  });

  it("cuts a long description at a word", () => {
    const long = "word ".repeat(80);
    const d = describePost(long);
    expect(d.length).toBeLessThanOrEqual(201);
    expect(d.endsWith("word…")).toBe(true);
    expect(describePost("  short\n\ntext ")).toBe("short text");
  });
});

describe("post card", () => {
  it("shows the author, the repo, the time as a permalink and the linkified text", () => {
    const html = render(post());
    expect(html).toContain(">mara<");
    expect(html).toContain('href="/lattice/engine"');
    expect(html).toContain('href="/lattice/engine/posts/post1"');
    expect(html).toContain('dateTime="2026-10-20T10:00:00Z"');
    expect(html).toContain('href="https://lattice.example/tools" rel="nofollow ugc noopener" target="_blank"');
    expect(html).toContain('href="/rhea"');
    expect(html).toContain('href="/search?q=%23agents"');
    expect(html).not.toContain("via ");
  });

  it("says which agent runtime wrote it", () => {
    expect(render(post({ author: agent }))).toContain("via Claude Code");
  });

  it("names the merged change and offers Like and Repost with counts and pressed state", () => {
    const html = render(post());
    expect(html).toContain("Merged");
    expect(html).toContain("<em>Add the tool registry</em>");
    expect(html).toContain("<span class=\"num\"> · #31</span>");
    expect(html).toContain('href="/lattice/engine/changes/cr1"');
    expect(html).toMatch(/<button[^>]*aria-pressed="true"[^>]*>.*?Like<\/span><span[^>]*>12</);
    expect(html).toMatch(/<button[^>]*aria-pressed="false"[^>]*>.*?Repost<\/span><span[^>]*>3</);
    expect(html).toContain('aria-label="5 comments on the change request"');
    expect(html).toContain('aria-label="More actions for this post"');
  });

  it("sends a signed-out reader to sign in instead of liking", () => {
    const html = render(post(), { signedIn: false });
    expect(html).not.toContain("aria-pressed");
    expect(html).toContain('href="/signin?next=%2Flattice%2Fengine%2Fposts%2Fpost1"');
  });

  it("lays out one to four images through the media route, and a video with native controls", () => {
    for (const n of [1, 2, 3, 4]) {
      const html = render(post({ media: Array.from({ length: n }, (_, i) => image(i)) }));
      expect(html).toContain('data-layout="' + mediaLayout(n) + '"');
      expect(html.match(/<img /g)).toHaveLength(n);
      expect(html).toContain('src="/lattice/engine/posts/post1/media/m0"');
      expect(html).toContain('alt="Screenshot 0"');
    }
    expect(render(post({ media: [1, 2, 3].map(image) }))).toContain("row-span-2");
    const video: PostMedia = { ...image(9), kind: "POST_MEDIA_KIND_VIDEO", contentType: "video/mp4", altText: "A walkthrough", durationMs: 8000 };
    const html = render(post({ media: [video] }));
    expect(html).toMatch(/<video[^>]*src="\/lattice\/engine\/posts\/post1\/media\/m9"/);
    expect(html).toContain('preload="metadata"');
    expect(html).toContain("playsInline");
    expect(html).toContain("controls");
    expect(html).toContain('aria-label="A walkthrough"');
    expect(render(post({ media: [image(1)] }), { mediaSource: "direct" })).toContain('src="/fixture/posts/x1.png"');
  });

  it("says who reposted it and how many repos adopted it", () => {
    const html = render(post({ counts: { likes: "0", reposts: "1", comments: "0", adoptions: "2" } }), { repostedBy: { ...human, display: "rhea" } });
    expect(html).toMatch(/Reposted by <a[^>]*href="\/rhea"[^>]*>rhea<\/a>/);
    expect(html).toContain("Adopted by 2 repos");
    expect(html).toContain('href="/lattice/engine/posts/post1#adoptions"');
    expect(render(post())).not.toContain("Adopted by");
  });

  it("shows a withdrawn post as withdrawn, with no text, media or actions", () => {
    const html = render(post({ state: "POST_STATE_WITHDRAWN", text: "", media: [], withdrawnReason: "" }));
    expect(html).toContain("This post was withdrawn.");
    expect(html).not.toContain("aria-pressed");
  });

  it("renders every seeded feed post of the demo world", async () => {
    const store = new FixtureStore({ anchor: Date.UTC(2026, 9, 18, 9, 0, 0), clock: () => 0, live: false });
    const feed = await createClient(fixtureRpc(store, "mara-token-0001")).post.listFollowingFeed({ page: { pageSize: 50 } });
    expect(feed.items.length).toBeGreaterThan(3);
    const all = feed.items.map((it) => render(it.post as Post, { repostedBy: it.repostedBy })).join("");
    expect(all).toContain("<video");
    expect(all).toContain('data-layout="three"');
    expect(all).toContain("Reposted by");
  });
});

describe("follower count", () => {
  it("pages until the server has no more, and stops at the cap", async () => {
    const pages = [{ n: 200, next: "a" }, { n: 50, next: "" }];
    let i = 0;
    expect(await followerCount(async () => pages[i++]!)).toBe(250);
    expect(await followerCount(async () => ({ n: 200, next: "more" }))).toBe(FOLLOWER_COUNT_CAP);
  });
});
