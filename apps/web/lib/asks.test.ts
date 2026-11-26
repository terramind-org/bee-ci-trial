import { describe, expect, it } from "vitest";
import { askStates, decisionComment, openAsks, parseDecision } from "./asks";
import { AGENT, comment, LIN, MARA } from "./brief-samples";

const ask = { question: "Sign out other devices when a role changes?", options: ["Only this session", "Every device"] };

describe("asks", () => {
  it("writes and reads the decision comment", () => {
    const body = decisionComment(ask.question, "Every device");
    expect(body).toBe('Decision on "Sign out other devices when a role changes?": Every device');
    expect(parseDecision(body)).toEqual({ question: ask.question, option: "Every device" });
    expect(parseDecision("Decision on “Ship it?”: Yes\nbecause")).toEqual({ question: "Ship it?", option: "Yes" });
    expect(parseDecision('Decision on "Use "fast" mode?": No')).toEqual({ question: 'Use "fast" mode?', option: "No" });
    expect(parseDecision("I think we should decide")).toBeNull();
  });

  it("keeps the latest answer per ask, case and spacing aside", () => {
    const comments = [comment(MARA, decisionComment(ask.question, "Only this session"), "2026-10-27T10:00:00Z"), comment(AGENT, "Thanks.", "2026-10-27T10:01:00Z"), comment(LIN, 'Decision on "sign out OTHER devices  when a role changes?": every device', "2026-10-27T11:00:00Z")];
    const [s] = askStates([ask], comments);
    expect(s?.answer).toBe("Every device");
    expect(s?.by?.display).toBe("Lin Chen");
    expect(s?.at).toBe("2026-10-27T11:00:00Z");
    expect(s?.offList).toBe(false);
    expect(openAsks([ask, { question: "Keep the old flag?", options: ["Yes", "No"] }], comments).map((a) => a.question)).toEqual(["Keep the old flag?"]);
  });

  it("flags an answer the ask no longer offers", () => {
    const [s] = askStates([ask], [comment(MARA, decisionComment(ask.question, "Ask me later"), "2026-10-27T10:00:00Z")]);
    expect(s?.answer).toBe("Ask me later");
    expect(s?.offList).toBe(true);
  });
});
