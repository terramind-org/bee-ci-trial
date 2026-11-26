import { createElement as h } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import { pageTools } from "@/lib/tools";
import { PageFacts } from "./page-facts";

describe("a page in the agent's view", () => {
  const html = renderToStaticMarkup(
    h(PageFacts, {
      page: "pull",
      title: "acme/api · pull request #101",
      groups: [
        { title: "facts", facts: [{ label: "state", value: "open" }, null, false, { label: "target", value: "main" }] },
        { title: "why a person", lines: ["A change to a path the rules mark sensitive needs two approvals from a person."] },
        { title: "commits", lines: [], empty: "The server did not list them." },
      ],
      tools: pageTools("pull"),
    }),
  );

  it("opens with one heading and prints each fact as a labelled line", () => {
    expect(html.match(/<h1/g)).toHaveLength(1);
    expect(html).toContain("# acme/api · pull request #101");
    expect(html).toMatch(/<dt[^>]*>state<\/dt><dd[^>]*>open<\/dd>/);
    expect(html).toMatch(/<dt[^>]*>target<\/dt><dd[^>]*>main<\/dd>/);
    // A fact the page had no value for is not printed at all.
    expect(html.match(/<dt/g)).toHaveLength(2);
    expect(html).toContain("## why a person");
    expect(html).toContain("The server did not list them.");
  });

  it("lists the page's tools with their kind, and says what needs a person", () => {
    expect(html).toContain("## tools on this page");
    for (const t of pageTools("pull")) expect(html).toContain('data-tool="' + t.name + '"');
    expect(html).toMatch(/data-tool="change_get_change" data-kind="read"/);
    expect(html).toMatch(/data-tool="change_create_change_comment" data-kind="write"/);
    expect(html).toMatch(/data-tool="change_enqueue_change" data-kind="needs a person"/);
    expect(html).toContain("<strong");
    // Nothing to press: the agent's view has no buttons, only text and links.
    expect(html).not.toContain("<button");
  });

  it("keeps the heading order under a page that already has its h1", () => {
    const nested = renderToStaticMarkup(h(PageFacts, { page: "code", title: "acme/api · code", groups: [{ title: "facts", facts: [{ label: "repository", value: "acme/api" }] }], tools: pageTools("code"), level: 2 }));
    expect(nested).not.toContain("<h1");
    expect(nested.match(/<h2/g)).toHaveLength(1);
    expect(nested.match(/<h3/g)).toHaveLength(2);
  });
});
