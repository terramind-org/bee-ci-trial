// Exit test step 4: verify log, diff and blame over the API (HTTP/JSON), with assertions on the
// content of the changes made in steps 2 and 3: who made them, which lines they touched, and that
// blame attributes each line to the right changeset.

import { Api, assert, env, fail, log, rawHex, readState, runStep, unb64 } from "./lib/rpc";

/** Text form of a changeset id: `c1_` and the 32 hash bytes, without the algorithm and kind bytes. */
const idText = (rawHex: string): string => `c1_${rawHex.slice(4)}`;

await runStep("step 4: log, diff, blame", async () => {
  const e = env();
  const state = readState(e.work);
  if (!state.repo || !state.mcpChange || !state.gitChange || !state.mcpAgent || !state.pusher) {
    fail("step 4 needs the state steps 2 and 3 leave behind; run the steps in order");
  }
  const { repo, mcpChange, gitChange, mcpAgent, pusher } = state;
  const branch = state.defaultBranch ?? "main";
  const admin = new Api(e.endpoint, e.adminToken);
  const r = { fullName: repo };
  const path = mcpChange.path;
  const gitNote = "exit-test/git-note.txt";

  // ---- log ----------------------------------------------------------------------------------
  const full = await admin.call("QueryService", "Log", { repo: r, from: { expr: branch }, firstParent: true, page: { pageSize: 5 }, includeMeta: true });
  const [tip, second, third] = full.changesets;
  assert(tip && second && third, `log of ${branch} returns at least three changesets (${full.changesets.length} on the first page)`);
  assert(rawHex(tip.id) === gitChange.commit, `log[0] is the change pushed with git (${gitChange.commit.slice(0, 12)})`);
  assert(rawHex(second.id) === mcpChange.id, `log[1] is the change made over MCP (${mcpChange.id.slice(0, 12)})`);
  assert(rawHex(tip.parents?.[0]) === rawHex(second.id), `log[0]'s parent is log[1]`);
  assert(rawHex(second.parents?.[0]) === rawHex(third.id), `log[1]'s parent is log[2], the history before the exit test`);
  assert(tip.meta.creator === pusher.id && second.meta.creator === mcpAgent.id, `creators are the pusher and the MCP agent, in order`);
  assert(tip.meta.author === "Exit Test Pusher <pusher@exit-test.invalid>", `log[0] author is the git identity (${tip.meta.author})`);
  assert(second.meta.author === "Exit Test Agent", `log[1] author is "Exit Test Agent" (${second.meta.author})`);
  assert(/^Add a line and a note by git push/.test(tip.meta.message), `log[0] message is the pushed commit's subject`);
  assert(/^Add the exit test agent note/.test(second.meta.message), `log[1] message is the MCP change's subject`);
  assert(second.meta.provenance?.session === "exit-test-mcp", `log[1] carries the MCP token's session in its provenance`);
  assert(new Set(tip.delta.map((d: any) => unb64(d.path))).size === 2, `log[0] changes two paths`);

  const filtered = await admin.call("QueryService", "Log", { repo: r, from: { expr: branch }, path: Buffer.from(path).toString("base64"), page: { pageSize: 10 }, includeMeta: true });
  assert(
    filtered.changesets.length >= 2 && rawHex(filtered.changesets[0].id) === gitChange.commit && rawHex(filtered.changesets[1].id) === mcpChange.id,
    `log filtered to ${path} starts with the git change then the MCP change`,
  );
  const byAuthor = await admin.call("QueryService", "Log", { repo: r, from: { expr: branch }, author: mcpAgent.id, page: { pageSize: 10 } });
  assert(byAuthor.changesets.length === 1 && rawHex(byAuthor.changesets[0].id) === mcpChange.id, `log filtered to the MCP agent returns only its change`);

  // ---- diff ---------------------------------------------------------------------------------
  const diffOf = async (from: string, to: string) =>
    new Map<string, any>((await admin.stream("QueryService", "Diff", { repo: r, from: { expr: from }, to: { expr: to }, includeHunks: true, contextLines: 3 })).map((d) => [unb64(d.path), d]));
  const hunkLines = (item: any): string[] => (item.hunks ?? []).flatMap((h: any) => (h.lines as string[]).map((l) => l.replace(/\n$/, "")));

  const mcpDiff = await diffOf(idText(rawHex(third.id)), idText(mcpChange.id));
  assert(mcpDiff.size === 1 && mcpDiff.has(path), `diff before..MCP change touches exactly ${path}`);
  const mcpItem = mcpDiff.get(path);
  assert(mcpItem.status === "DIFF_STATUS_ADDED", `${path} is ADDED by the MCP change`);
  assert(
    JSON.stringify(hunkLines(mcpItem)) === JSON.stringify(mcpChange.lines.map((l) => `+${l}`)),
    `the MCP change's hunk is the three lines the agent wrote`,
  );

  const gitDiff = await diffOf(idText(mcpChange.id), idText(gitChange.commit));
  assert(gitDiff.size === 2 && gitDiff.has(path) && gitDiff.has(gitNote), `diff MCP change..git change touches ${path} and ${gitNote}`);
  assert(gitDiff.get(path).status === "DIFF_STATUS_MODIFIED", `${path} is MODIFIED by the git push`);
  const gitLines = hunkLines(gitDiff.get(path)).filter((l) => l.startsWith("+") || l.startsWith("-"));
  assert(JSON.stringify(gitLines) === JSON.stringify([`+${gitChange.addedLine}`]), `the git push added exactly one line to ${path} and removed none`);
  assert(gitDiff.get(gitNote).status === "DIFF_STATUS_ADDED", `${gitNote} is ADDED by the git push`);

  const both = await diffOf(idText(rawHex(third.id)), branch);
  assert(both.size === 2 && both.get(path).additions === 4, `diff before..${branch} shows ${path} with 4 additions`);

  // ---- blame --------------------------------------------------------------------------------
  const blame = await admin.call("QueryService", "Blame", { repo: r, at: { expr: branch }, path: Buffer.from(path).toString("base64") });
  const byLine: string[] = [];
  for (const range of blame.ranges) for (let i = 0; i < Number(range.lineCount); i++) byLine[Number(range.startLine) - 1 + i] = rawHex(range.changeset);
  log(`  blame ${path}: ${blame.ranges.map((x: any) => `${x.startLine}+${x.lineCount}=${rawHex(x.changeset).slice(0, 8)}`).join(" ")}`);
  assert(byLine.length === 4, `blame covers the file's 4 lines`);
  assert(byLine.slice(0, 3).every((c) => c === mcpChange.id), `lines 1-3 of ${path} are attributed to the MCP change`);
  assert(byLine[3] === gitChange.commit, `line 4 of ${path} is attributed to the git push`);
  const note = await admin.call("QueryService", "Blame", { repo: r, at: { expr: branch }, path: Buffer.from(gitNote).toString("base64") });
  assert(note.ranges.length === 1 && rawHex(note.ranges[0].changeset) === gitChange.commit, `${gitNote} is attributed to the git push`);
  const ranged = await admin.call("QueryService", "Blame", { repo: r, at: { expr: branch }, path: Buffer.from(path).toString("base64"), startLine: 4, endLine: 4 });
  assert(ranged.ranges.length === 1 && rawHex(ranged.ranges[0].changeset) === gitChange.commit, `blame of line 4 alone names the git push`);
});
