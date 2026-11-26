import { createElement as h } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import type { ChangeRequest, FeedEvent, Principal } from "@/lib/api/types";
import type { Need, SafetyNeed } from "@/lib/inbox/needs";
import { EmptyInbox, headline, mergedSentence, needLine, safetyNeedLine } from "./inbox";
import { NeedsList } from "./needs-list";

const mara: Principal = { id: "mara", kind: "PRINCIPAL_KIND_HUMAN", display: "Mara Okafor", ownerId: "", runtime: "", externalIds: [] };
const triage: Principal = { id: "triage", kind: "PRINCIPAL_KIND_AGENT", display: "triage", ownerId: "mara", runtime: "claude-code", externalIds: [] };
const change: ChangeRequest = { id: "cr1", repoId: "r", targetRef: "main", changeIds: [], state: "CHANGE_STATE_OPEN", author: triage, title: "Rotate session ids", body: "", revisions: [{ n: 1, diffClass: "sensitive", createdAt: "2026-10-20T00:00:00Z" }], dependsOn: "", number: "101" };
const need = (over: Partial<Need> = {}): Need => ({ kind: "review", repo: "acme/api", change, who: triage, mine: false, revisedAfterYou: false, failing: [], reasons: [], missing: ["approval:human:sensitive (1 of 2)"], owners: {}, branch: "ws/triage/rotate-sessions", answers: mara, at: "2026-10-20T00:00:00Z", ...over });
const land = (id: string, actor: Principal, repo: string): FeedEvent => ({ id, kind: "land", actor, subject: { kind: "SUBJECT_KIND_CHANGESET", repo: { fullName: repo }, id }, summary: "", payload: "", createdAt: "2026-10-20T00:00:00Z" }) as FeedEvent;

describe("EmptyInbox", () => {
  it("shows the setup-repository prompt with the first repository actions", () => {
    const html = renderToStaticMarkup(h(EmptyInbox, { endpoint: "https://bee.example" }));
    expect(html).toContain('data-testid="home-onboarding"');
    expect(html).toContain("Set up Bee for a repository");
    expect(html).toContain('href="/new"');
    expect(html).toContain('href="/import"');
  });
});

describe("Needs you", () => {
  it("says how much needs a person in the page's one large sentence", () => {
    expect(headline([])).toBe("Nothing needs you");
    expect(headline([{ kind: "review" }])).toBe("One pull request needs you");
    expect(headline([{ kind: "review" }, { kind: "failing" }])).toBe("Two pull requests need you");
    expect(headline([{ kind: "review" }, { kind: "post" }])).toBe("Two things need you");
  });

  it("names the repository, number and branch, the rule, the person who answers, and one action", () => {
    const html = renderToStaticMarkup(h(NeedsList, { safety: [], needs: [need(), need({ kind: "failing", failing: ["test"], change: { ...change, id: "cr2", number: "104" }, answers: undefined, branch: undefined })], now: Date.parse("2026-10-20T02:00:00Z") }));
    const rows = html.split("<li").slice(1);
    expect(rows).toHaveLength(2);
    const first = rows[0] ?? "";
    expect(first).toContain("acme/api #101");
    expect(first).toContain("wants to merge");
    expect(first).toContain("A change to a path the rules mark sensitive needs two approvals from a person. It has one.");
    expect(first).toContain("ws/triage/rotate-sessions");
    expect(first).toContain("High risk");
    // The mark sits beside the person who answers, and nowhere else.
    expect(first).toMatch(/class="dot"[^>]*><\/span><span>Mara Okafor answers for it/);
    expect(html.match(/class="dot"/g)).toHaveLength(1);
    // One action per row, and one solid button on the screen.
    expect(first.match(/href="\/acme\/api\/changes\/cr1"/g)).toHaveLength(2);
    expect(html.match(/bg-accent /g)).toHaveLength(1);
    expect(rows[1]).toContain("is stuck on");
    expect(rows[1]).not.toContain("answers for it");
  });

  it("puts safety items first and shows the reason in both modes", () => {
    const item: SafetyNeed = {
      id: "s1", kind: "tripwire", repo: "acme/api", title: "Agent run stopped",
      reason: "The reviewer found customer identifiers in a draft.",
      href: "/acme/api/safety?work=r1&kind=SAFETY_WORK_KIND_RUN",
      answers: mara,
    };
    const html = renderToStaticMarkup(h(NeedsList, { needs: [need()], safety: [item], now: 0 }));
    const rows = html.split("<li").slice(1);
    expect(rows[0]).toContain('data-kind="tripwire"');
    expect(rows[0]).toContain(item.reason);
    expect(rows[0]).toContain("Review safety");
    expect(rows[0]).toContain("Mara Okafor answers for it");
    expect(rows[1]).toContain('data-kind="review"');
    expect(html.match(/bg-accent /g)).toHaveLength(1);
    expect(headline([{ kind: "review" }], 1)).toBe("Two things need you");
    expect(safetyNeedLine(item)).toBe("acme/api · Agent run stopped. The reviewer found customer identifiers in a draft. · Mara Okafor answers for it · /acme/api/safety?work=r1&kind=SAFETY_WORK_KIND_RUN");
  });

  it("gives an agent the same sentence as a line of text", () => {
    expect(needLine(need())).toBe(
      "acme/api #101 · ws/triage/rotate-sessions → main · triage wants to merge “Rotate session ids”. A change to a path the rules mark sensitive needs two approvals from a person. It has one. · Mara Okafor answers for it · high risk · /acme/api/changes/cr1",
    );
  });

  it("counts what merged today and how much of it no person merged", () => {
    expect(mergedSentence([])).toBe("Nothing has merged in your repositories in the last day.");
    const groups = [
      { repo: "acme/api", items: [land("a", triage, "acme/api"), land("b", mara, "acme/api")] },
      { repo: "acme/web", items: [land("c", triage, "acme/web")] },
    ];
    expect(mergedSentence(groups)).toBe("Three commits merged today across two repositories. Two were merged by agents on their own.");
    expect(mergedSentence([{ repo: "acme/api", items: [land("a", triage, "acme/api")] }])).toBe("One commit merged today across one repository. An agent merged it on its own.");
  });
});
