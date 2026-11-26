import { expect, test } from "bun:test";
import { checkFidelity } from "../lib/fidelity";

test("a report without drops is clean", () => {
  const s = checkFidelity({ items: [{ category: "commits", imported: "10", dropped: "0", drops: [] }] });
  expect(s.unexplained).toEqual([]);
  expect(s.imported).toBe(10);
});

test("a drop with a reason is explained and counted", () => {
  const s = checkFidelity({
    items: [{ category: "pull_requests", imported: 3, dropped: 2, drops: [{ ref: "pull/1", reason: "not_supported_by_target" }, { ref: "pull/2", reason: "not_supported_by_target" }] }],
  });
  expect(s.unexplained).toEqual([]);
  expect(s.explained).toEqual([{ category: "pull_requests", reason: "not_supported_by_target", count: 2 }]);
});

test("a drop without a reason is unexplained", () => {
  const s = checkFidelity({ items: [{ category: "refs", dropped: 1, drops: [{ ref: "refs/x", reason: "  " }] }] });
  expect(s.unexplained).toEqual(["refs: drop of refs/x has no reason"]);
});

test("a dropped count that disagrees with the listed drops is unexplained", () => {
  const s = checkFidelity({ items: [{ category: "blobs", dropped: 3, drops: [{ ref: "a", reason: "r" }] }] });
  expect(s.unexplained).toEqual(["blobs: 3 dropped but 1 listed"]);
});
