import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import { canAnimate, createEventClaims, nextSnapshot } from "./event-keys";
import {
  Capping, Claiming, Landing, LoadingBee, PauseSmoke,
  QuietRepository, Refusal, YourBeeArrives,
} from "./pieces";

describe("confirmed event keys", () => {
  it("keeps the first snapshot still and claims each transition once across mounts", () => {
    const { claim, has } = createEventClaims();
    const open = nextSnapshot(undefined, "repo", { task: "open" }, "mounted");
    expect(open.keys).toEqual({});

    const working = nextSnapshot(open, "repo", { task: "working" }, "mounted");
    const workKey = working.keys.task!;
    expect(has(workKey)).toBe(false);
    expect(claim(workKey)).toBe(true);
    expect(has(workKey)).toBe(true);
    expect(claim(workKey)).toBe(false);
    expect(nextSnapshot(working, "repo", { task: "working" }, "mounted")).toBe(working);

    const finished = nextSnapshot(working, "repo", { task: "finished" }, "mounted");
    expect(claim(finished.keys.task!)).toBe(true);
    expect(nextSnapshot(undefined, "repo", finished.values, "remounted").keys).toEqual({});
    const other = nextSnapshot(finished, "other-repo", { task: "open" }, "mounted");
    expect(other.keys).toEqual({});
    const back = nextSnapshot(other, "repo", open.values, "mounted");
    expect(claim(nextSnapshot(back, "repo", working.values, "mounted").keys.task!)).toBe(true);
  });

  it("keeps keys for unchanged items and does not treat newly listed history as an event", () => {
    const open = nextSnapshot(undefined, "repo", { task: "open" }, "mounted");
    const working = nextSnapshot(open, "repo", { task: "working" }, "mounted");
    const newRows = nextSnapshot(working, "repo", { task: "working", history: "finished" }, "mounted");
    expect(newRows.keys).toEqual(working.keys);
    expect(nextSnapshot(newRows, "repo", { history: "finished" }, "mounted").keys).toEqual({});
  });
});

describe("resting pictures", () => {
  it("suppresses motion for reduced motion, agent mode and the review still", () => {
    expect(canAnimate("keeper", false, false)).toBe(true);
    expect(canAnimate("keeper", true, false)).toBe(false);
    expect(canAnimate("bee", false, false)).toBe(false);
    expect(canAnimate("keeper", false, true)).toBe(false);
  });

  it("gives every still an accessible label and marks only the person's arrival", () => {
    const pieces = [
      createElement(LoadingBee, { still: true }),
      ...[Capping, Claiming, Landing, Refusal, PauseSmoke, YourBeeArrives].map((Piece) =>
        createElement(Piece, { text: "Event confirmed.", still: true, eventKey: "seen" })),
      createElement(QuietRepository, { text: "No active work." }),
    ];

    const markup = pieces.map((piece) => renderToStaticMarkup(piece));
    for (const html of markup) {
      expect(html).toContain('aria-hidden="true"');
      expect(html).toContain('role="status"');
      expect(html).not.toContain("data-play");
    }
    expect(markup[0]).toContain(">Loading<");
    expect(markup.filter((html) => html.includes("var(--mark)"))).toEqual([markup[6]]);
  });
});
