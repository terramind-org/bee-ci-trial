#!/usr/bin/env bun
// A scripted stand-in for an agent runtime, driven through the `command:` runtime. It receives
// the prompt as its argument and BEE_ENDPOINT/BEE_TOKEN in its environment, like a real
// agent, and behaves according to FAKE_AGENT_MODE:
//   good   fix the file named in the prompt through the API and land it
//   noop   do nothing (the assertions must fail)
//   slow   sleep far past any test timeout (the harness must kill it)
//   flaky  good, except on every third invocation (counted in FAKE_AGENT_COUNTER)
//   leak   write the environment to env.json (proves no admin token reaches agents)

import { readFileSync, writeFileSync, existsSync } from "node:fs";
import { blobId } from "../src/blake3";
import { ConnectClient } from "../src/connect";

const mode = process.env.FAKE_AGENT_MODE ?? "good";
const prompt = process.argv[2] ?? "";
const client = new ConnectClient(process.env.BEE_ENDPOINT ?? "", process.env.BEE_TOKEN);

async function fix(): Promise<void> {
  const repo = /repository ([\w.-]+\/[\w.-]+)/.exec(prompt)?.[1];
  const path = /`([^`]+\.py)`/.exec(prompt)?.[1];
  if (!repo || !path) throw new Error("prompt did not name a repo and a file");
  const tip = await client.unary("ObjectService", "ResolveRef", { repo: { fullName: repo }, name: "main" });
  const parent = (tip.body as any).ref.target;
  const bytes = new TextEncoder().encode("TAX_RATE = 0.07\n");
  const id = Buffer.from(blobId(bytes)).toString("base64");
  const up = await client.clientStream("WriteService", "UploadObjects", [
    { repo: { fullName: repo }, id: { raw: id }, offset: "0", data: Buffer.from(bytes).toString("base64"), last: true },
  ]);
  if (!up.ok) throw new Error(`upload: ${up.error?.reason}`);
  const cs = await client.unary("WriteService", "CreateChangeset", {
    repo: { fullName: repo },
    parents: [parent],
    delta: [{ path: Buffer.from(path).toString("base64"), put: { mode: "MODE_FILE", blob: { raw: id }, size: String(bytes.length) } }],
    changeId: { raw: Buffer.alloc(16, 7).toString("base64") },
    message: "Fix the tax rate",
  });
  if (!cs.ok) throw new Error(`create: ${cs.error?.reason}`);
  const land = await client.unary("WriteService", "Land", { repo: { fullName: repo }, ref: "main", head: (cs.body as any).changeset.id });
  if (!land.ok) throw new Error(`land: ${land.error?.reason}`);
  writeFileSync("answer.json", JSON.stringify({ landed: path }));
}

if (mode === "slow") {
  await Bun.sleep(600_000);
} else if (mode === "leak") {
  writeFileSync("env.json", JSON.stringify(process.env));
  await fix();
} else if (mode === "flaky") {
  const file = process.env.FAKE_AGENT_COUNTER!;
  const n = (existsSync(file) ? Number(readFileSync(file, "utf8")) : 0) + 1;
  writeFileSync(file, String(n));
  if (n % 3 !== 0) await fix();
} else if (mode === "good") {
  await fix();
}
