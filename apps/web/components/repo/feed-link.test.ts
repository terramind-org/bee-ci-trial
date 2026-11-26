import { createElement as h } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import { FeedLink } from "./feed-link";

describe("FeedLink", () => {
  it("offers the page's Atom feed as a visible link", () => {
    const html = renderToStaticMarkup(h(FeedLink, { pagePath: "/acme/api" }));
    expect(html).toContain('href="/acme/api/feed.atom"');
    expect(html).toContain("Activity feed");
  });
});
