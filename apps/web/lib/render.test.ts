import { describe, expect, it } from "vitest";
import { renderMarkdown } from "../components/markdown";
import { BeeError } from "./api/transport";
import { escapeHtml, highlightLines, languageOf } from "./highlight";
import { parsePolicy, evaluateLand } from "./api/fixture/policy";
import { POLICY_V1 } from "./api/fixture/content";

describe("markdown", () => {
  it("escapes HTML and neutralises javascript: links", () => {
    const html = renderMarkdown('<script>alert(1)</script>\n\n[x](javascript:alert(1)) [y](https://bee.dev) [z](/changes)');
    expect(html).not.toContain("<script>");
    expect(html).toContain("&lt;script&gt;");
    expect(html).toContain('<a href="#">x</a>');
    expect(html).toContain('href="https://bee.dev" rel="noreferrer noopener" target="_blank"');
    expect(html).toContain('<a href="/changes">z</a>');
  });

  it("renders headings one level below the page title, lists, code and tables", () => {
    const html = renderMarkdown("# Title\n\n- a\n- b\n\n```\nconst x = 1 < 2;\n```\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\nFixes #12.");
    expect(html).toContain("<h2>Title</h2>");
    expect(html).toContain("<ul><li>a</li><li>b</li></ul>");
    expect(html).toContain("<pre><code>const x = 1 &lt; 2;</code></pre>");
    expect(html).toContain("<td>1</td>");
    expect(html).toContain('<a href="/work/12">#12</a>');
  });

  it("links issue numbers into the repo it renders for", () => {
    expect(renderMarkdown("Fixes #12.", "acme/api")).toContain('<a href="/acme/api/work/12">#12</a>');
  });
});

describe("highlight", () => {
  it("returns one escaped HTML string per line", () => {
    const lines = highlightLines('const a = "<b>";\n// c\n', "x.ts");
    expect(lines).toHaveLength(3);
    expect(lines[0]).toContain("&lt;b&gt;");
    expect(highlightLines("<x>", "notes.md")).toEqual(["&lt;x&gt;"]);
    expect(escapeHtml('"&')).toBe("&quot;&amp;");
    expect(languageOf("a/b.tsx")).toBe("TypeScript JSX");
  });
});

describe("BeeError identity", () => {
  it("is recognised across module copies by its registered brand", () => {
    const foreign = Object.assign(new Error("x"), { [Symbol.for("bee.v1.BeeError")]: true, code: "not_found" });
    expect(foreign instanceof BeeError).toBe(true);
    expect(new Error("y") instanceof BeeError).toBe(false);
    const e = new BeeError("failed_precondition", { reason: "LEASED", message: "held" });
    expect(e.toJSON().details[0]?.debug?.reason).toBe("LEASED");
    expect(BeeError.fromJSON(e.toJSON(), 400).reason).toBe("LEASED");
  });
});

describe("policy reader", () => {
  it("reads the demo policy without errors and evaluates land rules", () => {
    const { rules, errors } = parsePolicy(POLICY_V1);
    expect(errors).toEqual([]);
    expect(rules.requiredChecks).toEqual(["test", "lint"]);
    const base = { authorHandle: "docs", paths: ["docs/x.md"], checks: [{ name: "test", status: "CHECK_STATUS_PASSED" }, { name: "lint", status: "CHECK_STATUS_PASSED" }], approvals: [], exclusiveLeases: [] };
    expect(evaluateLand(rules, { ...base, diffClass: "docs", authorKind: "agent" }).allowed).toBe(true);
    const logic = evaluateLand(rules, { ...base, diffClass: "logic", authorKind: "agent", paths: ["src/billing/tax.ts"] });
    expect(logic.missing).toContain("approval:owner:src/billing (0 of 1)");
    const auth = evaluateLand(rules, { ...base, diffClass: "sensitive", authorKind: "agent", paths: ["src/auth/session.ts"], approvals: [{ handle: "mara", kind: "human" }, { handle: "dev", kind: "human" }] });
    expect(auth.missing).toEqual(["lease:exclusive:src/auth"]);
  });

  it("flags unknown sections, classes and unclosed arrays with line numbers", () => {
    const { errors } = parsePolicy('[nope]\n[land."main"]\nagents_may_land = ["docs", "magic"]\napprovals = [\n');
    expect(errors.map((e) => e.line)).toEqual([1, 3, 4]);
  });
});
