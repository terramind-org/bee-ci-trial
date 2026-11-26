import { describe, expect, it } from "vitest";
import { areaOf, changeSize, fromDiff, isTestPath, whatChanged } from "./whatchanged";
import { diffItem } from "./brief-samples";

describe("what changed", () => {
  it("names areas near the top of the tree", () => {
    expect(areaOf("src/billing/invoice.ts")).toBe("src/billing");
    expect(areaOf("src/billing/tax/vat.ts")).toBe("src/billing");
    expect(areaOf("apps/web/lib/x.ts")).toBe("apps/web/lib");
    expect(areaOf("README.md")).toBe("");
    expect(areaOf("tests/billing/invoice.test.ts")).toBe("billing");
    expect(areaOf("tests/session.rs")).toBe("tests");
    expect(isTestPath("src/billing/invoice.test.ts")).toBe(true);
    expect(isTestPath("pkg/x_test.go")).toBe(true);
    expect(isTestPath("src/testing.ts")).toBe(false);
  });

  it("groups files into sentences by area, riskiest first, tests joining their code", () => {
    const files = fromDiff([
      diffItem("src/billing/invoice.ts", "logic", ["-a", "+b", "+c"]),
      diffItem("src/billing/tax.ts", "logic", ["+x"]),
      diffItem("src/billing/rounding.ts", "logic", ["+y"], "added"),
      diffItem("tests/invoice.test.ts", "test_only", ["+t"]),
      diffItem("src/auth/session.ts", "sensitive", ["-old", "+new"]),
      diffItem("docs/billing.md", "docs", ["+doc"]),
      diffItem("README.md", "docs", ["+r"]),
    ]);
    const areas = whatChanged(files, [{ text: "Invoices round half to even", paths: ["src/billing/rounding.ts"] }]);
    expect(areas.map((a) => a.sentence)).toEqual([
      "Sensitive changes in src/auth (1 file), without tests.",
      "Behaviour changes in src/billing (3 files), with tests.",
      "Documentation changes at the top of the repo (1 file).",
      "Documentation changes in docs (1 file).",
    ]);
    expect(areas[1]?.tests.map((t) => t.path)).toEqual(["tests/invoice.test.ts"]);
    expect(areas[1]?.highlights).toEqual(["Invoices round half to even"]);
    expect(areas[1]?.added).toBe(5);
    expect(changeSize(files)).toBe("7 files in 4 areas, +8 −2");
  });

  it("keeps tests with no code of their own as their own area, and says when all files are new", () => {
    const areas = whatChanged(fromDiff([diffItem("e2e/login.spec.ts", "test_only", ["+a"]), diffItem("src/cache/lru.ts", "logic", ["+a"], "added"), diffItem("src/cache/ttl.ts", "logic", ["+a"], "added")]));
    expect(areas.map((a) => a.sentence)).toEqual(["Behaviour changes in src/cache (2 files), all new, without tests.", "Test changes in e2e (1 file)."]);
  });

  it("does not depend on input order", () => {
    const a = [diffItem("src/a/x.ts", "logic", ["+1"]), diffItem("src/b/y.ts", "logic", ["+1"])];
    expect(whatChanged(fromDiff(a))).toEqual(whatChanged(fromDiff([...a].reverse())));
  });
});
