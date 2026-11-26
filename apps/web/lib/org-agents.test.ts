import type { ReactNode } from "react";
import { prerender } from "react-dom/static";
import { describe, expect, it, vi } from "vitest";
import { asCount, asMicros, dollars } from "@/components/org/agents-budget";

const state = vi.hoisted(() => {
  process.env.BEE_ADAPTER = "fixture";
  process.env.BEE_TOKEN = "demo-token-1";
  process.env.BEE_FIXTURE_LIVE = "0";
  return { mode: "keeper" };
});

vi.mock("next/headers", () => ({
  cookies: async () => ({ get: (name: string) => name === "bee_mode" ? { value: state.mode } : undefined }),
  headers: async () => new Headers(),
}));

vi.mock("next/navigation", () => ({
  useRouter: () => ({ refresh: () => {} }),
  usePathname: () => "/orgs/acme/settings/agents",
  useSearchParams: () => new URLSearchParams(),
  useSelectedLayoutSegment: () => null,
  notFound: () => { throw new Error("not found"); },
}));

async function html(node: ReactNode) {
  const { prelude } = await prerender(node);
  return new Response(prelude as unknown as BodyInit).text();
}

describe("organization agents in Acme", () => {
  it.each(["keeper", "bee"])("renders the directory, budgets, tokens and leases in %s mode", async (mode) => {
    state.mode = mode;
    const { default: Page } = await import("@/app/(shell)/orgs/[org]/settings/agents/page");
    const markup = await html(await Page({ params: Promise.resolve({ org: "acme" }) }));
    const copy = markup.replace(/<!--.*?-->|<[^>]+>/g, " ").replace(/\s+/g, " ");
    expect(markup.match(/<h1\b/g)).toHaveLength(1);
    expect(copy).toContain("nightly-import");
    expect(copy).toContain("No person answers for this agent");
    expect(copy).toContain("Mara Okafor");
    expect(copy).toContain("claude-code");
    expect(copy).toContain("claude-opus-5");
    expect(copy).toContain("recorded in the last 7 days");
    expect(copy).toContain("triage · main token");
    expect(copy).toContain("commits 61 used 200 limit");
    expect(copy).toContain("src/auth");
    expect(copy).toContain("$500.00 limit");
    expect(copy).toContain("Token details are visible to the agent and its owner.");
    expect(copy).not.toContain("billing-fixer · main token");
    if (mode === "keeper") {
      expect(markup).toContain("data-mark");
      expect(markup).toContain('data-agent-confirm="Revokes triage · main token and tokens derived from it"');
      expect(copy).toContain("Shared monthly budgets");
    } else {
      expect(markup).toContain('data-bee-page="org-agents"');
      expect(markup).toContain('data-tool="agent_get_agent_budget" data-kind="read"');
      expect(markup).toContain('data-tool="agent_set_agent_budget" data-kind="needs a person"');
      expect(markup).not.toContain("<button");
    }
  });
});

describe("monthly budget amounts", () => {
  it("preserves exact amounts and stays within the wire integer range", () => {
    expect(asMicros("1.000001")).toBe("1000001");
    expect(dollars("1000001")).toBe("1.000001");
    expect(asMicros("18446744073709.551615")).toBe("18446744073709551615");
    expect(asMicros("18446744073709.551616")).toBeNull();
    expect(asMicros("1.0000001")).toBeNull();
    expect(asCount("0")).toBe("0");
    expect(asCount("-1")).toBeNull();
    expect(asCount("18446744073709551616")).toBeNull();
  });
});
