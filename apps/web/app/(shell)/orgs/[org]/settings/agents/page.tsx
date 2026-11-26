import type { Metadata } from "next";
import { requestMode } from "@/app/request-mode";
import { PageFacts, type FactGroup } from "@/components/bee/page-facts";
import { EmptyState } from "@/components/kit";
import { PageMenu } from "@/components/menu";
import { OrgRevokeToken, OwnerBudgetControl } from "@/components/org/agents-actions";
import { dollars, ownerBudgetFacts } from "@/components/org/agents-budget";
import { agentName, loadOrgAgents, tokenState } from "@/components/org/agents-data";
import { Mark, State } from "@/components/parts";
import { Time } from "@/components/time";
import { PageHeader } from "@/components/ui";
import { pathText } from "@/lib/api/ids";
import { currentPrincipal, serverClient } from "@/lib/api/server";
import type { Budget, TokenInfo } from "@/lib/api/types";
import { absoluteTime } from "@/lib/format";
import { guard } from "@/lib/page";
import { toolFor } from "@/lib/tools";
import { SettingsTabs } from "../tabs";

export const metadata: Metadata = { title: "Agents and budgets" };

const NO_OWNER = "No person answers for this agent";
const NO_AGENTS = "No agents in this organization";
const NO_BUDGETS = "Shared budgets are visible to their owners and organization administrators.";

const QUOTAS: [keyof Budget, string][] = [
  ["maxChangesets", "commits"],
  ["maxLanded", "merged commits"],
  ["maxUploadBytes", "uploaded bytes"],
];

function quotas(token: TokenInfo): string[] {
  return QUOTAS.flatMap(([field, unit]) => {
    const used = token.used?.[field];
    const limit = token.scope.budget?.[field];
    if (used == null && (limit == null || limit === "0")) return [];
    const number = (value: string) => BigInt(value).toLocaleString("en-US");
    return [[unit, used != null ? number(used) + " used" : "", limit && limit !== "0" ? number(limit) + " limit" : ""].filter(Boolean).join(" ")];
  });
}

const expiry = (iso: string, expired = false) => (expired ? "Expired " : "Expires ") + absoluteTime(iso);

function tokenScope({ scope }: TokenInfo) {
  return [
    scope.repos.length ? "Repositories " + scope.repos.join(", ") : "",
    scope.refs.length ? "Refs " + scope.refs.join(", ") : "",
    scope.readPaths.length ? "Read paths " + scope.readPaths.map(pathText).join(", ") : "",
    scope.writePaths.length ? "Write paths " + scope.writePaths.map(pathText).join(", ") : "",
    scope.actions.length ? "Actions " + scope.actions.map((action) => action === "land" ? "merge" : action).join(", ") : "",
  ].filter(Boolean);
}

export default async function OrgAgentsPage({ params }: { params: Promise<{ org: string }> }) {
  return guard(async () => {
    const org = decodeURIComponent((await params).org);
    const [api, viewer, { mode }] = await Promise.all([serverClient(), currentPrincipal(), requestMode()]);
    const { rows, budgets, manageBudgets, repoNames, leaseError } = await loadOrgAgents(api, viewer, org);
    const now = Date.now();
    const agents = rows.map(({ agent, owner, leases, tokens }) => ({
      id: agent.principal.id,
      name: agentName(agent.principal),
      owner: owner && agentName(owner),
      runtime: agent.principal.runtime,
      model: agent.model,
      spend: agent.registered ? "$" + dollars(agent.spend7dUsdMicros) + " recorded in the last 7 days" : "",
      tokens: tokens.result?.map((token) => ({
        token, name: token.label || token.tokenId, state: tokenState(token, now), quotas: quotas(token), scope: tokenScope(token),
      })),
      tokenError: tokens.error,
      leases: leases.map((lease) => ({
        id: lease.id,
        repo: repoNames.get(lease.repoId) ?? lease.repoId,
        ref: lease.ref,
        paths: lease.paths?.map(pathText).join(", "),
        expires: lease.expiresAt,
      })),
    }));
    const sharedBudgets = budgets.map(({ person, result, error }) => ({
      person, result, error, name: agentName(person), facts: result ? ownerBudgetFacts(result) : [],
    }));

    if (mode === "bee") {
      const groups: FactGroup[] = agents.map((agent) => ({
        title: agent.name,
        facts: [
          { label: "answers for the agent", value: agent.owner ?? NO_OWNER },
          agent.runtime ? { label: "runtime", value: agent.runtime } : null,
          agent.model ? { label: "model", value: agent.model } : null,
          agent.spend ? { label: "spend", value: agent.spend } : null,
        ],
        lines: [
          ...(agent.tokenError ? [agent.tokenError] : agent.tokens?.length ? agent.tokens.map(({ name, state, token, quotas, scope }) =>
            ["Token " + name, state.label, token.scope.expiresAt && expiry(token.scope.expiresAt, state.label === "Expired"), ...scope, ...quotas].filter(Boolean).join(" · ")) : ["No tokens issued"]),
          ...(leaseError ? [] : agent.leases.length ? agent.leases.map((lease) =>
            ["Lease in " + lease.repo, lease.ref, lease.paths, lease.expires && expiry(lease.expires)].filter(Boolean).join(" · ")) : ["No leases held"]),
        ],
      }));
      return (
        <PageFacts page="org-agents" title={org + " · agents and budgets"} groups={[
            ...(leaseError ? [{ title: "leases", lines: ["Could not load leases · " + leaseError] }] : []),
            ...(groups.length ? groups : [{ title: "agents", empty: NO_AGENTS }]),
            ...(sharedBudgets.length ? sharedBudgets.map((budget) => ({
              title: "Monthly budget for " + budget.name,
              facts: budget.facts.map((fact) => ({ label: fact.label.toLowerCase(), value: fact.value + (fact.exhausted ? " · Limit reached" : "") })),
              lines: budget.error ? ["Could not load budget · " + budget.error] : [],
            })) : agents.length ? [{ title: "shared monthly budgets", empty: NO_BUDGETS }] : []),
          ]} tools={[
            toolFor("AgentService/ListAgents", "List agents, runtimes, models and recorded usage"),
            toolFor("AgentService/ListTokens", "List your tokens or those of an agent you own"),
            toolFor("LeaseService/ListLeases", "Find leases held within the organization", { repo: { fullName: org + "/*" } }),
            toolFor("AgentService/GetAgentBudget", "Read a person’s shared monthly budget"),
            toolFor("AuthService/Revoke", "Revoke a token and those derived from it"),
            ...(manageBudgets ? [toolFor("AgentService/SetAgentBudget", "Set monthly limits for a person’s agents")] : []),
        ]} />
      );
    }

    return (
      <div className="max-w-5xl">
        <PageHeader title="Agents and budgets" eyebrow={org} actions={<PageMenu op={{ service: "AgentService", method: "ListAgents", request: { org }, label: "List agents" }} />} />
        <SettingsTabs org={org} active="agents" />
        {agents.length ? (
          <section aria-label="Agents">
            {leaseError ? <p className="py-3 text-sm text-ink-2" role="status">Could not load leases · {leaseError}</p> : null}
            <ul className="divide-y divide-line border-b border-line">
              {agents.map((agent) => (
                <li key={agent.id} data-testid="org-agent" className="grid min-w-0 gap-5 py-6 md:grid-cols-[minmax(0,1fr)_minmax(0,1.7fr)] md:gap-8">
                  <div className="min-w-0 [overflow-wrap:anywhere]">
                    <h2 className="text-xl font-light text-ink">{agent.name}</h2>
                    <p className="mt-2 text-sm">
                      {agent.owner ? <Mark>{agent.owner} answers for this agent</Mark> : <State shape="waiting">{NO_OWNER}</State>}
                    </p>
                    {agent.runtime || agent.model ? <p className="mt-1 text-sm text-ink-2">{[agent.runtime, agent.model].filter(Boolean).join(" · ")}</p> : null}
                    {agent.spend ? <p className="mt-1 text-sm text-ink-2">{agent.spend}</p> : null}
                  </div>
                  <div className="min-w-0 space-y-4 [overflow-wrap:anywhere]">
                    <section aria-label={"Tokens for " + agent.name}>
                      <h3 className="mb-1 text-xs font-medium text-ink-2">Tokens</h3>
                      {agent.tokenError ? <p className="text-sm text-ink-2">{agent.tokenError}</p> : agent.tokens?.length ? (
                        <ul className="divide-y divide-line">
                          {agent.tokens.map(({ token, name, state, quotas, scope }) => (
                            <li key={token.tokenId} className="flex flex-wrap items-start justify-between gap-2 py-2">
                              <div className="min-w-0 flex-1">
                                <p className="text-base">{name}</p>
                                <p className="flex flex-wrap gap-x-2 text-sm text-ink-2">
                                  <State shape={state.shape}>{state.label}</State>
                                  {token.scope.expiresAt ? <span>{expiry(token.scope.expiresAt, state.label === "Expired")}</span> : null}
                                </p>
                                {scope.length ? <p className="text-sm text-ink-2">{scope.join(" · ")}</p> : null}
                                {quotas.length ? <p className="num text-sm text-ink-2">{quotas.join(" · ")}</p> : null}
                              </div>
                              {state.label === "Active" ? <OrgRevokeToken tokenId={token.tokenId} name={name} /> : null}
                            </li>
                          ))}
                        </ul>
                      ) : <p className="text-sm text-ink-2">No tokens issued.</p>}
                    </section>
                    {!leaseError ? (
                      <section aria-label={"Leases for " + agent.name}>
                        <h3 className="mb-1 text-xs font-medium text-ink-2">Leases</h3>
                        {agent.leases.length ? (
                          <ul className="space-y-1.5">
                            {agent.leases.map((lease) => (
                              <li key={lease.id} className="text-sm">
                                <p className="mono">{[lease.repo, lease.ref, lease.paths].filter(Boolean).join(" · ")}</p>
                                {lease.expires ? <p className="text-ink-2">Expires <Time iso={lease.expires} /></p> : null}
                              </li>
                            ))}
                          </ul>
                        ) : <p className="text-sm text-ink-2">No leases held.</p>}
                      </section>
                    ) : null}
                  </div>
                </li>
              ))}
            </ul>
          </section>
        ) : <EmptyState title={NO_AGENTS}>Agents appear here when they belong to {org}.</EmptyState>}
        {agents.length ? (
          <section aria-labelledby="shared-budgets" className="mt-10">
            <h2 id="shared-budgets" className="text-xl font-light">Shared monthly budgets</h2>
            <p className="mb-4 mt-1 text-sm text-ink-2">Each budget covers the agents that draw on one person’s limits. Usage resets at the start of each UTC month.</p>
            {sharedBudgets.length ? (
              <ul className="divide-y divide-line border-y border-line">
                {sharedBudgets.map(({ person, name, result, facts, error }) => (
                  <li key={person.id} data-testid="org-agent-budget" className="min-w-0 py-5 [overflow-wrap:anywhere]">
                    <h3 className="mb-2 text-base font-medium">{name}</h3>
                    {error ? <p className="text-sm text-ink-2">Could not load budget · {error}</p> : (
                      <dl className="grid grid-cols-[minmax(0,1fr)_minmax(0,2fr)] gap-x-3 gap-y-1 text-sm sm:max-w-lg">
                        {facts.map(({ label, value, exhausted }) => (
                          <div key={label} className="contents">
                            <dt className="text-ink-2">{label}</dt>
                            <dd className="num">{value} {exhausted ? <State shape="waiting">Limit reached</State> : null}</dd>
                          </div>
                        ))}
                      </dl>
                    )}
                    {manageBudgets && result ? <div className="mt-3"><OwnerBudgetControl org={org} ownerId={person.id} ownerName={name} budget={result} /></div> : null}
                  </li>
                ))}
              </ul>
            ) : <p className="text-sm text-ink-2">{NO_BUDGETS}</p>}
          </section>
        ) : null}
      </div>
    );
  });
}
