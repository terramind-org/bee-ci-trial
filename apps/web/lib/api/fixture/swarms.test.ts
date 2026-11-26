import { describe, expect, it } from "vitest";
import { toCli, toCurl, toMcp } from "../../copyas";
import { createClient } from "../client";
import { pathBytes, pathText } from "../ids";
import { wireOf } from "../ops";
import type { SwarmHandBack } from "../types";
import { FixtureStore, fixtureRpc } from "./index";
import { POLICY_PATH } from "./policy";

const repo = { fullName: "acme/api" };
const ANCHOR = Date.UTC(2026, 9, 18, 9, 0, 0);

function world() {
  const store = new FixtureStore({ anchor: ANCHOR, clock: () => 0, live: false });
  const mara = createClient(fixtureRpc(store, "mara-token-0001"));
  const as = (handle: string, session = "") => {
    const token = "swarm-token-" + handle + session;
    const tokenId = "swarm-test-" + handle + session;
    store.tokens.push({
      tokenId,
      parentTokenId: "",
      principalId: store.p(handle).id,
      scope: { repos: [], refs: [], readPaths: [], writePaths: [], actions: [], session },
      revoked: false,
      label: "Swarm test",
    });
    store.tokenSecrets.set(token, tokenId);
    return createClient(fixtureRpc(store, token));
  };
  const working = store.swarms.swarms.find((w) => w.state === "SWARM_STATE_WORKING")!;
  const swarm = { repo, id: working.id };
  return { store, mara, as, working, swarm };
}

const report: SwarmHandBack = {
  done: ["Ran the session tests."],
  deviations: ["Used the role service fixture."],
  concerns: ["Test the downgrade against the next deployment."],
  followUps: ["Link the test run to the pull request."],
};

describe("swarm operations against the demo repository", () => {
  it("returns two swarms, the live solo run and the existing adoption runs in activity order", async () => {
    const { store, mara } = world();
    const first = await mara.swarm.listRepoWork({ repo, page: { pageSize: 2 } });
    const second = await mara.swarm.listRepoWork({ repo, page: { pageSize: 2, pageToken: first.page?.nextPageToken } });
    const last = await mara.swarm.listRepoWork({ repo, page: { pageSize: 2, pageToken: second.page?.nextPageToken } });
    expect(first.page?.nextPageToken).toBeTruthy();
    expect(second.page?.nextPageToken).toBeTruthy();
    expect(last.page?.nextPageToken).toBe("");
    const items = [...first.items, ...second.items, ...last.items];
    expect(items.map((i) => i.activityAt)).toEqual(items.map((i) => i.activityAt).sort().reverse());

    const kinds = items.map((i) => i.swarm ? "swarm" : "run");
    expect(kinds).toEqual(["swarm", "run", "run", "run", "swarm"]);
    const solo = store.swarms.runs.find((run) => run.trigger === "manual")!;
    expect(items[1]?.run).toMatchObject({ id: solo.id, state: "AGENT_RUN_STATE_RUNNING" });
    expect(items.flatMap((i) => i.run?.id ?? []).sort()).toEqual([solo.id, ...store.posts.runs.map((run) => run.id)].sort());

    const working = items[0]!.swarm!;
    expect(working.lead?.id).toBe(store.p("triage").id);
    expect(working.owner?.id).toBe(store.p("mara").id);
    expect(working.budget?.maxUsdMicros).toBe("9000000");
    expect(working.cost?.usdMicros).toBe(store.current(store.crByKey("rotate")).head.provenance?.cost?.usdMicros);
    expect(working.tasks).toHaveLength(6);
    expect(new Set(working.tasks.map((t) => t.state))).toEqual(new Set(["SWARM_TASK_STATE_OPEN", "SWARM_TASK_STATE_WORKING", "SWARM_TASK_STATE_FINISHED", "SWARM_TASK_STATE_EMPTY"]));
    expect(working.tasks.map((t) => t.attempts.at(-1)?.outcome)).toEqual(expect.arrayContaining(["SWARM_ATTEMPT_OUTCOME_EMPTY", "SWARM_ATTEMPT_OUTCOME_REFUSED"]));
    expect(working.tasks[0]?.attempts[0]?.handBack?.followUps).toHaveLength(1);
    expect(working.tasks.some((t) => t.paths.map(pathText).includes("src/auth/session.ts"))).toBe(true);
    expect(working.joins).toMatchObject([{ state: "SWARM_JOIN_STATE_PENDING", requester: { display: "Dev Patel" }, agent: { display: "billing-fixer" } }]);

    const finished = items.at(-1)!;
    expect(finished.swarm?.state).toBe("SWARM_STATE_FINISHED");
    expect(finished.changeRequestIds).toEqual([store.crByKey("histogram").id]);
    expect(finished.swarm?.changeRequestIds).toEqual(finished.changeRequestIds);
  });

  it("keeps join details and budgets within their authorized audience", async () => {
    const { mara, as, swarm } = world();
    const own = await mara.swarm.getSwarm(swarm);
    const requester = await as("dev").swarm.getSwarm(swarm);
    const other = await as("sam").swarm.getSwarm(swarm);
    expect(own.joins).toHaveLength(1);
    expect(requester.joins).toEqual(own.joins);
    expect(other.joins).toEqual([]);
    expect(other.budget).toBeUndefined();
    expect(other.cost).toBeUndefined();
    await expect(as("rhea").swarm.getSwarm(swarm)).rejects.toMatchObject({ code: "not_found" });
  });

  it("creates a planning swarm from a live solo run and removes that run from the solo rows", async () => {
    const { store, mara, working } = world();
    const lead = store.swarms.runs.find((r) => r.id === working.leadRunId)!;
    // Supply another live run without changing the public demo's standalone runs.
    const solo = { ...lead, id: "01ARZ3NDEKTSV4RRFFQ69G5FAV" };
    store.swarms.runs.push(solo);
    const made = await mara.swarm.createSwarm({ repo, name: "Audit role changes", goal: "Record who changed a role.", leadRunId: solo.id, branch: "main", issue: "14" });
    expect(made).toMatchObject({ state: "SWARM_STATE_PLANNING", issue: "14", owner: { id: store.p("mara").id }, memberRunIds: [solo.id] });
    expect((await mara.swarm.listRepoWork({ repo })).items.some((i) => i.run?.id === solo.id)).toBe(false);
    expect(store.events.at(-1)).toMatchObject({ kind: "swarm.created", subject: { id: made.id } });
  });

  it("closes each task attempt with its hand-back, allows a retry and emits the changes", async () => {
    const { store, mara, as, working, swarm } = world();
    const lead = store.swarms.runs.find((r) => r.id === working.leadRunId)!;
    const leadClient = as("triage", lead.session);
    await expect(mara.swarm.setSwarmStatus({ swarm, status: "Waiting for review." })).rejects.toMatchObject({ reason: "PERMISSION_DENIED" });
    expect((await leadClient.swarm.setSwarmStatus({ swarm, status: "Waiting for review." })).status).toBe("Waiting for review.");

    const task = await mara.swarm.addSwarmTask({ swarm, title: "Check role switch tests", paths: [pathBytes("src/auth/session.ts")] });
    const request = { swarm, taskId: task.id, runId: working.leadRunId };
    expect((await mara.swarm.startSwarmTask(request)).state).toBe("SWARM_TASK_STATE_WORKING");
    const empty = await leadClient.swarm.finishSwarmTask({ ...request, attemptNumber: 1, outcome: "SWARM_ATTEMPT_OUTCOME_EMPTY", reason: "The first run found no change.", handBack: report });
    expect(empty.state).toBe("SWARM_TASK_STATE_EMPTY");
    expect(empty.attempts[0]?.handBack).toEqual(report);
    await mara.swarm.startSwarmTask(request);
    await expect(leadClient.swarm.finishSwarmTask({ ...request, attemptNumber: 1, outcome: "SWARM_ATTEMPT_OUTCOME_EMPTY", reason: "This attempt already ended.", handBack: report })).rejects.toMatchObject({ reason: "VERSION_MISMATCH", detail: { subcode: "swarm_attempt_changed" } });
    const done = await leadClient.swarm.finishSwarmTask({ ...request, attemptNumber: 2, outcome: "SWARM_ATTEMPT_OUTCOME_FINISHED", reason: "The tests passed.", handBack: report });
    expect(done.state).toBe("SWARM_TASK_STATE_FINISHED");
    expect(done.attempts.map((a) => a.reason)).toEqual(["The first run found no change.", "The tests passed."]);
    const events = await mara.event.listEvents({ repo, kinds: ["swarm.*"] });
    expect(events.events.map((e) => e.kind)).toEqual(["swarm.status_changed", "swarm.task_added", "swarm.task_started", "swarm.task_finished", "swarm.task_started", "swarm.task_finished"]);
    expect(events.events.every((e) => e.subject?.id === swarm.id)).toBe(true);
  });

  it("steers the lead only and makes an owner or admin stop the group", async () => {
    const { store, mara, as, working, swarm } = world();
    const sent = await as("sam").swarm.sendSwarmMessage({ swarm, body: "Please keep other sessions signed in." });
    expect(sent).toMatchObject({ runId: working.leadRunId, text: "Please keep other sessions signed in.", author: { id: store.p("sam").id } });
    expect([...store.swarms.messages.keys()]).toEqual([working.leadRunId]);
    await expect(as("reviewer").swarm.sendSwarmMessage({ swarm, body: "A worker sent this." })).rejects.toMatchObject({ reason: "PERMISSION_DENIED" });

    await expect(as("sam").swarm.setSwarmState({ swarm, state: "SWARM_STATE_PAUSED" })).rejects.toMatchObject({ code: "permission_denied" });
    expect((await mara.swarm.setSwarmState({ swarm, state: "SWARM_STATE_PAUSED" })).state).toBe("SWARM_STATE_PAUSED");
    const open = working.tasks.find((t) => t.state === "SWARM_TASK_STATE_OPEN")!;
    await expect(mara.swarm.startSwarmTask({ swarm, taskId: open.id, runId: working.leadRunId })).rejects.toMatchObject({ reason: "INVALID_STATE" });
    expect((await mara.swarm.setSwarmState({ swarm, state: "SWARM_STATE_WORKING" })).state).toBe("SWARM_STATE_WORKING");
    const stopped = await as("dev").swarm.setSwarmState({ swarm, state: "SWARM_STATE_STOPPED" });
    expect(stopped.state).toBe("SWARM_STATE_STOPPED");
    expect(stopped.tasks.some((t) => t.state === "SWARM_TASK_STATE_WORKING")).toBe(false);
    expect((await mara.swarm.getSwarm(swarm)).joins[0]).toMatchObject({ state: "SWARM_JOIN_STATE_DECLINED", reason: "Swarm ended" });
    expect(store.swarms.runs.filter((r) => working.memberRunIds.includes(r.id!) && r.state === "AGENT_RUN_STATE_CANCELLED")).toHaveLength(2);
    await expect(mara.swarm.setSwarmState({ swarm, state: "SWARM_STATE_WORKING" })).rejects.toMatchObject({ reason: "INVALID_STATE", detail: { subcode: "swarm_state" } });
  });

  it("admits, declines and refuses joins with their durable decisions", async () => {
    const { store, mara, as, working, swarm } = world();
    const pending = working.joins[0]!;
    const joined = await mara.swarm.decideSwarmJoin({ swarm, joinId: pending.id, accept: true });
    expect(joined.state).toBe("SWARM_JOIN_STATE_ACCEPTED");
    expect((await mara.swarm.getSwarm(swarm)).memberRunIds).toContain(joined.runId);
    expect(store.swarms.runs.find((run) => run.id === joined.runId)).toMatchObject({ agent: { id: store.p("billing").id }, budget: { maxUsdMicros: "1000000" }, baseRef: working.branch });

    const offer = { swarm, agentPrincipalId: store.p("deps").id, budget: { maxUsdMicros: "500000" } };
    const made = await as("dev").swarm.requestSwarmJoin(offer);
    await expect(as("dev").swarm.requestSwarmJoin(offer)).rejects.toMatchObject({ reason: "ALREADY_EXISTS" });
    const declined = await mara.swarm.decideSwarmJoin({ swarm, joinId: made.id, accept: false, reason: "We already have review coverage." });
    expect(declined).toMatchObject({ state: "SWARM_JOIN_STATE_DECLINED", runId: "", reason: "We already have review coverage." });

    store.repo(repo).repo.visibility = "VISIBILITY_PUBLIC";
    const outsider = await as("rhea").swarm.requestSwarmJoin({ ...offer, agentPrincipalId: store.p("patcher").id });
    expect(outsider.state).toBe("SWARM_JOIN_STATE_PENDING");
    await expect(mara.swarm.decideSwarmJoin({ swarm, joinId: outsider.id, accept: true })).rejects.toMatchObject({ code: "not_found" });
    expect((await as("rhea").swarm.getSwarm(swarm)).joins).toMatchObject([{ id: outsider.id, state: "SWARM_JOIN_STATE_PENDING" }]);

    const withheld = await as("lin").swarm.requestSwarmJoin({ ...offer, agentPrincipalId: store.p("docs").id });
    const tip = store.tip(store.repo(repo));
    tip.files.set(POLICY_PATH, (tip.files.get(POLICY_PATH) ?? "") + `
[roles.guest]
actions = ["read"]

[roles.steward]
actions = ["read", "commit", "swarm_join"]

[members]
guest = ["@everyone"]
steward = ["mara", "lin"]
`);
    const refused = await mara.swarm.decideSwarmJoin({ swarm, joinId: withheld.id, accept: true });
    expect(refused).toMatchObject({ state: "SWARM_JOIN_STATE_REFUSED", runId: "", reason: expect.stringContaining("Repository rules") });
  });

  it("copies the operation as the generated Bee command and MCP tool", () => {
    const request = { swarm: { repo, id: "01S" }, body: "Hold for review" };
    const operation = { service: "SwarmService", method: "SendSwarmMessage", request } as const;
    expect(wireOf(operation.service, operation.method, request)).toMatchObject({ ...operation, streaming: false });
    expect(toCli(operation)).toContain("bee swarm send-swarm-message --swarm");
    expect(toCurl(operation)).toContain("/bee.v1.SwarmService/SendSwarmMessage");
    expect(JSON.parse(toMcp(operation)).params).toEqual({ name: "swarm_send_swarm_message", arguments: request });
  });
});
