// Exit test step 2: make a change with an agent token through `bee mcp` and land it.
//
// The harness mints a scoped agent token (AuthService CreateAgent + MintToken), spawns `bee mcp` with the
// token and drives it with fixed JSON-RPC calls: initialize, tools/list, then tools/call for
// ResolveRef, UploadObjects, CreateChangeset and Land. The land is then checked through the API
// with the admin token, not through MCP.

import { Api, assert, changeId, env, fail, log, mintAgent, ok, putFile, rawHex, readFile, readState, runStep, unb64, writeState } from "./lib/rpc";
import { McpClient } from "./lib/mcp";

const BEE = process.env.EXIT_TEST_BEE || "bee";
const SESSION = "exit-test-mcp";
const MCP_FILE = "exit-test/agent-note.txt";
const MCP_LINES = ["# Written by an agent over MCP", "The phase 1 exit test made this change.", "It lands on the default branch under policy."];

await runStep("step 2: mcp", async () => {
  const e = env();
  const state = readState(e.work);
  const repo = state.repo ?? "dev/playground";
  const [org] = repo.split("/");
  const branch = state.defaultBranch ?? "main";
  const admin = new Api(e.endpoint, e.adminToken);

  // Setup, as the admin: the agent and its token.
  const repoRow = await admin.call("RepoService", "GetRepo", { repo: { fullName: repo } });
  const repoId: string = repoRow.id ?? repoRow.repo?.id;
  if (!repoId) fail(`RepoService/GetRepo returned no id for ${repo}`);
  const agent = await mintAgent(admin, org!, repo, "exit-test-mcp-agent", SESSION);
  ok(`minted an agent token for ${agent.id} scoped to ${repo} (session ${SESSION})`);

  writeState(e.work, { repo, repoId, defaultBranch: branch, mcpAgent: agent });

  // The agent: `bee mcp` with only its token and the endpoint.
  const mcp = new McpClient([BEE, "mcp", "--toolsets", "core"], {
    BEE_ENDPOINT: e.endpoint,
    BEE_TOKEN: agent.token,
    BEE_CONFIG_DIR: `${e.work}/mcp-config`,
  });
  try {
    const init = await mcp.initialize();
    assert(init.serverInfo?.name, `MCP initialize: server ${init.serverInfo?.name} ${init.serverInfo?.version ?? ""}, protocol ${init.protocolVersion}`);
    const tools = (await mcp.listTools()).map((t) => t.name);
    for (const want of ["auth_who_am_i", "object_resolve_ref", "write_upload_objects", "write_create_changeset", "write_land", "query_log"]) {
      assert(tools.includes(want), `tools/list offers ${want} (${tools.length} tools in the core toolset)`);
    }

    const who = await mcp.call("auth_who_am_i", {});
    if (who.isError) fail(`auth_who_am_i: ${who.text}`);
    assert(JSON.stringify(who.data).includes(agent.id), `auth_who_am_i answers as the agent ${agent.id}`);

    const resolved = await mcp.call("object_resolve_ref", { repo: { fullName: repo }, name: branch });
    if (resolved.isError) fail(`object_resolve_ref: ${resolved.text}`);
    const parent = resolved.data.ref.target as { raw: string };
    const versionBefore = Number(resolved.data.ref.version);
    log(`  ${branch} is at version ${versionBefore}, tip ${rawHex(parent).slice(0, 12)}`);

    const file = putFile({ fullName: repo }, MCP_FILE, `${MCP_LINES.join("\n")}\n`, true);
    const up = await mcp.call("write_upload_objects", { messages: [file.upload] });
    if (up.isError) fail(`write_upload_objects: ${up.text}`);
    ok("write_upload_objects stored the blob");

    const created = await mcp.call("write_create_changeset", {
      repo: { fullName: repo },
      parents: [parent],
      delta: [file.delta],
      changeId: changeId("mcp-change"),
      author: "Exit Test Agent",
      message: "Add the exit test agent note\n\nMade through bee mcp by the phase 1 exit test.",
    });
    if (created.isError) fail(`write_create_changeset: ${created.text}`);
    const changeset = created.data.changeset;
    ok(`write_create_changeset made ${rawHex(changeset.id).slice(0, 12)}`);

    const landed = await mcp.call("write_land", { repo: { fullName: repo }, ref: branch, head: changeset.id });
    if (landed.isError) fail(`write_land: ${landed.text}`);
    const result = landed.data.landed?.[0]?.result ?? landed.data.ref?.target;
    ok(`write_land moved ${branch} to version ${landed.data.ref?.version}`);

    // Assertions through the API, not through MCP.
    const ref = await admin.call("ObjectService", "ResolveRef", { repo: { fullName: repo }, name: branch });
    assert(Number(ref.ref.version) > versionBefore, `${branch} advanced past version ${versionBefore} (now ${ref.ref.version})`);
    const log1 = await admin.call("QueryService", "Log", { repo: { fullName: repo }, from: { expr: branch }, page: { pageSize: 1 }, includeMeta: true });
    const tip = log1.changesets[0];
    assert(rawHex(tip.id) === rawHex(ref.ref.target), `the log's first entry is the branch tip`);
    assert(tip.meta?.creator === agent.id, `the tip was created by the agent (${tip.meta?.creator})`);
    assert(tip.meta?.provenance?.session === SESSION, `the tip carries the token's session ${SESSION} in its provenance`);
    assert(tip.delta?.length === 1 && unb64(tip.delta[0].path) === MCP_FILE, `the tip changes exactly ${MCP_FILE}`);
    assert(rawHex(tip.id) === rawHex(result) || rawHex(tip.parents?.[0]) === rawHex(parent), `the tip descends from the tip the agent read`);
    const content = await readFile(admin, repo, branch, MCP_FILE);
    assert(content === `${MCP_LINES.join("\n")}\n`, `${MCP_FILE} on ${branch} has the three lines the agent wrote`);
    const effective = JSON.parse((await admin.call("PolicyService", "GetEffectivePolicy", { repo: { fullName: repo } })).normalizedJson);
    assert(effective.land?.[branch], `the effective policy has a land rule for ${branch}, which the agent's land had to pass`);
    writeState(e.work, { mcpChange: { id: rawHex(tip.id), path: MCP_FILE, lines: MCP_LINES, session: SESSION } });
  } finally {
    await mcp.close();
  }
});
