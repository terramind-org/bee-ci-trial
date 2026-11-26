import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import { LinearRefusal, MergeRefusal } from "../change-actions";
import { isPolicyEjection } from "./live";

describe("merge refusal", () => {
  it("renders the policy reason and its explanation in the same output as the picture", () => {
    const html = renderToStaticMarkup(createElement(MergeRefusal, { eventKey: "attempt-1", text: "Not merged yet: it needs an approval.", reason: "POLICY_DENIED" }));
    expect(html).toContain("<svg");
    expect(html).toContain("<p>Not merged yet: it needs an approval.");
    expect(html).toContain("POLICY_DENIED");
  });

  it("keeps a read-only linear rule visible, and only pictures an actual denial", () => {
    const rule = renderToStaticMarkup(createElement(LinearRefusal));
    const denied = renderToStaticMarkup(createElement(LinearRefusal, { eventKey: "attempt-2" }));
    expect(denied.match(/<svg\b/g)).toHaveLength((rule.match(/<svg\b/g)?.length ?? 0) + 1);
    expect(denied).toContain("This pull request contains a merge commit and the target branch requires linear history.");
    expect(denied).toContain("POLICY_DENIED · linear");
  });

  it("only treats an explicitly policy-denied queue departure as a refusal", () => {
    const kind = "merge_queue.left";
    expect(isPolicyEjection({ kind, payloadJson: '{"eject_reason":"POLICY_DENIED"}' })).toBe(true);
    expect(isPolicyEjection({ kind, payloadJson: '{"eject_reason":"MERGE_CONFLICT"}' })).toBe(false);
    expect(isPolicyEjection({ kind })).toBe(false);
    expect(isPolicyEjection({ kind, payloadJson: "invalid" })).toBe(false);
    expect(isPolicyEjection({ kind: "cr.landed", payloadJson: '{"eject_reason":"POLICY_DENIED"}' })).toBe(false);
  });
});
