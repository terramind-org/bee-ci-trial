import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import { Avatar, type AvatarKind } from "./avatar";

const render = (id: string, kind: AvatarKind, extra: { runtime?: string; avatarUrl?: string } = {}) =>
  renderToStaticMarkup(createElement(Avatar, { principal: { id, name: "Name " + id, kind, ...extra }, size: 32 }));

describe("Avatar", () => {
  it("renders the same markup for the same principal", () => {
    for (const kind of ["human", "agent", "service"] as const) expect(render("p-1", kind, { runtime: "claude-code" })).toBe(render("p-1", kind, { runtime: "claude-code" }));
  });

  it("draws a distinct image for each of 20 ids, per kind", () => {
    const ids = Array.from({ length: 20 }, (_, i) => "principal-" + i);
    for (const kind of ["human", "agent", "service"] as const) {
      const drawings = new Set(ids.map((id) => render(id, kind).replace(/av-[a-z0-9]+/g, "")));
      expect(drawings.size).toBe(20);
    }
  });

  it("is labelled with the name and kind", () => {
    const html = render("p-1", "agent", { runtime: "claude-code" });
    expect(html).toContain('role="img"');
    expect(html).toContain('aria-label="Name p-1 (agent)"');
  });

  it("shows a photo when there is an avatarUrl", () => {
    expect(render("p-1", "human", { avatarUrl: "https://example.com/a.png" })).toContain('src="https://example.com/a.png"');
  });

  it("marks agents with their runtime and nobody else", () => {
    expect(render("a", "agent", { runtime: "codex" })).toContain(">C</text>");
    expect(render("a", "human", { runtime: "codex" })).not.toContain("<text");
  });
});
