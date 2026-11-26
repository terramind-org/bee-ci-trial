import { describe, expect, it } from "vitest";
import { worthALook } from "./hunks";
import { diffItem } from "./brief-samples";

describe("worth a human look", () => {
  it("picks at most three sensitive or logic hunks, sensitive first, one per file before seconds", () => {
    const twoHunks = diffItem("src/billing/invoice.ts", "logic", ["+a", "+b", "+c"]);
    twoHunks.hunks.push({ oldStart: 40, oldLen: 1, newStart: 42, newLen: 1, lines: ["-x", "+y"] });
    const picked = worthALook([
      diffItem("docs/x.md", "docs", ["+doc"]),
      diffItem("src/billing/invoice.test.ts", "logic", ["+t", "+t", "+t", "+t"]),
      twoHunks,
      diffItem("src/cache/lru.ts", "logic", ["+a"]),
      diffItem("src/auth/session.ts", "sensitive", ["-a", "+b"]),
      diffItem("src/feed/rank.ts", "logic", [" ctx"]),
    ]);
    expect(picked.map((p) => p.path + "#" + p.index)).toEqual(["src/auth/session.ts#0", "src/billing/invoice.ts#0", "src/cache/lru.ts#0"]);
    expect(picked[0]?.reason).toBe("Touches a path the rules mark sensitive.");
  });

  it("prefers paths the brief names, then fills with second hunks, and cuts long hunks", () => {
    const big = diffItem("src/a/big.ts", "logic", Array.from({ length: 60 }, (_, i) => "+" + i));
    big.hunks.push({ oldStart: 90, oldLen: 1, newStart: 150, newLen: 1, lines: ["-p", "+q"] });
    const named = diffItem("src/z/small.ts", "logic", ["+1"]);
    const picked = worthALook([big, named], { highlights: [{ text: "The small fix", paths: ["src/z"] }] });
    expect(picked.map((p) => p.path + "#" + p.index)).toEqual(["src/z/small.ts#0", "src/a/big.ts#0", "src/a/big.ts#1"]);
    expect(picked[0]?.highlight).toBe("The small fix");
    expect(picked[1]?.hunk.lines).toHaveLength(40);
    expect(picked[1]?.truncated).toBe(20);
    expect(picked[1]?.added).toBe(60);
  });
});
