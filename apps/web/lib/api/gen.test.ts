/**
 * The generated code is committed; this fails when lib/gen differs from what protoc +
 * protoc-gen-es produce from proto/bee/v1 and apps/web/proto today (run `bun run gen`).
 */
import { execFileSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";

describe("generated code", () => {
  it("lib/gen is up to date with the protos", () => {
    const script = fileURLToPath(new URL("../../scripts/gen.mjs", import.meta.url));
    const out = execFileSync(process.execPath, [script, "--check"], { encoding: "utf8" });
    expect(out).toMatch(/up to date/);
  }, 60_000);
});
