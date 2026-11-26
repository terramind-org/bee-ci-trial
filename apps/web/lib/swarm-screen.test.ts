import { renderToStaticMarkup } from "react-dom/server";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { FixtureStore, fixtureRpc } from "@/lib/api/fixture";
import { createClient } from "@/lib/api/client";
import SwarmPage from "@/app/(repo)/[owner]/[repo]/agents/swarms/[swarmId]/page";
import { spendingLimit } from "@/components/agents/swarm-actions";

const request = vi.hoisted(() => {
  process.env.BEE_ADAPTER = "fixture";
  return { mode: "keeper", token: "mara-token-0001" };
});

vi.mock("next/headers", () => ({
  cookies: async () => ({
    get: (name: string) => name === "bee_mode" ? { value: request.mode } : name === "bee_token" ? { value: request.token } : undefined,
  }),
  headers: async () => new Headers(),
}));

vi.mock("next/navigation", () => ({
  useRouter: () => ({ refresh: vi.fn() }),
  notFound: () => { throw new Error("Repository not found"); },
}));

const repo = { fullName: "acme/api" };
const params = (swarmId: string) => Promise.resolve({ owner: "acme", repo: "api", swarmId });

function working(store: FixtureStore) {
  return store.swarms.swarms.find((row) => row.state === "SWARM_STATE_WORKING")!;
}

async function renderSwarm(swarmId: string) {
  return renderToStaticMarkup(await SwarmPage({ params: params(swarmId) }));
}

describe("swarm detail on the demo repository", () => {
  let store: FixtureStore;

  beforeEach(() => {
    request.mode = "keeper";
    request.token = "mara-token-0001";
    store = new FixtureStore({ anchor: Date.UTC(2026, 10, 25), clock: () => 0, live: false });
    globalThis.__beeFixture = store;
  });

  it("shows the goal, all four task cells, attempted work, accountable people and confirmed decisions", async () => {
    const swarm = working(store);
    const html = await renderSwarm(swarm.id);
    expect(html.match(/<h1\b/g)).toHaveLength(1);
    expect(html).toContain(swarm.goal);
    expect(html).toContain(swarm.status);
    const linked = store.crByKey("rotate");
    expect(html).toContain('href="/acme/api/changes/' + linked.id + '"');
    expect(html).toContain("Pull request #" + linked.number);
    expect(html.match(/data-cell="/g)).toHaveLength(swarm.tasks.length);
    expect(html).toContain('aria-label="Map the role downgrade path · Finished"');
    expect(html).toContain('aria-label="Review the rotation regression tests · Being worked on"');
    expect(html).toContain('aria-label="Check the API guide for stale claims · Came back empty"');
    expect(html).toContain('aria-label="Verify account switching · Open"');
    for (const fact of ["tests/auth.test.ts", "Assigned to", "Attempts", "Refused", "Repository rules deny access", "Hand-back", "Done", "Deviations", "Concerns", "Follow-ups", "Queen · lead", "Mara Okafor", "Lin Zhou", "Dev Patel", "Your message goes to the lead only."]) {
      expect(html).toContain(fact);
    }
    expect(html).toContain('data-agent-confirm="Approves the join request from Dev Patel"');
    expect(html).toContain('data-agent-confirm="Declines the join request from Dev Patel"');
    expect(html).toContain('data-agent-confirm="Stops the swarm and cancels its active runs"');
    expect(html).toContain(">Pause<");
    expect(html).not.toContain('aria-label="Join this swarm"');

    request.mode = "bee";
    const agent = await renderSwarm(swarm.id);
    expect(agent.match(/<h1\b/g)).toHaveLength(1);
    expect(agent).toContain('data-bee-page="swarm"');
    expect(agent).toContain("tools on this page");
    expect(agent).toContain('data-tool="swarm_send_swarm_message"');
    expect(agent).toContain('data-tool="swarm_request_swarm_join"');
    expect(agent).toContain("Repository rules deny access");
    expect(agent).toContain("Dev Patel");
    expect(agent).not.toContain("data-agent-confirm=");
  });

  it("shows Resume and lets a different person offer an owned non-pending agent", async () => {
    const swarm = working(store);
    const client = createClient(fixtureRpc(store, request.token));
    await client.swarm.setSwarmState({ swarm: { repo, id: swarm.id }, state: "SWARM_STATE_PAUSED" });
    const paused = await renderSwarm(swarm.id);
    expect(paused).toContain(">Resume<");
    expect(paused).not.toContain(">Pause<");

    const tokenId = "swarm-screen-dev";
    store.tokens.push({
      tokenId, parentTokenId: "", principalId: store.p("dev").id,
      scope: { repos: [], refs: [], readPaths: [], writePaths: [], actions: [], session: "" },
      revoked: false, label: "Swarm screen test",
    });
    request.token = "swarm-screen-dev-secret";
    store.tokenSecrets.set(request.token, tokenId);
    const join = await renderSwarm(swarm.id);
    expect(join).toContain('aria-label="Join this swarm"');
    expect(join).toContain('value="' + store.p("deps").id + '"');
    expect(join).not.toContain('value="' + store.p("billing").id + '"');
    expect(join).toContain("Spending limit (USD)");
    expect(join).not.toContain("data-agent-confirm=");
  });
});

describe("join budget", () => {
  it("converts USD exactly and rejects out-of-range and malformed amounts", () => {
    expect(spendingLimit("1.02")).toBe("1020000");
    expect(spendingLimit(".000001")).toBe("1");
    expect(spendingLimit("18446744073709.551615")).toBe("18446744073709551615");
    for (const input of ["0", "-2", "1e8", "1.0000001", "18446744073709.551616"]) {
      expect(spendingLimit(input)).toBeUndefined();
    }
  });
});
