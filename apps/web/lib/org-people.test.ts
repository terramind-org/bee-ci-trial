import { prerender } from "react-dom/static";
import { describe, expect, it, vi } from "vitest";
import { createClient } from "@/lib/api/client";
import { FixtureStore, fixtureRpc } from "@/lib/api/fixture";

const state = vi.hoisted(() => {
  process.env.BEE_ADAPTER = "fixture";
  process.env.BEE_TOKEN = "demo-token-1";
  process.env.BEE_FIXTURE_LIVE = "0";
  return { mode: "keeper" };
});

vi.mock("next/headers", () => ({
  cookies: async () => ({ get: (name: string) => (name === "bee_mode" ? { value: state.mode } : undefined) }),
  headers: async () => new Headers(),
}));

vi.mock("next/navigation", () => ({
  useRouter: () => ({ refresh() {} }),
  usePathname: () => "/orgs/acme/settings/members",
  useSearchParams: () => new URLSearchParams(),
  useSelectedLayoutSegment: () => null,
}));

async function render(slug: "members" | "teams", mode: "keeper" | "bee") {
  state.mode = mode;
  const { default: Page } = slug === "members"
    ? await import("@/app/(shell)/orgs/[org]/settings/members/page")
    : await import("@/app/(shell)/orgs/[org]/settings/teams/page");
  const { prelude } = await prerender(await Page({ params: Promise.resolve({ org: "acme" }) }));
  return new Response(prelude as unknown as BodyInit).text();
}

describe("organisation people and teams", () => {
  it.each(["keeper", "bee"] as const)("shows named people, accountability and team membership in %s mode", async (mode) => {
    const people = await render("members", mode);
    const teams = await render("teams", mode);
    expect(people.match(/<h1\b/g)).toHaveLength(1);
    expect(teams.match(/<h1\b/g)).toHaveLength(1);
    expect(people).toContain("Mara Okafor");
    expect(people).toMatch(/\b[1-9]\d* agents? answers? to Mara Okafor/);
    expect(people).toContain("Platform");
    expect(teams).toContain("@acme/billing");
    expect(teams).toContain("Policy requires its approval on src/billing");
    expect(teams).toContain("Members include");
    expect(teams).toContain("Mara Okafor");
    if (mode === "bee") {
      expect(people).toContain('data-tool="org_set_member" data-kind="needs a person"');
      expect(people).toContain('data-tool="org_create_org_invitation" data-kind="needs a person"');
      expect(teams).toContain('data-tool="team_list_team_members" data-kind="read"');
      expect(people).not.toContain("<button");
      expect(teams).not.toContain("<button");
    } else {
      expect(people).toContain('data-agent-confirm="Invites this person to the organisation"');
      expect(people).toContain('data-agent-confirm="Changes the organisation role for Mara Okafor"');
      expect(people).toContain('data-agent-confirm="Removes Mara Okafor from the organisation"');
    }
  });

  it("passes invitations, role changes and removal through the fixture RPCs", async () => {
    const api = createClient(fixtureRpc(new FixtureStore(), "demo-token-1"));
    const { members } = await api.org.listMembers({ org: "acme" });
    const principalId = members.find((m) => m.principal?.display === "Lin Zhou")?.principal?.id;
    expect(principalId).toBeTruthy();
    const request = { org: "acme", principalId };
    await expect(api.org.setMember({ ...request, role: "ROLE_BILLING_MANAGER" })).resolves.toMatchObject({ role: "ROLE_BILLING_MANAGER" });
    await api.org.setMember({ ...request, role: "ROLE_UNSPECIFIED" });
    expect((await api.org.listMembers({ org: "acme" })).members.some((member) => member.principal?.id === principalId)).toBe(false);
    const invitation = { org: "acme", inviteeEmail: "new@example.test", role: "ROLE_MEMBER" as const };
    await expect(api.org.createOrgInvitation(invitation)).resolves.toMatchObject({ state: "INVITATION_STATE_PENDING" });
    await expect(api.org.createOrgInvitation(invitation)).rejects.toMatchObject({ code: "already_exists" });
  });
});
