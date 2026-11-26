import { describe, expect, it } from "vitest";
import { policyRules } from "./policy-rules";

describe("effective policy in words", () => {
  it("keeps the branch, path, approver and check constraints in the sentences", () => {
    const normalized = JSON.stringify({
      land: {
        release: {
          approvals: [{ paths: ["src/auth"], classes: [], count: 2, from: "any", include_agents: false }],
          required_checks: ["test", "lint"],
          agents_may_land_without_human: ["docs", "test_only"],
          require_signed: true,
          require_linear: false,
        },
      },
      access: { "/src": { read: null, write: ["@security"] } },
      paths: { exclusive_lease: ["/src/auth"] },
    });

    expect(policyRules(normalized)).toEqual([
      { text: "A change to src/auth needs two approvals from a person to merge into release." },
      { text: "Merges into release require the test and lint checks." },
      { text: "Agents may merge docs and tests on their own when targeting release." },
      { text: "Every commit must have a verified signature to merge into release." },
      { text: "The rule for /src limits write access to @security." },
      { text: "Changes under /src/auth need an exclusive lease to merge." },
    ]);
  });

  it("retains unsupported rules and malformed payloads in their raw form", () => {
    const futureApproval = { paths: [], classes: ["docs"], count: 1, from: "any", include_agents: false, quorum: true };
    const output = policyRules(JSON.stringify({
      land: { main: { approvals: [futureApproval], required_checks: { optional: ["unit"] } } },
      future_rule: [],
    }));
    expect(output).toEqual([
      { text: "land.main.approvals = " + JSON.stringify(futureApproval, null, 2), raw: true },
      { text: 'land.main.required_checks = {\n  "optional": [\n    "unit"\n  ]\n}', raw: true },
      { text: "future_rule = []", raw: true },
    ]);
    expect(policyRules("an unrecognized policy format")).toEqual([{ text: "an unrecognized policy format", raw: true }]);
  });

  it("shows explicit empty allow lists as denials", () => {
    expect(policyRules(JSON.stringify({
      land: { main: { allow: [], agents_may_land_without_human: [] } },
      access: { "/secrets": { read: [], write: null } },
    }))).toEqual([
      { text: "Merging into main is prohibited." },
      { text: "Agents need a person's approval to merge into main." },
      { text: "The rule for /secrets denies read access." },
    ]);
  });
});
