import type { ReactNode } from "react";
import { prerender } from "react-dom/static";
import { describe, expect, it, vi } from "vitest";

const state = vi.hoisted(() => {
  process.env.BEE_ADAPTER = "fixture";
  process.env.BEE_TOKEN = "demo-token-1";
  process.env.BEE_FIXTURE_LIVE = "0";
  process.env.BEE_EDITION = "cloud";
  return { mode: "keeper" };
});

vi.mock("next/headers", () => ({
  cookies: async () => ({ get: (name: string) => name === "bee_mode" ? { value: state.mode } : undefined }),
  headers: async () => new Headers(),
}));

vi.mock("next/navigation", () => ({
  useRouter: () => ({ refresh: () => {} }),
  notFound: () => { throw new Error("not found"); },
}));

const params = Promise.resolve({ org: "acme" });

async function render(node: ReactNode) {
  const { prelude } = await prerender(node);
  const html = await new Response(prelude as unknown as BodyInit).text();
  expect(html.match(/<h1[ >]/g)).toHaveLength(1);
  return {
    html,
    text: html.replace(/<script[\s\S]*?<\/script>/g, "").replace(/<[^>]+>/g, " ").replace(/\s+/g, " "),
  };
}

describe.each(["keeper", "bee"] as const)("organization settings in %s mode", (mode) => {
  it("lists the organization and links to repository rules", async () => {
    state.mode = mode;
    const { default: Page } = await import("@/app/(shell)/orgs/[org]/settings/page");
    const { html, text } = await render(await Page({ params }));
    expect(text).toContain("Acme Robotics");
    expect(text).toContain("acme/api");
    expect(text).toContain("Private");
    if (mode === "bee") {
      expect(html).toContain('data-bee-page="org-settings"');
      expect(html).toContain('data-tool="repo_list_repos"');
      expect(text).toContain("rules at /acme/api/settings");
    } else {
      expect(html).toContain('href="/acme/api/settings"');
      expect(html).toContain('href="/orgs/acme/settings/apps"');
      expect(html).toContain('href="/orgs/acme/settings/billing"');
    }
  });

  it("keeps app permissions and the installation's account attribution", async () => {
    state.mode = mode;
    const { default: Page } = await import("@/app/(shell)/orgs/[org]/settings/apps/page");
    const { html, text } = await render(await Page({ params }));
    expect(text).toContain("Release notes");
    expect(text).toContain("release-notes");
    expect(text).toContain("releases:write");
    expect(text).toContain("Active");
    expect(text).toMatch(/installed by Mara Okafor/i);
    if (mode === "bee") {
      expect(html).toContain('data-bee-page="org-apps"');
      expect(html).toContain('data-tool="app_list_apps"');
    } else {
      expect(html).toContain('data-testid="app-list"');
      expect(html).toContain('data-state="open"');
    }
  });

  it("shows the plan, metered usage and seats, and marks a plan change for confirmation", async () => {
    state.mode = mode;
    const { default: Page } = await import("@/app/(shell)/orgs/[org]/settings/billing/page");
    const { html, text } = await render(await Page({ params }));
    expect(text).toContain("Team");
    expect(text).toContain("Monthly");
    expect(text).toContain("runner minutes");
    expect(text).toContain("Total charged");
    expect(text).toContain("12 purchased");
    if (mode === "bee") {
      expect(html).toContain('data-bee-page="org-billing"');
      expect(html).toContain('data-tool="billing_query_billing_usage"');
      expect(html).toContain('data-tool="billing_change_billing_plan" data-kind="needs a person"');
      expect(html).not.toContain("<button");
    } else {
      expect(html).toContain('data-agent-confirm="Changes the billing plan, seats or cycle for acme."');
      expect(text).toContain("Review change");
      expect(text).toContain("$4.00 per seat per month");
    }
  });
});
