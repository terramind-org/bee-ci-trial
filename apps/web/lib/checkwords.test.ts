import { describe, expect, it } from "vitest";
import { checksClause, checksSentence, countWord, groupChecks, listing, shortListing, tallyChecks } from "./checkwords";
import { check } from "./brief-samples";

describe("checkwords", () => {
  it("joins names as prose", () => {
    expect(listing([])).toBe("");
    expect(listing(["a"])).toBe("a");
    expect(listing(["a", "b"])).toBe("a and b");
    expect(listing(["a", "b", "c"])).toBe("a, b and c");
    expect(shortListing(["a", "b", "c", "d", "e"])).toBe("a, b, c and 2 more");
    expect(countWord(0)).toBe("no");
    expect(countWord(3)).toBe("three");
    expect(countWord(14)).toBe("14");
  });

  it("groups by outcome, failures first, names sorted, the last run of a name counting", () => {
    const groups = groupChecks([check("test", "CHECK_STATUS_FAILED"), check("lint", "CHECK_STATUS_PASSED"), check("e2e", "CHECK_STATUS_RUNNING"), check("build", "CHECK_STATUS_PASSED"), check("test", "CHECK_STATUS_PASSED"), check("docs", "CHECK_STATUS_SKIPPED")]);
    expect(groups.map((g) => g.state)).toEqual(["running", "passed", "skipped"]);
    expect(groups[0]?.sentence).toBe("E2e is still running.");
    expect(groups[1]?.sentence).toBe("Build, lint and test passed.");
    expect(groups[2]?.sentence).toBe("Docs was skipped.");
  });

  it("says the whole set in one clause", () => {
    expect(checksClause([])).toBe("no checks have run");
    const all = Array.from({ length: 14 }, (_, i) => check("c" + String(i).padStart(2, "0"), "CHECK_STATUS_PASSED"));
    expect(checksClause(all)).toBe("all 14 checks pass");
    expect(checksSentence([check("only", "CHECK_STATUS_PASSED")])).toBe("The one check passes.");
    expect(checksClause([check("a", "CHECK_STATUS_PASSED"), check("b", "CHECK_STATUS_SKIPPED")])).toBe("the one check passes (1 skipped)");
    expect(checksClause([check("test", "CHECK_STATUS_FAILED"), check("lint", "CHECK_STATUS_PASSED")])).toBe("test check failed");
    expect(checksClause([check("e2e", "CHECK_STATUS_QUEUED"), check("lint", "CHECK_STATUS_PASSED")])).toBe("1 of 2 checks still running");
    expect(tallyChecks([check("x", "CHECK_STATUS_UNSPECIFIED")]).pending).toEqual(["x"]);
  });
});
