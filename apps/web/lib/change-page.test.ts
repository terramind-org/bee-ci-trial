import { describe, expect, it } from "vitest";
import { pathBytes } from "./api/ids";
import type { ChangeBrief, Comment, Evidence, Review } from "./api/types";
import {
  annotationViews,
  annotationsSentence,
  bearerToken,
  changeCodePath,
  changeHref,
  codeHref,
  codeRedirect,
  conversationOf,
  evidenceByCheck,
  riskSentence,
  safeImageLink,
  sinceRevision,
  tabOf,
  unclaimedCiEvidence,
  viewedRevision,
} from "./change-page";

const brief = (over: Partial<ChangeBrief> = {}): ChangeBrief => ({ outcome: "", highlights: [], asks: [], risks: [], rollback: "", tryItUrl: "", ...over });

describe("change page tabs and URLs", () => {
  it("maps segments to tabs, unknown ones to the Brief", () => {
    expect(tabOf(null)).toBe("brief");
    expect(tabOf("code")).toBe("code");
    expect(tabOf("checks")).toBe("checks");
    expect(tabOf("agents")).toBe("agents");
    expect(tabOf("change.md")).toBe("brief");
  });

  it("builds each tab's URL", () => {
    expect(changeHref("acme/api", "01X")).toBe("/acme/api/changes/01X");
    expect(changeHref("acme/api", "01X", "agents")).toBe("/acme/api/changes/01X/agents");
    expect(codeHref("acme/api", "01X", 2, 1)).toBe("/acme/api/changes/01X/code?rev=2&since=1");
    expect(codeHref("acme/api", "01X")).toBe("/acme/api/changes/01X/code");
  });

  it("sends ?rev and ?since to the Code tab, keeping both", () => {
    expect(codeRedirect("acme/api", "01X", {})).toBeNull();
    expect(codeRedirect("acme/api", "01X", { rev: "1" })).toBe("/acme/api/changes/01X/code?rev=1");
    expect(codeRedirect("acme/api", "01X", { rev: "3", since: "1" })).toBe("/acme/api/changes/01X/code?rev=3&since=1");
    expect(codeRedirect("acme/api", "01X", { since: ["2", "9"] })).toBe("/acme/api/changes/01X/code?since=2");
  });

  it("gives the proxy a real redirect for ?rev and ?since on the Brief only", () => {
    const q = (s: string) => new URLSearchParams(s);
    expect(changeCodePath("/acme/api/changes/01X", q("rev=2&since=1&x=y"))).toBe("/acme/api/changes/01X/code?rev=2&since=1");
    expect(changeCodePath("/acme/api/changes/01X", q("x=y"))).toBeNull();
    expect(changeCodePath("/acme/api/changes/01X/code", q("rev=2"))).toBeNull();
    expect(changeCodePath("/api/x/changes/01X", q("rev=2"))).toBeNull();
    expect(changeCodePath("/acme/api/changes", q("rev=2"))).toBeNull();
  });

  it("clamps the viewed revision and ignores a since that is not earlier", () => {
    expect(viewedRevision("", 3)).toBe(3);
    expect(viewedRevision("1", 3)).toBe(1);
    expect(viewedRevision("9", 3)).toBe(3);
    expect(viewedRevision("-2", 3)).toBe(3);
    expect(viewedRevision("x", 3)).toBe(3);
    expect(viewedRevision("1", 0)).toBe(0);
    expect(sinceRevision("1", 3)).toBe(1);
    expect(sinceRevision("3", 3)).toBe(0);
    expect(sinceRevision("", 3)).toBe(0);
  });

  it("reads a Bearer token and nothing else", () => {
    expect(bearerToken("Bearer abc.def")).toBe("abc.def");
    expect(bearerToken("bearer abc")).toBe("abc");
    expect(bearerToken("Basic abc")).toBeUndefined();
    expect(bearerToken("Bearer a b")).toBeUndefined();
    expect(bearerToken(null)).toBeUndefined();
  });
});

describe("riskSentence", () => {
  it("names the class and what the brief offers", () => {
    expect(riskSentence("sensitive", brief({ risks: [{ text: "a", mitigation: "m" }, { text: "b", mitigation: "n" }], rollback: "Revert." }))).toBe(
      "High risk: it touches a path the repository marks sensitive. The author lists two risks with mitigations and a rollback.",
    );
    expect(riskSentence("logic", brief({ risks: [{ text: "a", mitigation: "" }, { text: "b", mitigation: "n" }] }))).toBe("Some risk: it changes how the code behaves. The author lists two risks, one mitigated.");
    expect(riskSentence("logic", undefined)).toBe("Some risk: it changes how the code behaves. The author names no risks and no rollback.");
  });

  it("says nothing more for low-risk classes without a brief", () => {
    expect(riskSentence("docs", undefined)).toBe("Low risk: only documentation changed.");
    expect(riskSentence("", undefined)).toBe("Some risk: the change is not classified yet. The author names no risks and no rollback.");
  });
});

describe("conversationOf", () => {
  it("merges comments and reviews oldest first, ties by id", () => {
    const c = (id: string, at: string): Comment => ({ id, body: id, createdAt: at, edited: false });
    const r = (id: string, at: string): Review => ({ id, changeId: "x", revision: 1, verdict: "VERDICT_APPROVE", body: "", comments: [], createdAt: at }) as Review;
    const items = conversationOf([c("c2", "2026-10-02T00:00:00Z"), c("c1", "2026-10-01T00:00:00Z")], [r("r1", "2026-10-01T12:00:00Z"), r("a0", "2026-10-02T00:00:00Z")]);
    expect(items.map((i) => (i.kind === "comment" ? i.comment.id : i.review.id))).toEqual(["c1", "r1", "a0", "c2"]);
  });
});

describe("check output", () => {
  it("orders annotations failures first and says them in words", () => {
    const views = annotationViews([
      { path: pathBytes("src/b.ts"), startLine: 4, level: "ANNOTATION_LEVEL_WARNING", message: "w" },
      { path: pathBytes("src/a.ts"), startLine: 9, level: "ANNOTATION_LEVEL_FAILURE", message: "f", title: "T" },
      { path: pathBytes("src/a.ts"), startLine: 2, level: "ANNOTATION_LEVEL_FAILURE", message: "g" },
    ]);
    expect(views.map((v) => v.path + ":" + v.line + ":" + v.level)).toEqual(["src/a.ts:2:failure", "src/a.ts:9:failure", "src/b.ts:4:warning"]);
    expect(annotationsSentence(views)).toBe("Two failures and one warning point at code.");
    expect(annotationsSentence(views.slice(2))).toBe("One warning points at code.");
    expect(annotationsSentence([])).toBe("");
  });

  it("links check images only over https", () => {
    expect(safeImageLink("https://ci.example/shot.png")).toBe("https://ci.example/shot.png");
    expect(safeImageLink("http://ci.example/shot.png")).toBeNull();
    expect(safeImageLink("javascript:alert(1)")).toBeNull();
    expect(safeImageLink(undefined)).toBeNull();
  });

  it("files CI evidence under its check run, and keeps the rest apart", () => {
    const e = (id: string, source: Evidence["source"], checkRunId = ""): Evidence => ({ id, source, checkRunId }) as Evidence;
    const items = [e("1", "EVIDENCE_SOURCE_CHECK_RUN", "chk"), e("2", "EVIDENCE_SOURCE_UPLOADED"), e("3", "EVIDENCE_SOURCE_WORKFLOW_ARTIFACT"), e("4", "EVIDENCE_SOURCE_CHECK_RUN", "old")];
    expect([...evidenceByCheck(items).keys()]).toEqual(["chk", "old"]);
    expect(unclaimedCiEvidence(items, [{ id: "chk" }]).map((x) => x.id)).toEqual(["3", "4"]);
  });
});
