import { describe, expect, it } from "vitest";
import { initialPresence, refreshDelay, stepPresence, withGhosts } from "./presence";

const key = (s: string) => s;

describe("stepPresence", () => {
  it("does nothing while the keys and context are unchanged", () => {
    const s = initialPresence(["a", "b"], key, "");
    expect(stepPresence(s, ["a", "b"], key, "")).toBeNull();
  });

  it("keeps a row that left as a ghost at its old index", () => {
    const s = initialPresence(["a", "b", "c"], key, "");
    const next = stepPresence(s, ["a", "c"], key, "")!;
    expect(next.ghosts).toEqual([{ item: "b", key: "b", index: 1, gen: 1 }]);
    expect(withGhosts(["a", "c"], next.ghosts, key)).toEqual([
      { item: "a", key: "a", leaving: false },
      { item: "b", key: "b", leaving: true },
      { item: "c", key: "c", leaving: false },
    ]);
  });

  it("drops a ghost whose row comes back", () => {
    const s = stepPresence(initialPresence(["a", "b"], key, ""), ["a"], key, "")!;
    const back = stepPresence(s, ["a", "b"], key, "")!;
    expect(back.ghosts).toEqual([]);
  });

  it("treats a new filter as a new view: no ghosts", () => {
    const s = initialPresence(["a", "b", "c"], key, "");
    const next = stepPresence(s, ["a"], key, "a")!;
    expect(next.ghosts).toEqual([]);
  });

  it("gives each removal its own generation", () => {
    const one = stepPresence(initialPresence(["a", "b", "c"], key, ""), ["a", "c"], key, "")!;
    const two = stepPresence(one, ["a"], key, "")!;
    expect(two.ghosts.map((g) => [g.key, g.gen])).toEqual([
      ["b", 1],
      ["c", 2],
    ]);
  });

  it("places ghosts at the end when the list got shorter than their old index", () => {
    expect(withGhosts(["a"], [{ item: "z", key: "z", index: 5, gen: 1 }], key).map((r) => r.key)).toEqual(["a", "z"]);
  });
});

describe("refreshDelay", () => {
  it("waits for a quiet gap at the start of a burst", () => {
    expect(refreshDelay(1000, 1000)).toBe(250);
  });
  it("never lets a steady stream push the refresh past the ceiling", () => {
    expect(refreshDelay(1000, 1900)).toBe(100);
    expect(refreshDelay(1000, 2500)).toBe(0);
  });
});
