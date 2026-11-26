import { describe, expect, it } from "vitest";
import { blameOrigins, countChanges, diffLines, diffText, hunkLines, splitLines, toSplitRows } from "./diff";
import { classifyChange, classifyPath, highestClass } from "./diffclass";

describe("diff", () => {
  it("produces no hunks for identical text", () => {
    expect(diffText("a\nb\n", "a\nb\n")).toEqual([]);
  });

  it("produces unified hunks with correct line numbers", () => {
    const old = ["1", "2", "3", "4", "5", "6", "7", "8", "9", "10"].join("\n") + "\n";
    const nw = old.replace("5\n", "five\n");
    const [h] = diffText(old, nw, 2);
    expect(h).toEqual({ oldStart: 3, oldLen: 5, newStart: 3, newLen: 5, lines: [" 3", " 4", "-5", "+five", " 6", " 7"] });
  });

  it("handles pure additions to an empty file", () => {
    const [h] = diffText("", "a\nb\n");
    expect(h).toMatchObject({ oldStart: 0, oldLen: 0, newStart: 1, newLen: 2 });
  });

  it("is deterministic and minimal on a classic example", () => {
    const ops = diffLines(splitLines("a\nb\nc\na\nb\nb\na"), splitLines("c\nb\na\nb\na\nc"));
    expect(ops.filter((o) => o.kind === "eq")).toHaveLength(4);
    expect(diffLines(["x", "y"], ["y", "x"])).toEqual(diffLines(["x", "y"], ["y", "x"]));
  });

  it("pairs deletions and additions for split view", () => {
    const [h] = diffText("a\nb\nc\n", "a\nB\nc\nd\n", 1);
    if (!h) throw new Error("no hunk");
    const rows = toSplitRows(hunkLines(h));
    expect(rows.map((r) => [r.left?.text ?? null, r.right?.text ?? null])).toEqual([
      ["a", "a"],
      ["b", "B"],
      ["c", "c"],
      [null, "d"],
    ]);
    expect(countChanges([h])).toEqual({ added: 2, removed: 1 });
  });

  it("blames each line to the version that introduced it", () => {
    expect(blameOrigins(["a\nb\n", "a\nB\nb\n", "a\nB\nb\nc\n"])).toEqual([0, 1, 0, 2]);
  });
});

describe("diff classes", () => {
  it("classifies paths by policy rule before content", () => {
    expect(classifyPath("src/auth/session.ts", "a", "b")).toBe("sensitive");
    expect(classifyPath("bun.lock", "a", "b")).toBe("generated");
    expect(classifyPath("package.json", "a", "b")).toBe("dependency");
    expect(classifyPath("docs/api.md", "a", "b")).toBe("docs");
    expect(classifyPath("tests/x.test.ts", "a", "b")).toBe("test_only");
    expect(classifyPath("src/x.ts", "const a = 1;\n", "// why\nconst a = 1;\n")).toBe("comment_only");
    expect(classifyPath("src/x.ts", "const a = 1;\n", "const a = 2;\n")).toBe("logic");
  });

  it("detects content-identical moves", () => {
    const r = classifyChange([
      { path: "src/old.ts", oldText: "x\n", newText: null },
      { path: "src/new.ts", oldText: null, newText: "x\n" },
    ]);
    expect(r.overall).toBe("rename_only");
  });

  it("takes the highest-risk class for a revision", () => {
    expect(highestClass(["docs", "logic", "test_only"])).toBe("logic");
    expect(highestClass(["docs", "sensitive"])).toBe("sensitive");
    expect(highestClass([])).toBe("generated");
  });
});
