import { describe, expect, it } from "vitest";
import { createClient } from "../client";
import { FixtureStore, fixtureRpc } from ".";

const repo = { fullName: "acme/api" };
const api = (store = new FixtureStore()) => createClient(fixtureRpc(store, "mara-token-0001"));

describe("CI management operations", () => {
  it("persists workflow state and returns the workflow badge", async () => {
    const store = new FixtureStore();
    const { workflow } = api(store);
    const list = await workflow.listWorkflows({ repo });
    const ci = list.workflows.find((w) => w.name === "ci");
    expect(ci).toMatchObject({ path: ".github/workflows/ci.yml", state: "WORKFLOW_STATE_ACTIVE" });
    const request = { repo, workflow: ci!.id };
    await workflow.disableWorkflow(request);
    expect((await workflow.listWorkflows({ repo })).workflows).toContainEqual({ ...ci, state: "WORKFLOW_STATE_DISABLED_MANUALLY" });
    const total = store.runs.length;
    const tax = store.crByKey("tax");
    store.revise(tax, {}, "mara");
    expect(store.runs).toHaveLength(total);
    expect(await workflow.enableWorkflow(request)).toEqual(ci);
    store.revise(tax, {}, "mara");
    expect(store.runs).toHaveLength(total + 1);
    const badge = await workflow.getWorkflowBadge(request);
    expect(badge.svg).toContain("<svg");
    expect(badge.svg).toContain(badge.message);
  });

  it("distinguishes approval from runner queueing and cancels a waiting run", async () => {
    const { workflow } = api();
    const { runs } = await workflow.listRuns({ repo, page: { pageSize: 100 } });
    const waiting = runs.find((r) => r.status === "waiting");
    expect(waiting).toMatchObject({ trigger: "workflow_dispatch", jobs: [] });
    const request = { repo, id: waiting!.id };
    await workflow.cancelRun(request);
    expect(await workflow.getRun(request)).toMatchObject({ status: "cancelled" });
    await expect(workflow.cancelRun(request)).rejects.toMatchObject({ reason: "INVALID_STATE" });
  });

  it("returns deployment provenance and actual state", async () => {
    const { deployment } = api();
    expect((await deployment.listDeployments({ repo })).deployments).toEqual([
      expect.objectContaining({ environment: "staging", ref: "main", commit: expect.any(Object), state: "DEPLOYMENT_STATE_SUCCESS" }),
    ]);
  });
});
