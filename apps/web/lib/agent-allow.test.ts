import { describe, expect, it, vi } from "vitest";
import { allowStep, decideStep, hasWebMcp, registerSafely } from "./agent-allow";

describe("what a browser's agent may do on the page", () => {
  it("runs a step no person was asked to guard", async () => {
    expect(decideStep({}, null)).toBe("run");
    expect(decideStep({}, "PRINCIPAL_KIND_AGENT")).toBe("run");
    const ask = vi.fn();
    expect(await allowStep({}, "PRINCIPAL_KIND_HUMAN", ask)).toBe(true);
    expect(ask).not.toHaveBeenCalled();
  });

  it("asks the signed-in person about a guarded step and takes their answer", async () => {
    const step = { confirm: "Approves and merges pull request #41" };
    expect(decideStep(step, "PRINCIPAL_KIND_HUMAN")).toBe("ask");
    const yes = vi.fn(async () => true);
    expect(await allowStep(step, "PRINCIPAL_KIND_HUMAN", yes)).toBe(true);
    expect(yes).toHaveBeenCalledWith("Approves and merges pull request #41");
    expect(await allowStep(step, "PRINCIPAL_KIND_HUMAN", async () => false)).toBe(false);
  });

  it("refuses a guarded step when no person is signed in, without asking", async () => {
    const step = { confirm: "Revokes this token" };
    const ask = vi.fn(async () => true);
    for (const kind of ["PRINCIPAL_KIND_AGENT", "PRINCIPAL_KIND_UNSPECIFIED", null, undefined] as const) {
      expect(decideStep(step, kind)).toBe("refuse");
      expect(await allowStep(step, kind, ask)).toBe(false);
    }
    expect(ask).not.toHaveBeenCalled();
  });

  it("finds WebMCP only where the browser offers it", () => {
    expect(hasWebMcp(undefined)).toBe(false);
    expect(hasWebMcp({})).toBe(false);
    expect(hasWebMcp({ modelContext: {} })).toBe(false);
    expect(hasWebMcp({ modelContext: { registerTool: () => {} } })).toBe(true);
  });
  it("survives a browser whose registerTool returns nothing, throws, or is missing", async () => {
    // The shape UI SDK expects: it calls `.catch` on whatever registerTool returns.
    const like = (doc: { modelContext: { registerTool: (...a: unknown[]) => unknown } }) => () => {
      (doc.modelContext.registerTool({ name: "snapshot" }) as Promise<unknown>).catch(() => {});
      return "registered";
    };
    const silent = { modelContext: { registerTool: vi.fn(() => undefined) } };
    expect(registerSafely(like(silent), silent)).toBe("registered");
    expect(silent.modelContext.registerTool).toHaveBeenCalledWith({ name: "snapshot" });

    const throwing = { modelContext: { registerTool: () => { throw new Error("no"); } } };
    expect(registerSafely(like(throwing), throwing)).toBe("registered");

    expect(registerSafely(() => "never", {})).toBeUndefined();
    expect(registerSafely(() => { throw new Error("library bug"); }, silent)).toBeUndefined();
  });

  it("puts the browser's own registerTool back afterwards", () => {
    const own = () => undefined;
    const doc = { modelContext: { registerTool: own } };
    registerSafely(() => {}, doc);
    expect(doc.modelContext.registerTool).toBe(own);
  });
});
