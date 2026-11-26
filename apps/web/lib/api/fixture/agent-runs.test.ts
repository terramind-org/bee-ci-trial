import { describe, expect, it } from "vitest";
import { createClient } from "../client";
import { FixtureStore, fixtureRpc } from "./index";

const repo = { fullName: "acme/api" };
const anchor = Date.UTC(2026, 9, 18, 9, 0, 0);

function world() {
  const store = new FixtureStore({ anchor, clock: () => 0, live: false });
  const mara = createClient(fixtureRpc(store, "mara-token-0001"));
  const solo = store.swarms.runs.find((run) => run.trigger === "manual")!;
  const as = (handle: string) => {
    const token = "agent-run-fixture-" + handle;
    store.tokens.push({
      tokenId: token, principalId: store.p(handle).id, parentTokenId: "", revoked: false,
      scope: { repos: [], refs: [], readPaths: [], writePaths: [], actions: [], session: "" }, label: "Run test",
    });
    store.tokenSecrets.set(token, token);
    return createClient(fixtureRpc(store, token));
  };
  return { store, mara, solo, as };
}

describe("standalone run operations in the demo world", () => {
  it("gets a run by id and lists repository and agent runs using exclusive id cursors", async () => {
    const { mara, solo, store, as } = world();
    expect(await mara.agent.getAgentRun({ id: solo.id! })).toMatchObject({
      id: solo.id, state: "AGENT_RUN_STATE_RUNNING", task: solo.task, budget: solo.budget,
    });
    await expect(as("rhea").agent.getAgentRun({ id: solo.id! })).rejects.toMatchObject({ code: "not_found" });

    const request = { repo, agentPrincipalId: solo.agent!.id };
    const first = await mara.agent.listAgentRuns({ ...request, page: { pageSize: 1 } });
    expect(first.runs.map((run) => run.id)).toEqual([solo.id]);
    expect(first.page?.nextPageToken).toBe(solo.id);
    const next = await mara.agent.listAgentRuns({ ...request, page: { pageSize: 1, pageToken: first.page?.nextPageToken } });
    expect(next.runs.map((run) => run.id)).toEqual([store.swarms.swarms.find((swarm) => swarm.state === "SWARM_STATE_FINISHED")?.leadRunId]);
    expect(next.page?.nextPageToken).toBe("");
    const adopted = await mara.agent.listAgentRuns({ repo, taskKind: "AGENT_TASK_KIND_ADOPT_EVALUATE" });
    expect(adopted.runs.map((run) => run.id)).toEqual(store.posts.runs.map((run) => run.id));
    expect(adopted.runs.every((run) => run.summary && run.state === "AGENT_RUN_STATE_SUCCEEDED")).toBe(true);
  });

  it("persists steering, cancels terminally and retries into a new run", async () => {
    const { mara, solo, as } = world();
    const id = solo.id!;
    await expect(as("sam").agent.sendAgentRunMessage({ runId: id, body: "A message without permission" })).rejects.toMatchObject({ reason: "PERMISSION_DENIED" });
    await expect(mara.agent.retryAgentRun({ id })).rejects.toMatchObject({ reason: "INVALID_STATE" });
    const request = { runId: id, body: "Check the timing before changing it." };
    const sent = await mara.agent.sendAgentRunMessage(request);
    expect(sent).toMatchObject({ seq: "4", text: request.body, kind: "AGENT_RUN_EVENT_KIND_STEERING_MESSAGE", author: { id: solo.startedBy!.id } });

    const page = await mara.agent.listAgentRunEvents({ runId: id, page: { pageSize: 2 } });
    expect(page.events.map((event) => event.seq)).toEqual(["1", "2"]);
    expect(page.page?.nextPageToken).toBe("2");
    const next = await mara.agent.listAgentRunEvents({ runId: id, page: { pageToken: page.page?.nextPageToken } });
    expect(next.events).toMatchObject([{ seq: "3", kind: "AGENT_RUN_EVENT_KIND_LOG" }, sent]);
    expect(next.page?.nextPageToken).toBe("");
    expect((await mara.swarm.listRepoWork({ repo })).items[0]?.run?.id).toBe(id);

    const stopped = await mara.agent.cancelAgentRun({ id, reason: "Investigating a live incident." });
    expect(stopped).toMatchObject({ id, state: "AGENT_RUN_STATE_CANCELLED", failureReason: "CANCELLED", lastEventSeq: "5" });
    await expect(mara.agent.sendAgentRunMessage(request)).rejects.toMatchObject({ reason: "INVALID_STATE" });
    await expect(mara.agent.cancelAgentRun({ id })).rejects.toMatchObject({ reason: "INVALID_STATE" });

    const retry = await mara.agent.retryAgentRun({ id });
    expect(retry.id).not.toBe(id);
    expect(retry).toMatchObject({ state: "AGENT_RUN_STATE_QUEUED", task: solo.task, trigger: "retry", triggerRef: id });
    expect(await mara.agent.getAgentRun({ id: retry.id! })).toEqual(retry);
    expect(await mara.agent.getAgentRun({ id })).toEqual(stopped);
    expect((await mara.agent.listAgentRunEvents({ runId: retry.id! })).events).toMatchObject([{ seq: "1", state: "AGENT_RUN_STATE_QUEUED" }]);
  });

  it("reads swarm messages in the lead's event log and rejects direct steering of swarm workers", async () => {
    const { mara, store } = world();
    const working = store.swarms.swarms.find((swarm) => swarm.state === "SWARM_STATE_WORKING")!;
    const worker = working.memberRunIds.find((id) => id !== working.leadRunId)!;
    await expect(mara.agent.sendAgentRunMessage({ runId: worker, body: "Check the setup." })).rejects.toMatchObject({ reason: "PERMISSION_DENIED" });
    const sent = await mara.swarm.sendSwarmMessage({ swarm: { repo, id: working.id }, body: "Check the setup." });
    expect(sent.runId).toBe(working.leadRunId);
    const { events } = await mara.agent.listAgentRunEvents({ runId: working.leadRunId });
    expect(events).toEqual([sent]);
  });
});
