import { describe, expect, it } from "vitest";
import { renderMarkdown } from "@/components/markdown";
import type { ChangeRequest, Issue, Principal } from "./api/types";
import { AGENT, change, LIN, MARA } from "./brief-samples";
import { expandIssueDraft, issueLabels, issueLine, issueWork, linkedPulls, needsIssueDetail, pullsText } from "./issues";

const issue = (over: Partial<Issue> = {}): Issue => ({ id: "i1", repoId: "r", number: 15, title: "Tax lines drift by a cent", body: "", state: "open", type: "bug", labels: ["billing"], milestone: "", assignees: [], author: LIN, parentNumber: 0, subIssues: [], subIssuesTotal: 0, subIssuesDone: 0, linkedChanges: [], comments: [], commentCount: 0, ...over });
const directory = new Map<string, Principal>([[MARA.id, MARA]]);

describe("issues", () => {
  it("says who is working on an issue, and who answers for an agent", () => {
    expect(issueWork(issue({ assignees: [AGENT] }), directory)).toMatchObject({ text: "triage is working on it", person: MARA, personIsAssignee: false, taken: true });
    expect(issueWork(issue({ assignees: [LIN] }), directory)).toMatchObject({ text: "Lin Chen has it", person: LIN, personIsAssignee: true });
    expect(issueWork(issue(), directory)).toMatchObject({ text: "Nobody has it yet", taken: false });
    expect(issueWork(issue(), directory).person).toBeUndefined();
    // An agent's owner the page could not read is not named.
    expect(issueWork(issue({ assignees: [AGENT] })).person).toBeUndefined();
    expect(issueWork(issue({ assignees: [AGENT], state: "closed" }), directory).text).toBe("triage worked on it");
    expect(issueWork(issue({ state: "closed" })).text).toBe("Closed with nobody assigned");
  });

  it("names the linked pull requests by number and state", () => {
    const changes: ChangeRequest[] = [change({ id: "a", number: "105" }), change({ id: "b", number: "102", state: "CHANGE_STATE_LANDED" })];
    expect(linkedPulls(issue({ linkedChanges: ["b", "gone", "a"] }), changes).map((c) => c.id)).toEqual(["b", "a"]);
    expect(pullsText([])).toBe("");
    expect(pullsText(changes.slice(0, 1))).toBe("pull request #105 (open)");
    expect(pullsText(changes)).toBe("pull requests #105 (open) and #102 (merged)");
  });

  it("writes an issue as one line for an agent", () => {
    expect(issueLine(issue({ assignees: [AGENT], linkedChanges: ["a"], subIssuesTotal: 2, subIssuesDone: 1 }), directory, [change({ id: "a", number: "105" })])).toBe(
      "#15 · Tax lines drift by a cent · open · triage is working on it for Mara Okafor · asked by Lin Chen · pull request #105 (open) · bug · billing · 1 of 2 sub-issues done",
    );
  });
});

describe("issue draft", () => {
  it("keeps a short request as the title and splits a Markdown paste at the first line", () => {
    expect(needsIssueDetail("Trace invoice totals")).toBe(false);
    expect(expandIssueDraft(" Trace invoice totals ")).toEqual({ title: "Trace invoice totals", body: "" });
    const request = "Trace invoice totals\r\n\r\n**Expected**\r\n\r\n- Sum each line";
    expect(needsIssueDetail(request)).toBe(true);
    expect(expandIssueDraft(request)).toEqual({ title: "Trace invoice totals", body: "**Expected**\r\n\r\n- Sum each line" });
  });

  it("preserves a long single-line request in full", () => {
    const request = "Compare invoice totals ".repeat(8).trim();
    expect(needsIssueDetail(request)).toBe(true);
    const draft = expandIssueDraft(request);
    expect(draft.title.length).toBeGreaterThan(0);
    expect(draft.title.length).toBeLessThanOrEqual(120);
    expect(request.startsWith(draft.title + " ")).toBe(true);
    expect(draft.body).toBe(request);
  });

  it("renders a draft with a literal pipe before a Markdown table", () => {
    const request = "| Write down the current totals.\n| item | total |\n|---|---|\n| tax | 5 |";
    expect(renderMarkdown(request)).toBe("<p>| Write down the current totals.</p>\n<table><thead><tr><th>item</th><th>total</th></tr></thead><tbody><tr><td>tax</td><td>5</td></tr></tbody></table>");
  });

  it("keeps entered label names in order, once each", () => {
    expect(issueLabels(" billing, ,needs review, billing ")).toEqual(["billing", "needs review"]);
  });
});
