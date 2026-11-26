import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import { InspiredBy } from "./inspired-by";

describe("InspiredBy", () => {
  it("links to the post's permalink and names the change it posted about", () => {
    const html = renderToStaticMarkup(createElement(InspiredBy, { repo: "lattice/engine", inspiredBy: { postId: "01POST", repo: { fullName: "lattice/engine" }, changeNumber: "42", changeTitle: "Walk the graph without recursion" } }));
    expect(html).toContain('href="/lattice/engine/posts/01POST"');
    expect(html).toContain("Inspired by lattice/engine’s post");
    expect(html).toContain("Walk the graph without recursion");
    expect(html.replace(/<!-- -->/g, "")).toContain("(#42)");
    expect(html).toContain('aria-label="Inspired by"');
  });

  it("leaves out the change when the post does not name one", () => {
    const html = renderToStaticMarkup(createElement(InspiredBy, { repo: "a/b", inspiredBy: { postId: "p", changeNumber: "0", changeTitle: "" } }));
    expect(html).not.toContain(" about ");
    expect(html).not.toContain("#");
  });

  it("quotes the post and shows its first image", () => {
    const html = renderToStaticMarkup(
      createElement(InspiredBy, {
        repo: "lattice/engine",
        inspiredBy: { postId: "01POST", changeNumber: "42", changeTitle: "Walk the graph" },
        post: { text: "Iterative walk, no stack overflow.\n\nSame results, 3x less memory.", thumb: { src: "/lattice/engine/posts/01POST/media/m1", alt: "Memory before and after" } },
      }),
    );
    expect(html).toContain("Iterative walk, no stack overflow. Same results, 3x less memory.");
    expect(html).toContain('src="/lattice/engine/posts/01POST/media/m1"');
    expect(html).toContain('alt="Memory before and after"');
  });

  it("shows neither excerpt nor image without a post", () => {
    const html = renderToStaticMarkup(createElement(InspiredBy, { repo: "a/b", inspiredBy: { postId: "p", changeNumber: "1", changeTitle: "T" } }));
    expect(html).not.toContain("<blockquote");
    expect(html).not.toContain("<img");
  });
});
