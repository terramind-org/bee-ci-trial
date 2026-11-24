import { describe, expect, it } from "vitest";
import { pathBytes } from "./api/ids";
import { mcpToolName, toCli, toCurl, toMcp } from "./copyas";
import { relativeTime, shortId, initials, bytes, usd, duration } from "./format";

const repo = { fullName: "acme/api" };

describe("copy as", () => {
  it("renders the CLI form with repo and flags", () => {
    expect(toCli({ service: "ChangeService", method: "Enqueue", request: { repo, changeRequestId: "01ABC" } })).toBe("bee cr land 01ABC --repo acme/api --queue --json");
    expect(toCli({ service: "ChangeService", method: "SubmitReview", request: { repo, changeRequestId: "01ABC", revision: 3, verdict: "VERDICT_APPROVE", body: "Ship it, it's fine" } })).toBe(
      "bee review 01ABC --repo acme/api --rev 3 --approve --body 'Ship it, it'\\''s fine' --json",
    );
    expect(toCli({ service: "LeaseService", method: "AcquireLease", request: { repo, paths: [pathBytes("src/auth")], mode: "LEASE_MODE_EXCLUSIVE", ttlSeconds: 3600 } })).toBe(
      "bee lease acquire src/auth --repo acme/api --exclusive --ttl 3600s --json",
    );
  });

  it("falls back to bee api for methods without a dedicated command", () => {
    expect(toCli({ service: "OrgService", method: "GetOrg", request: { nameOrId: "acme" } })).toBe('bee api bee.v1.OrgService/GetOrg --data \'{"nameOrId":"acme"}\'');
  });

  it("names MCP tools <service>_<method>", () => {
    expect(mcpToolName("ChangeService", "EvaluateLanding")).toBe("change_evaluate_landing");
    expect(mcpToolName("AuthService", "WhoAmI")).toBe("auth_who_am_i");
    const call = JSON.parse(toMcp({ service: "LeaseService", method: "ListLeases", request: { repo } }));
    expect(call).toMatchObject({ method: "tools/call", params: { name: "lease_list_leases", arguments: { repo } } });
  });

  it("renders curl for unary and an exact Connect envelope for streaming calls", () => {
    expect(toCurl({ service: "ChangeService", method: "GetChange", request: { repo, id: "X" } })).toContain('-d \'{"repo":{"fullName":"acme/api"},"id":"X"}\'');
    const stream = toCurl({ service: "EventService", method: "Subscribe", request: { repo } });
    const len = JSON.stringify({ repo }).length;
    expect(stream).toContain("printf '\\x00\\x00\\x00\\x00\\x" + len.toString(16).padStart(2, "0") + "%s'");
    expect(stream).toContain("application/connect+json");
  });
});

describe("format", () => {
  const now = Date.UTC(2026, 9, 18, 9, 0, 0);
  it("formats relative times", () => {
    expect(relativeTime(new Date(now - 10_000).toISOString(), now)).toBe("just now");
    expect(relativeTime(new Date(now - 5 * 60_000).toISOString(), now)).toBe("5m ago");
    expect(relativeTime(new Date(now + 2 * 3_600_000).toISOString(), now)).toBe("in 2h");
    expect(relativeTime(new Date(now - 3 * 86_400_000).toISOString(), now)).toBe("3d ago");
  });
  it("formats ids, initials, sizes, money and durations", () => {
    expect(shortId("01JAC7Q8M2RX0Z5B6M4ARA0K1E")).toBe("ra0k1e");
    expect(initials("Mara Okafor")).toBe("MO");
    expect(initials("billing-fixer")).toBe("BF");
    expect(initials("triage")).toBe("TR");
    expect(bytes(18_400_000)).toBe("18 MB");
    expect(usd(410_000)).toBe("$0.41");
    expect(duration(94_000)).toBe("1m 34s");
  });
});
