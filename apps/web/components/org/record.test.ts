import { prerender } from "react-dom/static";
import { describe, expect, it, vi } from "vitest";
import { verificationLine } from "./record";

const state = vi.hoisted(() => {
  process.env.BEE_ADAPTER = "fixture";
  process.env.BEE_TOKEN = "mara-token-0001";
  process.env.BEE_FIXTURE_LIVE = "0";
  return { mode: "keeper" };
});

vi.mock("next/headers", () => ({
  cookies: async () => ({ get: (name: string) => (name === "bee_mode" ? { value: state.mode } : undefined) }),
  headers: async () => new Headers(),
}));
vi.mock("next/navigation", () => ({
  useRouter: () => ({ refresh: () => {}, push: () => {}, replace: () => {} }),
  usePathname: () => "/orgs/acme/settings/audit",
  useSearchParams: () => new URLSearchParams(),
  useSelectedLayoutSegment: () => null,
  notFound: () => { throw new Error("not found"); },
}));

async function renderRecord(mode: "keeper" | "bee", filters: Record<string, string> = {}) {
  state.mode = mode;
  const { default: RecordPage } = await import("@/app/(shell)/orgs/[org]/settings/audit/page");
  const node = await RecordPage({ params: Promise.resolve({ org: "acme" }), searchParams: Promise.resolve(filters) });
  const { prelude } = await prerender(node);
  const html = await new Response(prelude as unknown as BodyInit).text();
  const text = html.replace(/<!--[^>]*-->|<[^>]+>/g, "").replace(/\s+/g, " ");
  return { html, text };
}

describe("the organisation record on the demo world", () => {
  it.each(["keeper", "bee"] as const)("shows the authenticated chain and audit facts in %s mode", async (mode) => {
    const { html, text } = await renderRecord(mode, { action: "change.land", outcome: "refused" });
    expect(html.match(/<h1\b/g)).toHaveLength(1);
    expect(text).toMatch(/The record is intact up to entry \d+\./);
    expect(text).toContain("triage on behalf of Mara Okafor requested to merge a pull request");
    expect(text).toContain("refused under land.main");
    expect(text).toContain("UTC");
    expect(text).toContain("Previous hash");
    expect(text).not.toContain("Tighten token audience check");
    if (mode === "bee") {
      expect(html).toContain('data-bee-page="org-record"');
      expect(html).toContain('data-tool="audit_query_audit_log" data-kind="read"');
      expect(html).toContain('data-tool="audit_verify_audit_chain" data-kind="read"');
      expect(html).not.toContain("<button");
    } else {
      expect(html).toContain('class="dot"');
      expect(html).toContain('name="actor"');
      expect(html).toContain('name="outcome"');
    }
  });

  it("keeps the verified range when a filter matches nothing", async () => {
    const full = await renderRecord("bee");
    const empty = await renderRecord("bee", { action: "no.such.action" });
    const chain = full.text.match(/The record is intact up to entry \d+\./)?.[0] ?? "";
    expect(chain).not.toBe("");
    expect(empty.text).toContain(chain);
    expect(empty.text).toContain("No entries match.");
  });

  it("reports the verifier's failing entry and reason", () => {
    expect(verificationLine({
      entries: [],
      chainVerified: false,
      verifiedToSeq: "2",
      firstBrokenSeq: "3",
      verificationFailure: "prev_hash_mismatch",
    })).toBe("The record failed verification at entry 3 because the link to the previous entry does not match.");
  });
});
