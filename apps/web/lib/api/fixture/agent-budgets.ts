import type { AgentService } from "../../gen/bee/v1/agents_pb";
import { BeeError } from "../transport";
import type { AgentOwnerBudget } from "../types";
import type { Ctx, Impl } from "./router";
import type { FixtureStore } from "./store";

type Limits = Pick<AgentOwnerBudget, "maxUsdMicros" | "maxInputTokens" | "maxOutputTokens" | "maxRuns">;
const budgets = new WeakMap<FixtureStore, Map<string, Limits>>();

function scope(s: FixtureStore, ctx: Ctx, req: { org?: string; ownerId?: string }, write: boolean) {
  const actor = s.caller(ctx.token);
  const record = s.orgRecord(req.org);
  const org = record.org.name;
  const ownerId = req.ownerId || s.p(actor).id;
  const role = record.members.get(actor);
  if (!s.inOrg(actor, org) || ((write || ownerId !== s.p(actor).id) && role !== "ROLE_OWNER" && role !== "ROLE_ADMIN")) {
    throw new BeeError("permission_denied", { reason: "ACCESS_DENIED", message: "only an organization administrator can manage other people's agent budgets" });
  }
  let entries = budgets.get(s);
  if (!entries) {
    entries = new Map([[s.org.id + "/" + s.p("mara").id, {
      maxUsdMicros: "500000000",
      maxInputTokens: "20000000",
      maxOutputTokens: "5000000",
      maxRuns: "100",
    }]]);
    budgets.set(s, entries);
  }
  return { actor, org, orgId: record.org.id, ownerId, entries, key: record.org.id + "/" + ownerId };
}

function budget(org: string, ownerId: string, limits?: Limits): AgentOwnerBudget {
  // The fixture simulates repository work, which is separate from the hosted run ledger.
  return { org, ownerId, ...limits, spent: { usdMicros: "0", inputTokens: "0", outputTokens: "0" }, runsStarted: "0", exhausted: "" };
}

// The organization budget controls exercise these same owner-scoped wire methods in the demo.
export const agentBudgetImpl: Pick<Impl<typeof AgentService>, "getAgentBudget" | "setAgentBudget"> = {
  getAgentBudget: (s, req, ctx) => {
    const { orgId, ownerId, entries, key } = scope(s, ctx, req, false);
    return budget(orgId, ownerId, entries.get(key));
  },
  setAgentBudget: (s, req, ctx) => {
    const { actor, org, orgId, ownerId, entries, key } = scope(s, ctx, req, true);
    const { maxUsdMicros, maxInputTokens, maxOutputTokens, maxRuns } = req;
    const limits = { maxUsdMicros, maxInputTokens, maxOutputTokens, maxRuns };
    entries.set(key, limits);
    if (org === s.org.name) s.appendAudit(actor, "agent.budget_updated", s.principals.get(s.handleOf(ownerId))?.display || ownerId, "", true, "agent.budget_set");
    return budget(orgId, ownerId, limits);
  },
};
