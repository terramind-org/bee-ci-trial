import { renderToStaticMarkup } from "react-dom/server";
import { prerender } from "react-dom/static";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import AgentsPage from "@/app/(repo)/[owner]/[repo]/agents/page";
import AgentPage from "@/app/(repo)/[owner]/[repo]/agents/[id]/page";
import StandaloneRunPage from "@/app/(repo)/[owner]/[repo]/agents/runs/[runId]/page";
import { nextSnapshot } from "@/components/bees/event-keys";
import { activityHref } from "@/components/repo/presence";
import { createClient } from "@/lib/api/client";
import { FixtureStore, fixtureRpc } from "@/lib/api/fixture";
import { pathBytes } from "@/lib/api/ids";
import type { AgentRun, Lease } from "@/lib/api/types";
import { runCells, taskMotionKey, workAnchor, workMotionSnapshot } from "@/components/agents/work";

const request = vi.hoisted(() => ({ mode: "keeper", token: "mara-token-0001" }));

vi.mock("next/headers", () => ({
  cookies: async () => ({ get: (name: string) => name === "bee_mode" ? { value: request.mode } : name === "bee_token" ? { value: request.token } : undefined }),
  headers: async () => new Headers(),
}));

vi.mock("next/navigation", () => ({
  useRouter: () => ({ refresh: vi.fn(), push: vi.fn() }),
  notFound: () => { throw new Error("Repository not found"); },
}));

const repo = { fullName: "acme/api" };
const params = { owner: "acme", repo: "api" };

let fixture: FixtureStore;
let previous: FixtureStore | undefined;
beforeEach(() => {
  request.mode = "keeper";
  request.token = "mara-token-0001";
  vi.stubEnv("BEE_ADAPTER", "fixture");
  previous = globalThis.__beeFixture;
  fixture = new FixtureStore({ anchor: Date.now(), clock: () => 0, live: false });
  globalThis.__beeFixture = fixture;
});
afterEach(() => {
  globalThis.__beeFixture = previous;
  vi.unstubAllEnvs();
});

const api = () => createClient(fixtureRpc(fixture, request.token));
const list = async () => renderToStaticMarkup(await AgentsPage({ params: Promise.resolve(params) }));
const detail = async (runId: string) => renderToStaticMarkup(await StandaloneRunPage({ params: Promise.resolve({ ...params, runId }), searchParams: Promise.resolve({}) }));
const principal = async (id: string) => {
  const { prelude } = await prerender(await AgentPage({ params: Promise.resolve({ ...params, id }), searchParams: Promise.resolve({}) }));
  return new Response(prelude as unknown as BodyInit).text();
};
const count = (markup: string, pattern: RegExp) => markup.match(pattern)?.length ?? 0;

describe("Agents on the fixture repository", () => {
  it("mixes swarms and standalone runs in activity order, with accessible task states", async () => {
    const { items } = await api().swarm.listRepoWork({ repo });
    const solo = items.find((item) => item.run?.trigger === "manual")!;
    const swarm = items.find((item) => item.swarm?.state === "SWARM_STATE_WORKING")!;
    const html = await list();
    const swarmRow = html.indexOf('id="' + workAnchor(swarm) + '"');
    const soloRow = html.indexOf('id="' + workAnchor(solo) + '"');
    expect(count(html, /<h1\b/g)).toBe(1);
    expect(count(html, /data-nav-item=/g)).toBe(items.length);
    expect(swarmRow).toBeGreaterThan(0);
    expect(soloRow).toBeGreaterThan(swarmRow);
    expect(html).toContain("Queen · lead triage");
    expect(html).toContain("Swarm · 3 agents");
    expect(html).toContain("One agent");
    expect(html).toContain("Mara Okafor");
    const histogram = fixture.crByKey("histogram");
    expect(html).toContain('href="/acme/api/changes/' + histogram.id + '">Pull request #' + histogram.number + '</a>');
    expect(html).toMatch(/data-state="merged"[^>]*>.*?<span>Merged<\/span>/s);
    expect(html).toMatch(/<details[^>]*><summary[^>]*><h2[^>]*>Finished this week · 3<\/h2>/);
    for (const label of [
      "Verify account switching · Open",
      "Review the rotation regression tests · Being worked on",
      "Map the role downgrade path · Finished",
      "Check the API guide for stale claims · Came back empty",
    ]) {
      expect(html).toContain('role="img" aria-label="' + label + '" title="' + label + '"');
    }

    await api().swarm.setSwarmState({ swarm: { repo, id: swarm.swarm!.id }, state: "SWARM_STATE_PAUSED" });
    const paused = await list();
    expect(paused.indexOf('id="' + workAnchor(swarm) + '"')).toBeGreaterThan(paused.indexOf(">Paused</h2>"));
    request.mode = "bee";
    const agent = await list();
    expect(count(agent, /<h1\b/g)).toBe(1);
    expect(agent).toContain('data-bee-page="agents"');
    expect(agent).toContain('data-bee-group="Paused"');
    expect(agent).toContain('data-bee-group="Finished this week · 3"');
    expect(agent).toContain('data-tool="swarm_list_repo_work"');
    expect(agent).toContain("Check the API guide for stale claims · Came back empty");
    expect(agent).toContain('href="/acme/api/agents/runs/' + solo.run!.id + '"');
  });

  it("renders session logs and steering on active runs and reports or reasons after they end", async () => {
    const solo = (await api().swarm.listRepoWork({ repo })).items.find((item) => item.run?.trigger === "manual")!.run!;
    const runId = solo.id!;
    await api().agent.sendAgentRunMessage({ runId, body: "Check cold starts separately." });
    const active = await detail(runId);
    expect(count(active, /<h1\b/g)).toBe(1);
    expect(active).toContain(solo.task!.prompt);
    expect(active).toContain("Read the startup sequence");
    expect(active).not.toContain("State unavailable");
    expect(active).toContain("Check cold starts separately.");
    expect(active).toContain("$0.02 spent · $1.50 budget");
    expect(active).toContain('data-agent-confirm="Stops this run permanently."');
    expect(active).not.toContain(">Pause<");
    expect(active).not.toContain("Attempts");
    expect(active).not.toContain("What it reported");

    request.mode = "bee";
    const agentActive = await detail(runId);
    expect(count(agentActive, /<h1\b/g)).toBe(1);
    expect(agentActive).toContain('data-tool="agent_cancel_agent_run" data-kind="needs a person"');
    expect(agentActive).toContain("Check cold starts separately.");
    expect(agentActive).not.toContain('data-tool="agent_retry_agent_run"');

    await api().agent.cancelAgentRun({ id: runId });
    const stopped = await detail(runId);
    expect(stopped).toContain('data-tool="agent_retry_agent_run"');
    expect(stopped).not.toContain('data-tool="agent_send_agent_run_message"');

    request.mode = "keeper";
    const done = (await api().swarm.listRepoWork({ repo })).items.find((item) => item.run?.summary)!.run!;
    const report = await detail(done.id!);
    expect(report).toContain(">What it reported</h2>");
    expect(report).toContain(">Retry<");

    const failed = fixture.swarms.runs.find((run) => run.id === runId)!;
    failed.state = "AGENT_RUN_STATE_FAILED";
    failed.failureReason = "no_runner_configured";
    const failure = await detail(runId);
    expect(failure).toContain(">Failure reason</dt>");
    expect(failure).toContain("no_runner_configured");
    expect(failure).toContain("Came back empty");
  });

  it("tracks confirmed task, pause and join changes from the repository work feed", async () => {
    const client = api();
    const work = async () => (await client.swarm.listRepoWork({ repo })).items;
    const initial = await work();
    const item = initial.find(({ swarm }) => swarm?.state === "SWARM_STATE_WORKING")!;
    const swarm = { repo, id: item.swarm!.id };
    const row = workAnchor(item);
    const task = item.swarm!.tasks.find(({ state }) => state === "SWARM_TASK_STATE_OPEN")!;
    const taskKey = taskMotionKey(row, task.id);
    const join = item.swarm!.joins[0]!;
    const requester = join.requester!.id;
    const baseline = nextSnapshot(undefined, "fixture agents", workMotionSnapshot(initial, requester), "fixture", true);
    const capture = async (previous: typeof baseline, viewer = requester) =>
      nextSnapshot(previous, "fixture agents", workMotionSnapshot(await work(), viewer), "fixture", true);

    expect(baseline.keys).toEqual({});
    await client.swarm.startSwarmTask({ swarm, taskId: task.id, runId: item.swarm!.leadRunId });
    const started = await capture(baseline);
    expect(started.values[taskKey]).toBe("working/1");
    expect(Object.keys(started.keys)).toEqual([taskKey]);

    await client.swarm.finishSwarmTask({
      swarm, taskId: task.id, runId: item.swarm!.leadRunId, attemptNumber: 1,
      outcome: "SWARM_ATTEMPT_OUTCOME_FINISHED", reason: "The task is complete.",
      handBack: { done: ["Reviewed the account switch."], deviations: [], concerns: [], followUps: [] },
    });
    const finished = await capture(started);
    expect(finished.values[taskKey]).toBe("finished/1");
    expect(finished.keys[taskKey]).not.toBe(started.keys[taskKey]);

    await client.swarm.setSwarmState({ swarm, state: "SWARM_STATE_PAUSED" });
    const paused = await capture(finished);
    expect(paused.keys[taskKey]).toBe(finished.keys[taskKey]);
    expect(paused.keys[row + "/pause"]).toBeTruthy();
    await client.swarm.decideSwarmJoin({ swarm, joinId: join.id, accept: true });
    const accepted = await capture(paused);
    expect(accepted.keys[row + "/join"]).toBeTruthy();
    expect((await capture(paused, fixture.p("mara").id)).keys[row + "/join"]).toBeUndefined();

    const expected = ["swarm.task_started", "swarm.task_finished", "swarm.state_changed", "swarm.join_decided"];
    const filter = { repo, kinds: ["swarm.*"] };
    const { events } = await client.event.listEvents(filter);
    expect(events.map(({ kind }) => kind)).toEqual(expected);

    const replayed: string[] = [];
    // Subscribe stays open after replaying history. Stop at the last recorded swarm event.
    for await (const event of client.event.subscribe(filter, { signal: AbortSignal.timeout(3000) })) {
      replayed.push(event.kind);
      if (event.seq === events.at(-1)?.seq) break;
    }
    expect(replayed).toEqual(expected);
    expect(events.every(({ note }) => note?.target === "swarm:" + swarm.id)).toBe(true);
    expect(await list()).not.toContain("Your request to join was accepted.");
  });

  it("links from the principal to its runs in both modes and keeps access and adoption", async () => {
    const id = fixture.p("watcher").id;
    const expected = "/acme/api/agents/runs/" + fixture.swarms.runs.find((run) => run.trigger === "manual")!.id;
    const keeper = await principal(id);
    expect(count(keeper, /<h1\b/g)).toBe(1);
    expect(keeper).toContain('href="' + expected + '"');
    expect(keeper).toContain(">Access</h2>");
    expect(keeper).toContain(">Feed trigger</h2>");
    expect(keeper).toContain(">Recent work</h2>");
    request.mode = "bee";
    const agent = await principal(id);
    expect(count(agent, /<h1\b/g)).toBe(1);
    expect(agent).toContain('href="' + expected + '"');
    expect(agent).toContain('data-bee-page="agent-principal"');
    expect(agent).toContain('data-tool="auth_revoke"');
  });

  it("links a matching claim straight to its active swarm row and keeps other agent profiles reachable", async () => {
    const { items } = await api().swarm.listRepoWork({ repo });
    const holder = fixture.p("reviewer");
    const taskLease: Lease = {
      id: "rotation-tests", holder, paths: [pathBytes("tests/auth.test.ts")],
      repoId: "", ref: "main", mode: "LEASE_MODE_EXCLUSIVE", note: "",
    };
    const working = items.find((item) => item.swarm?.state === "SWARM_STATE_WORKING")!;
    expect(activityHref(repo.fullName, items, holder, taskLease)).toBe("/acme/api/agents#" + workAnchor(working));
    const billing = fixture.p("billing");
    expect(activityHref(repo.fullName, items, billing)).toBe("/acme/api/agents/" + billing.id);
  });

  it("maps every supported standalone run state to the documented cell", () => {
    const states: Record<NonNullable<AgentRun["state"]>, string> = {
      AGENT_RUN_STATE_UNSPECIFIED: "open",
      AGENT_RUN_STATE_QUEUED: "open",
      AGENT_RUN_STATE_PROVISIONING: "working",
      AGENT_RUN_STATE_RUNNING: "working",
      AGENT_RUN_STATE_WAITING_FOR_REVIEW: "working",
      AGENT_RUN_STATE_SUCCEEDED: "finished",
      AGENT_RUN_STATE_FAILED: "empty",
      AGENT_RUN_STATE_CANCELLED: "empty",
    };
    for (const [state, cell] of Object.entries(states)) {
      expect(runCells({ state: state as AgentRun["state"] })).toEqual([{ title: "Agent run", state: cell }]);
    }
  });
});
