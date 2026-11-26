import { describe, expect, it } from "vitest";
import type { EventJson } from "../../gen/bee/v1/forge_pb";
import { noteOf } from "./forge";

const event = (kind: string, spec: unknown): EventJson => ({ kind, subject: { kind: "SUBJECT_KIND_CHANGE_REQUEST", id: "cr1" }, payloadJson: JSON.stringify({ id: "01M61E32KV1G9XRMJK331C4FA7", spec }) }) as EventJson;

describe("evidence events", () => {
  it("read as a sentence about the change request, never as the evidence id", () => {
    expect(noteOf(event("evidence.added", { title: "Unit tests" }))).toEqual({ verb: "added evidence", title: "Unit tests", target: "cr:cr1" });
    expect(noteOf(event("evidence.updated", { title: "Unit tests" }))?.verb).toBe("updated evidence");
    expect(noteOf(event("evidence.deleted", { title: "Unit tests" }))?.verb).toBe("removed evidence");
    expect(noteOf(event("evidence.added", undefined))?.title).toBe("to a change");
  });
});
