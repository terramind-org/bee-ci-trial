// Scenario files: every PRODUCT §4 scenario has a file and every file validates against the
// proto catalog; the validator itself rejects the mistakes it exists to catch.

import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { Catalog, repoRoot } from "../src/catalog";
import { dryRun, parseArgs, HELP } from "../src/cli";
import {
  errorReasons, loadFixtures, loadYaml, productScenarios, scenarioFiles, validateScenario,
  type Scenario, type ValidationContext,
} from "../src/scenario";

const HERE = resolve(import.meta.dir, "..");
const root = repoRoot();
const catalog = Catalog.load(root);
const fixtures = loadFixtures(join(HERE, "fixtures"));
const product = productScenarios(join(root, "docs", "PRODUCT.md"));
const reasons = errorReasons(catalog.protoDir);
const ctx: ValidationContext = { catalog, reasons, fixtures, product };
const files = scenarioFiles(join(HERE, "scenarios"));

describe("catalog", () => {
  test("loads every RPC and resolves names", () => {
    expect(catalog.methods.size).toBeGreaterThan(1000);
    expect(catalog.resolve("WriteService/Land")?.input).toBe("bee.v1.LandRequest");
    expect(catalog.resolve("bee.v1.QueryService/ReadPath")?.serverStreaming).toBe(true);
    expect(catalog.resolve("WriteService/UploadObjects")?.clientStreaming).toBe(true);
    expect(catalog.resolve("NoSuchService/Nope")).toBeUndefined();
  });
  test("reads the SPEC §12.2 reasons from common.proto", () => {
    expect(reasons.has("POLICY_DENIED")).toBe(true);
    expect(reasons.has("BUDGET_EXHAUSTED")).toBe(true);
    expect(reasons.has("UNSPECIFIED")).toBe(false);
  });
});

describe("PRODUCT §4 scenarios", () => {
  test("PRODUCT lists agent scenarios", () => {
    // The count is whatever PRODUCT §4 says; the next test pins each one to a file.
    expect(product.size).toBeGreaterThan(0);
  });
  test("there is exactly one file per scenario", () => {
    const ids = files.map((f) => f.split("/").pop()!.replace(/\.yaml$/, "")).sort();
    expect(ids).toEqual([...product.keys()].sort());
  });
  for (const f of files) {
    const id = f.split("/").pop()!.replace(/\.yaml$/, "");
    test(`${id} validates`, () => {
      const s = loadYaml<Scenario>(f);
      expect(validateScenario(s, { ...ctx, file: f })).toEqual([]);
    });
  }
  test("no setup commit re-puts the README that autoInit already wrote", () => {
    // The server refuses a put that leaves the path unchanged, which kills the run in setup.
    for (const f of files) {
      const s = loadYaml<Scenario>(f);
      for (const step of s.setup ?? []) {
        if (!("commit" in step)) continue;
        const readme = (step.commit as { files?: Record<string, string | null> }).files?.["README.md"];
        expect([f, readme === "# app\n"]).toEqual([f, false]);
      }
    }
  });
  test("token scopes use real actions and repo names the server matches", () => {
    // Action names mirror bee-policy's Action enum; the server matches scope repos against
    // full names, so a repo id in `repos` denies every ref and change-request write.
    const actions = new Set(["read", "triage", "review", "commit", "lease", "land", "manage_refs", "force_update", "redact", "purge", "manage_policy", "manage_settings", "issues_write"]);
    for (const f of [...files, ...[...fixtures.keys()].map((k) => join(HERE, "fixtures", `${k}.yaml`))]) {
      const text = readFileSync(f, "utf8");
      expect([f, /repos:\s*\[\s*"\$\{repo\.id\}"/.test(text)]).toEqual([f, false]);
      for (const m of text.matchAll(/actions:\s*\[([^\]]*)\]/g)) {
        // Other `actions:` keys (ruleset evaluation) list SCREAMING_CASE enums, not token actions.
        for (const a of m[1]!.split(",").map((x: string) => x.trim()).filter((x: string) => x && !/^[A-Z]/.test(x))) expect([f, a, actions.has(a)]).toEqual([f, a, true]);
      }
    }
  });
  test("--dry-run --scenarios all exits 0", () => {
    const lines: string[] = [];
    expect(dryRun("all", (l) => lines.push(l))).toBe(0);
    expect(lines.at(-1)).toContain("0 problem(s)");
  });
  test("--dry-run reports a missing scenario id", () => {
    expect(dryRun(["AS-NOPE-01"], () => {})).toBe(1);
  });
});

describe("the validator rejects", () => {
  const base = () => loadYaml<Scenario>(join(HERE, "scenarios", "AS-CORE-01.yaml"));
  const errs = (s: Scenario) => validateScenario(s, ctx);
  const assertion = (s: Scenario, i: number) => s.assertions[i] as unknown as Record<string, any>;

  test("unknown RPCs", () => {
    const s = base();
    s.assertions.push({ call: "QueryService/Nope", expect: [{ path: "x", exists: true }] });
    expect(errs(s).join("\n")).toContain("is not an RPC");
  });
  test("request fields the message does not have", () => {
    const s = base();
    assertion(s, 0).request.nmae = "main";
    expect(errs(s).join("\n")).toContain("ResolveRefRequest.nmae: no such field");
  });
  test("enum values the proto does not define", () => {
    const s = base();
    s.assertions.push({ call: "ChangeService/ListChanges", request: { state: "OPEN" }, expect: [{ path: "changes", len: 0 }] });
    expect(errs(s).join("\n")).toContain('"OPEN" is not one of');
  });
  test("asserted paths the response cannot have", () => {
    const s = base();
    assertion(s, 1).expect.push({ path: "changesets[0].colour", eq: 1 });
    expect(errs(s).join("\n")).toContain("changesets[0].colour: no such field");
  });
  test("reasons outside SPEC §12.2", () => {
    const s = base();
    s.assertions.push({ call: "ObjectService/ResolveRef", request: {}, expect_error: { reason: "NOPE" } });
    expect(errs(s).join("\n")).toContain("NOPE is not a SPEC §12.2 reason");
  });
  test("undefined template variables and functions", () => {
    const s = base();
    assertion(s, 0).request.name = "${nowhere.ref}";
    assertion(s, 1).request.repo.fullName = "${upper:x}";
    const e = errs(s).join("\n");
    expect(e).toContain("undefined variable ${nowhere.ref}");
    expect(e).toContain("unknown template function");
  });
  test("prompts that hint at commands or RPCs", () => {
    const s = base();
    s.prompt += "\nUse `bee land` or CreateChangeset.";
    const e = errs(s).join("\n");
    expect(e).toContain("names a CLI command");
    expect(e).toContain("names an RPC (CreateChangeset)");
  });
  test("predicates without exactly one operator, and bad regexes", () => {
    const s = base();
    assertion(s, 1).expect.push({ path: "changesets", len: 1, len_gte: 1 });
    assertion(s, 1).expect.push({ path: "changesets", matches: "(" });
    const e = errs(s).join("\n");
    expect(e).toContain("exactly one operator");
    expect(e).toContain("Invalid regular expression");
  });
  test("scenarios that assert nothing through the API", () => {
    const s = base();
    s.assertions = [{ answer: { file: "a.json", expect: [{ path: "x", exists: true }] } }];
    expect(errs(s).join("\n")).toContain("at least one assertion must check server state");
  });
  test("ids that are not in PRODUCT §4, and wrong areas", () => {
    const s = base();
    s.id = "AS-CORE-99";
    s.area = "4.2";
    const e = errs(s).join("\n");
    expect(e).toContain("is not an agent scenario in docs/PRODUCT.md");
  });
  test("agent files used before the agent ran", () => {
    const s = base();
    s.setup.push({ call: "AuthService/WhoAmI", as: "${file:token}", request: {} });
    expect(errs(s).join("\n")).toContain("only works after the agent ran");
  });
});

describe("cli", () => {
  test("parses options", () => {
    const { args, positional } = parseArgs(["--server", "http://x", "--runs=2", "--fixture", "a=b", "--fixture", "c=d", "--dry-run", "describe"]);
    expect(args.server).toBe("http://x");
    expect(args.runs).toBe("2");
    expect(args.fixture).toEqual(["a=b", "c=d"]);
    expect(args["dry-run"]).toBe(true);
    expect(positional).toEqual(["describe"]);
  });
  test("--help documents the run command", () => {
    expect(HELP).toContain("--runtimes");
    expect(HELP).toContain("--dry-run");
    expect(HELP).toContain("results.json");
  });
});
