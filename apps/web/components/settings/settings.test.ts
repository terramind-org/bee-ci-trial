import type { ReactNode } from "react";
import { prerender } from "react-dom/static";
import { describe, expect, it, vi } from "vitest";
import SettingsPage from "@/app/(repo)/[owner]/[repo]/settings/page";

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
  useRouter: () => ({ refresh: () => {}, push: () => {}, replace: () => {} }),
  usePathname: () => "/acme/api/settings",
  useSearchParams: () => new URLSearchParams(),
  notFound: () => { throw new Error("not found"); },
}));

async function settings(mode: "keeper" | "bee", section = "") {
  state.mode = mode;
  const node: ReactNode = await SettingsPage({
    params: Promise.resolve({ owner: "acme", repo: "api" }),
    searchParams: Promise.resolve({ section }),
  });
  const { prelude } = await prerender(node);
  const html = await new Response(prelude as unknown as BodyInit).text();
  expect(html.match(/<h1\b/g)).toHaveLength(1);
  const visible = html.replace(/<(script|style)[\s\S]*?<\/\1>|<[^>]+>/g, " ").replace(/\s+/g, " ");
  expect(visible).not.toMatch(/\b(?:Trellis(?:Hub)?|Nucleus|Terramind|Landed|Ready to land|Change requests?|Changesets?)\b/i);
  return { html, visible };
}

describe("repository settings on the fixture", () => {
  it("opens Rules by default, showing effective rules before the draft", async () => {
    const { html, visible } = await settings("keeper");
    expect(html).toMatch(/<a(?=[^>]*href="\/acme\/api\/settings")(?=[^>]*aria-current="page")[^>]*>Rules<\/a>/);
    expect(visible).toMatch(/Rules Instructions Access Visibility More settings/);
    expect(visible).toMatch(/needs two approvals from a person/);
    expect(visible).toContain("Agents may merge generated files, docs, comments, tests and file moves on their own when targeting main.");
    expect(visible).toContain("Rules change through a pull request, so the current rules decide who may change them.");
    expect(html).not.toContain("Policy source");
  });

  it("gives an agent the same rule sentences in the same section order and names the tools", async () => {
    const { html, visible } = await settings("bee");
    expect(html).toContain('data-bee-page="settings"');
    expect([...html.matchAll(/data-bee-group="([^"]+)"/g)].map((m) => m[1])).toEqual([
      "rules", "instructions", "access", "visibility", "more settings", "tools",
    ]);
    expect(visible).toMatch(/needs two approvals from a person/);
    expect(visible).toContain("Agents may merge generated files, docs, comments, tests and file moves on their own when targeting main.");
    expect(html).toContain('data-tool="repo_update_repo" data-kind="needs a person"');
    expect(html).toContain('data-tool="change_open_change" data-kind="write"');
    expect(html).not.toContain("<button");
  });

  it("renders the resolved Markdown and links to its file, then shows access and visibility on their own sections", async () => {
    const instructions = await settings("keeper", "instructions");
    expect(instructions.html).toMatch(/href="\/acme\/api\/code\?path=AGENTS.md(?:&amp;[^"]*)?"[^>]*>AGENTS.md/);
    expect(instructions.html).toMatch(/<div class="prose-bee[^"]*">[\s\S]*<(h[4-6]|p|ul)\b/);
    const access = await settings("keeper", "access");
    expect(access.visible).toContain("Mara Okafor");
    expect(access.visible).toMatch(/writing to \/src\/auth/);
    expect(access.visible).toContain("The Platform team can read, write, review pull requests and manage settings.");
    expect(access.visible).toContain("Deploy keys");
    expect(access.html).toContain('href="/acme/api/agents"');
    const visibility = await settings("keeper", "visibility");
    expect(visibility.visible).toContain("Anyone can read this repository.");
    expect(visibility.visible).toContain("Only people and agents with access can read this repository.");
    expect(visibility.html).toMatch(/data-agent-confirm="Makes acme\/api (public|private)/);
    const more = await settings("keeper", "more");
    expect(more.html).toContain('href="/orgs/acme/settings"');
    expect(more.html).toContain('href="/orgs/acme/settings/apps"');
  });
});
