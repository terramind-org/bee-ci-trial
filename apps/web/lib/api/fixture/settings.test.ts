import { describe, expect, it } from "vitest";
import { createClient } from "../client";
import { wireOf } from "../ops";
import { FixtureStore, fixtureRpc } from "./index";
import { POLICY_PATH } from "./policy";

const repo = { fullName: "acme/api" };
const world = () => {
  const store = new FixtureStore({ clock: () => 0, live: false });
  const api = createClient(fixtureRpc(store, "demo-token-1"));
  return { api, store };
};

describe("repository settings operations", () => {
  it("preserves the requested revision and replay count in the wire requests", () => {
    expect(wireOf("AgentService", "ResolveAgentInstructions", { repo, revision: "main" })).toMatchObject({
      method: "ResolveAgentInstructions", request: { repo, revision: { expr: "main" } },
    });
    expect(wireOf("PolicyService", "EvaluatePolicy", { repo, source: "version = 1", sample: 11 })).toMatchObject({
      method: "ValidatePolicy", request: { repo, toml: "version = 1", replaySample: 11 },
    });
  });

  it("returns normalized rules and access alongside the repository policy source", async () => {
    const { api } = world();
    const policy = await api.policy.getPolicy({ repo });
    expect(policy.path).toBe(POLICY_PATH);
    expect(policy.source).toContain('write = ["@security", "triage"]');
    expect(JSON.parse(policy.normalizedJson)).toMatchObject({
      access: {
        "/": { read: ["@acme/members"], write: ["@engineers", "@agents"] },
        "/src/auth": { read: null, write: ["@security", "triage"] },
      },
      land: { main: { required_checks: ["test", "lint"], approvals: expect.arrayContaining([
        { classes: ["sensitive"], paths: [], count: 2, from: "any", include_agents: false },
      ]) } },
    });
    const evaluated = await api.policy.evaluatePolicy({ repo, source: policy.source, sample: 3 });
    expect(evaluated.valid).toBe(true);
    expect(evaluated.normalizedJson).toBe(policy.normalizedJson);
    expect(evaluated.decisions).toHaveLength(3);
  });

  it("returns resolved Markdown with decoded paths and the resolved commit", async () => {
    const { api } = world();
    const instructions = await api.agent.resolveAgentInstructions({ repo, revision: "main" });
    expect(instructions.files).toMatchObject([
      { path: "AGENTS.md", source: "AGENT_INSTRUCTION_SOURCE_AGENTS_MD", content: expect.stringContaining("exclusive lease") },
    ]);
    expect(instructions.combined).toContain("<!-- AGENTS.md -->");
    expect(instructions.changeset?.raw).toBeTruthy();
  });

  it("paginates collaborators and returns named team grants and deploy keys", async () => {
    const { api } = world();
    const first = await api.repo.listCollaborators({ repo, page: { pageSize: 1 } });
    expect(first.collaborators).toHaveLength(1);
    expect(first.page?.nextPageToken).toBe(first.collaborators[0]?.principal?.id);
    const rest = await api.repo.listCollaborators({ repo, page: { pageToken: first.page?.nextPageToken } });
    expect(rest.collaborators).toHaveLength(4);
    expect([...first.collaborators, ...rest.collaborators]).toEqual(expect.arrayContaining([
      expect.objectContaining({ principal: expect.objectContaining({ display: "Mara Okafor" }), role: "REPO_ROLE_ADMIN" }),
      expect.objectContaining({ principal: expect.objectContaining({ display: "triage" }), role: "REPO_ROLE_WRITE" }),
    ]));
    const teams = await api.team.listTeams({ org: "acme" });
    const { grants } = await api.team.listRepoTeams({ repo });
    expect(grants).toEqual(expect.arrayContaining([
      expect.objectContaining({ teamId: teams.teams.find((team) => team.slug === "platform")?.id, role: "maintain" }),
    ]));
    expect((await api.repo.listDeployKeys({ repo })).keys).toMatchObject([{ title: "CI checkout", readOnly: true }]);
  });

  it("applies repository visibility by version and returns permission and conflict reasons", async () => {
    const { api, store } = world();
    const before = await api.repo.getRepo({ repo });
    expect(before).toMatchObject({ version: "1", visibility: "VISIBILITY_PRIVATE" });
    const request = { repo, visibility: "VISIBILITY_PUBLIC" as const, expectedVersion: "1" };
    const minted = await api.auth.mintToken({
      principalId: store.p("triage").id,
      scope: {
        repos: ["acme/api"], readPaths: [], writePaths: [], refs: [], actions: ["read"], session: "",
        expiresAt: store.iso(store.now() + 3_600_000),
      },
    });
    const agent = createClient(fixtureRpc(store, minted.token));
    await expect(agent.repo.updateRepo(request)).rejects.toMatchObject({ reason: "POLICY_DENIED" });
    await expect(agent.repo.listDeployKeys({ repo })).rejects.toMatchObject({ reason: "POLICY_DENIED" });
    await expect(api.repo.updateRepo(request)).resolves.toMatchObject({ version: "2", visibility: "VISIBILITY_PUBLIC" });
    await expect(api.repo.updateRepo(request)).rejects.toMatchObject({ code: "aborted", reason: "VERSION_MISMATCH" });
    await expect(api.repo.getRepo({ repo })).resolves.toMatchObject({ version: "2", visibility: "VISIBILITY_PUBLIC" });
  });

  it("keeps Settings reads authenticated even on a public repository", async () => {
    const { store } = world();
    const anon = createClient(fixtureRpc(store));
    const repo = { fullName: "lattice/engine" };
    await expect(anon.repo.getRepo({ repo })).resolves.toMatchObject({ visibility: "VISIBILITY_PUBLIC" });
    await expect(anon.repo.listCollaborators({ repo })).rejects.toMatchObject({ reason: "UNAUTHENTICATED" });
    await expect(anon.agent.resolveAgentInstructions({ repo })).rejects.toMatchObject({ reason: "UNAUTHENTICATED" });
  });
});
