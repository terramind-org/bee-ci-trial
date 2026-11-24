import { createElement as h } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import { EmptyInbox } from "./inbox";

describe("EmptyInbox", () => {
  it("shows the setup-repository prompt with the first repository actions", () => {
    const html = renderToStaticMarkup(h(EmptyInbox, { endpoint: "https://bee.example" }));
    expect(html).toContain('data-testid="home-onboarding"');
    expect(html).toContain("Set up Bee for a repository");
    expect(html).toContain('href="/new"');
    expect(html).toContain('href="/import"');
  });
});
