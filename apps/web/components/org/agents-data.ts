import type { BeeClient } from "@/lib/api/client";
import { mapLimit } from "@/lib/api/ops/shared";
import { BeeError } from "@/lib/api/transport";
import type { AgentSummary, Principal, TokenInfo } from "@/lib/api/types";
import { orgMembers } from "./people-data";

export function agentName(principal: Principal): string {
  return principal.display || principal.id;
}

export function humanOwner(agent: AgentSummary, byId: ReadonlyMap<string, AgentSummary>): Principal | undefined {
  let owner = agent.owner;
  const seen = new Set([agent.principal.id]);
  while (owner && !seen.has(owner.id)) {
    if (owner.kind === "PRINCIPAL_KIND_HUMAN") return owner;
    seen.add(owner.id);
    owner = byId.get(owner.id)?.owner;
  }
}

async function optional<T>(request: Promise<T>): Promise<{ result: T; error?: never } | { result?: never; error: string }> {
  try {
    return { result: await request };
  } catch (err) {
    if (!(err instanceof BeeError)) throw err;
    return { error: err.reason + " · " + err.message };
  }
}

export async function loadOrgAgents(api: BeeClient, viewer: Principal | null, org: string) {
  const leaseRequest = { repo: { fullName: org + "/*" } };
  const [{ agents }, members, leases, repos] = await Promise.all([
    api.agent.listAgents({ org }),
    orgMembers(api, org),
    optional(api.lease.listLeases(leaseRequest)),
    optional(api.repo.listRepos({ org, page: { pageSize: 200 } })),
  ]);
  const repoNames = new Map(repos.result?.repos.map((repo) => [repo.id, repo.fullName]));
  const role = members.find((member) => member.principal?.id === viewer?.id)?.role;
  const manageBudgets = viewer?.kind === "PRINCIPAL_KIND_HUMAN" && (role === "ROLE_OWNER" || role === "ROLE_ADMIN");
  const byId = new Map(agents.map((agent) => [agent.principal.id, agent]));
  const people = new Map<string, Principal>();

  const rows = await mapLimit(agents, 6, async (agent) => {
    const owner = humanOwner(agent, byId);
    const id = agent.principal.id;
    if (owner) people.set(owner.id, owner);
    // The wire checks ownership even for organization administrators.
    const tokenAccess = !!viewer && (viewer.id === id || viewer.id === agent.principal.ownerId);
    const tokens = tokenAccess
      ? await optional(api.agent.listTokens({ principalId: id }).then((response) => response.tokens))
      : { result: undefined, error: "Token details are visible to the agent and its owner." };
    return { agent, owner, leases: leases.result?.leases.filter((lease) => lease.holder?.id === id) ?? [], tokens };
  });
  const budgets = await mapLimit([...people.values()].filter((person) => manageBudgets || viewer?.id === person.id), 6, async (person) => ({
    person,
    ...(await optional(api.agent.getAgentBudget({ org, ownerId: person.id }))),
  }));
  return { rows, budgets, manageBudgets, repoNames, leaseError: leases.error };
}

export function tokenState(token: TokenInfo, now: number) {
  if (token.revoked) return { shape: "failed", label: "Revoked" } as const;
  if (token.scope.expiresAt && Date.parse(token.scope.expiresAt) <= now) return { shape: "closed", label: "Expired" } as const;
  return { shape: "passed", label: "Active" } as const;
}
