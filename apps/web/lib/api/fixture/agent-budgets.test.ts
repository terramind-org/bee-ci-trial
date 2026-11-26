import { expect, it } from "vitest";
import { createClient } from "../client";
import { fixtureRpc, FixtureStore } from ".";

it("saves a person's limits per organization and enforces the budget permissions", async () => {
  const store = new FixtureStore();
  const mara = createClient(fixtureRpc(store, "demo-token-1"));
  const budget = { org: "acme", ownerId: store.p("lin").id };
  const limits = { maxUsdMicros: "3141592", maxRuns: "3" };
  expect(await mara.agent.setAgentBudget({ ...budget, ...limits })).toMatchObject(limits);
  expect(await mara.agent.getAgentBudget(budget)).toMatchObject({ ...limits, runsStarted: "0" });

  const token = store.tokens.find((token) => token.principalId === store.p("docs").id)!;
  store.tokenSecrets.set("docs-demo-token", token.tokenId);
  const docs = createClient(fixtureRpc(store, "docs-demo-token"));
  await expect(docs.agent.getAgentBudget(budget)).rejects.toMatchObject({ code: "permission_denied" });
  await expect(docs.agent.setAgentBudget({ ...budget, ...limits })).rejects.toMatchObject({ code: "permission_denied" });
});
