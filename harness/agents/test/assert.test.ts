// The step engine (setup, commit fixtures, assertions) against the in-process fake server.

import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { Catalog } from "../src/catalog";
import { ConnectClient } from "../src/connect";
import { loadFixtures, type Step } from "../src/scenario";
import { runStep, runSteps, type Executor } from "../src/steps";
import { startFakeServer, type FakeServer } from "./fake-server";

const HERE = resolve(import.meta.dir, "..");
const catalog = Catalog.load();
let server: FakeServer;
let ex: Executor;

beforeAll(async () => {
  server = startFakeServer();
  ex = {
    client: new ConnectClient(server.url),
    catalog,
    adminToken: server.adminToken,
    ctx: {
      now: new Date(),
      runId: "assert-test",
      workdir: mkdtempSync(join(tmpdir(), "harness-assert-")),
      vars: { run: { ns: "acme", session: "sess-1", runtime: "test" }, scenario: { id: "AS-TEST-01" } },
    },
  };
  const base = loadFixtures(join(HERE, "fixtures")).get("base")!;
  const out = await runSteps(ex, base.setup, true);
  expect(out.every((o) => o.ok)).toBe(true);
});
afterAll(() => {
  server.stop();
  rmSync(ex.ctx.workdir!, { recursive: true, force: true });
});

const one = async (s: Step) => runStep(ex, s);

describe("setup fixtures", () => {
  test("the base fixture saves org, repo, worker and its token", () => {
    const v = ex.ctx.vars as Record<string, any>;
    expect(v.repo.fullName).toBe("acme/app");
    expect(v.worker.kind).toBe("PRINCIPAL_KIND_AGENT");
    expect(typeof v.workerToken).toBe("string");
  });
  test("a commit step uploads verified blobs, creates and lands a changeset", async () => {
    const r = await one({ commit: { repo: "acme/app", ref: "main", message: "seed", files: { "a.txt": "hello\n" } }, save: "seed" });
    expect(r).toMatchObject({ ok: true });
    expect(server.calls).toContain("WriteService/UploadObjects");
    expect((ex.ctx.vars as any).seed.ref.version).toBe("2");
  });
  test("a commit step points its branch as the harness admin even when an agent authored it", async () => {
    const r = await one({ commit: { repo: "acme/app", ref: "main", as: "${workerToken}", land: false, ref_name: "ws/by-worker", message: "branch", files: { "c.txt": "c\n" } } });
    expect(r).toMatchObject({ ok: true });
    const tip = await one({ call: "ObjectService/ResolveRef", request: { repo: { fullName: "acme/app" }, name: "ws/by-worker" } });
    expect(tip.ok).toBe(true);
  });
  test("a commit step can act as another principal, and its session lands in provenance", async () => {
    const r = await one({ commit: { repo: "acme/app", ref: "main", as: "${workerToken}", message: "by worker", files: { "b.txt": "b\n" } } });
    expect(r.ok).toBe(true);
    const log = await one({
      call: "QueryService/Log",
      request: { repo: { fullName: "acme/app" }, from: { expr: "main" }, page: { pageSize: 1 }, includeMeta: true },
      expect: [
        { path: "changesets[0].meta.provenance.session", eq: "sess-1" },
        { path: "changesets[0].meta.creator", eq: "${worker.id}" },
      ],
    });
    expect(log.details).toEqual(["ok changesets[0].meta.provenance.session eq", "ok changesets[0].meta.creator eq"]);
    expect(log.ok).toBe(true);
  });
  test("a refused commit passes only when it is refused with the wanted reason", async () => {
    const parentless = { repo: "acme/app", parent: "nope", land: false, message: "x", files: { "c.txt": "c" } };
    expect((await one({ commit: { ...parentless } })).ok).toBe(false);
    const landRefused = await one({
      commit: { repo: "acme/app", ref: "main", message: "stale", files: { "d.txt": "d" }, expect_error: { reason: "MERGE_CONFLICT" } },
    });
    // The fake server fast-forwards, so this commit lands and the expectation fails.
    expect(landRefused.ok).toBe(false);
    expect(landRefused.details.join()).toContain("the commit succeeded");
  });
});

describe("assertions", () => {
  test("passing and failing expectations", async () => {
    const pass = await one({ call: "ObjectService/ResolveRef", request: { repo: { fullName: "acme/app" }, name: "main" }, expect: [{ path: "ref.version", gte: 2 }] });
    expect(pass.ok).toBe(true);
    const failing = await one({ call: "ObjectService/ResolveRef", request: { repo: { fullName: "acme/app" }, name: "main" }, expect: [{ path: "ref.version", gt: 1000 }] });
    expect(failing.ok).toBe(false);
    expect(failing.details[0]).toStartWith("FAIL ref.version gt");
  });
  test("expect_error matches SPEC §12.2 reasons", async () => {
    const r = await one({ call: "ObjectService/ResolveRef", request: { repo: { fullName: "acme/app" }, name: "nope" }, expect_error: { reason: "NOT_FOUND" } });
    expect(r.ok).toBe(true);
    const wrong = await one({ call: "ObjectService/ResolveRef", request: { repo: { fullName: "acme/app" }, name: "nope" }, expect_error: { reason: "POLICY_DENIED" } });
    expect(wrong.ok).toBe(false);
    const unexpectedSuccess = await one({ call: "ObjectService/ResolveRef", request: { repo: { fullName: "acme/app" }, name: "main" }, expect_error: { reason: "NOT_FOUND" } });
    expect(unexpectedSuccess.ok).toBe(false);
  });
  test("bad credentials surface as UNAUTHENTICATED", async () => {
    const r = await one({ call: "AuthService/WhoAmI", as: "forged", request: {}, expect_error: { reason: "UNAUTHENTICATED" } });
    expect(r.ok).toBe(true);
  });
  test("server streams are collected as messages", async () => {
    const r = await one({
      call: "QueryService/ReadPath",
      request: { repo: { fullName: "acme/app" }, at: { expr: "main" }, path: "${b64:a.txt}" },
      expect: [{ path: "messages[0].data", decode: "base64", eq: "hello\n" }],
    });
    expect(r.ok).toBe(true);
    const missing = await one({ call: "QueryService/ReadPath", request: { repo: { fullName: "acme/app" }, at: { expr: "main" }, path: "${b64:zzz}" }, expect_error: { reason: "NOT_FOUND" } });
    expect(missing.ok).toBe(true);
  });
  test("each iterates and saves arrays", async () => {
    const r = await one({ call: "AuthService/CreateAgent", each: ["a", "b", "c"], request: { display: "${item}-${index}" }, save: { bots: "$" } });
    expect(r.ok).toBe(true);
    expect((ex.ctx.vars as any).bots.map((b: any) => b.display)).toEqual(["a-0", "b-1", "c-2"]);
  });
  test("retry polls until the state holds", async () => {
    let landed = false;
    setTimeout(async () => {
      await one({ commit: { repo: "acme/app", ref: "main", message: "late", files: { "late.txt": "late" } } });
      landed = true;
    }, 300);
    const r = await one({
      call: "QueryService/ReadPath",
      request: { repo: { fullName: "acme/app" }, at: { expr: "main" }, path: "${b64:late.txt}" },
      retry: { attempts: 20, interval_ms: 100 },
      expect: [{ path: "messages[0].data", decode: "base64", eq: "late" }],
    });
    expect(landed).toBe(true);
    expect(r.ok).toBe(true);
  });
  test("answer and workdir steps read only the agent's working directory", async () => {
    writeFileSync(join(ex.ctx.workdir!, "answer.json"), JSON.stringify({ paths: ["x", "y"] }));
    expect((await one({ answer: { file: "answer.json", expect: [{ path: "paths", len: 2 }] } })).ok).toBe(true);
    expect((await one({ answer: { file: "missing.json", expect: [{ path: "x", exists: true }] } })).ok).toBe(false);
    expect((await one({ workdir: { glob: "*.json", min: 1, max: 1 } })).ok).toBe(true);
    expect((await one({ workdir: { glob: "*.json", max: 0 } })).ok).toBe(false);
  });
  test("an unimplemented RPC fails the step with its reason", async () => {
    const r = await one({ call: "ChangeService/ListChanges", request: {}, expect: [{ path: "changes", len: 0 }] });
    expect(r.ok).toBe(false);
    expect(r.details[0]).toContain("UNIMPLEMENTED");
  });
});
