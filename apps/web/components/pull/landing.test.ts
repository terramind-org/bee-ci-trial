import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import { Landing } from "@/components/bees/pieces";
import { PullDetailLanding } from "./landing";

describe("pull request landing", () => {
  it("shows the still on an already merged detail and nothing on an open detail", () => {
    const props = { repo: "acme/api", id: "cr1", number: "101", target: "main" };
    expect(renderToStaticMarkup(createElement(PullDetailLanding, { ...props, state: "CHANGE_STATE_OPEN" }))).toBe("");
    const historical = renderToStaticMarkup(createElement(PullDetailLanding, { ...props, state: "CHANGE_STATE_LANDED" }));
    expect(historical).toContain("<svg");
    expect(historical).not.toContain("Pull request #101 merged into main.");
  });

  it("announces the text when the merge has an observed event key", () => {
    const text = "Pull request #101 merged into main.";
    const observed = renderToStaticMarkup(createElement(Landing, { eventKey: "merge-101", text }));
    expect(observed).toContain('aria-live="polite"');
    expect(observed).toContain(text);
  });
});
