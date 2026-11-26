import { describe, expect, it } from "vitest";
import { bytesToBase64, parseObjectId, textToBase64 } from "./api/ids";
import type { ObjectId, Principal } from "./api/types";
import { answerableFor, branchOf, digestText, directoryOf, mergeVerb, otherHolds, whyPerson } from "./pulls";

const id = (hexByte: string): ObjectId => parseObjectId("c1_" + hexByte.repeat(32)) as ObjectId;
const person = (pid: string, display: string): Principal => ({ id: pid, kind: "PRINCIPAL_KIND_HUMAN", display, ownerId: "", runtime: "", externalIds: [] });
const agent = (pid: string, ownerId: string): Principal => ({ id: pid, kind: "PRINCIPAL_KIND_AGENT", display: pid, ownerId, runtime: "codex", externalIds: [] });

describe("pull request words", () => {
  it("finds the branch as the ref at the head, never the target", () => {
    const refs = [
      { name: "main", target: id("aa") },
      { name: "ws/billing/tax", target: id("bb") },
      { name: "ws/alias", target: id("bb") },
    ];
    expect(branchOf(refs, id("bb"), "main")).toBe("ws/alias");
    expect(branchOf(refs, id("aa"), "main")).toBe("");
    expect(branchOf(refs, id("cc"), "main")).toBe("");
    expect(branchOf(refs, undefined, "main")).toBe("");
  });

  it("names the person who answers: themselves, or the agent's owner when known", () => {
    const mara = person("mara", "Mara Okafor");
    const dir = directoryOf([mara, undefined], [agent("triage", "mara")]);
    expect(answerableFor(mara, dir)).toBe(mara);
    expect(answerableFor(agent("triage", "mara"), dir)).toBe(mara);
    // An owner the page could not load is left out, not printed as an id.
    expect(answerableFor(agent("deps", "nobody"), dir)).toBeNull();
    expect(answerableFor(agent("orphan", ""), dir)).toBeNull();
    expect(answerableFor(undefined, dir)).toBeNull();
  });

  it("says which rule sent a pull request to a person", () => {
    expect(whyPerson(["approval:human:sensitive (1 of 2)"])).toEqual(["A change to a path the rules mark sensitive needs two approvals from a person. It has one."]);
    expect(whyPerson(["approval:owner:src/billing (0 of 1)"])).toEqual(["It touches src/billing, which needs one approval from an owner of that path."]);
    expect(whyPerson(["approval:owner:src/billing (0 of 1)"], { "src/billing": ["Dev Patel", "Mara Okafor"] })).toEqual(["It touches src/billing, which needs one approval from Dev Patel and Mara Okafor."]);
    expect(whyPerson(["approval:any (0 of 1)"])).toEqual(["It needs one approval, from a person or another agent."]);
    expect(whyPerson(["approval:human"])).toEqual(["This change needs one approval from a person."]);
    // Checks, leases and stack parents are not about a person.
    expect(whyPerson(["check:test (failed)", "lease:exclusive:src/auth"])).toEqual([]);
    expect(otherHolds(["check:test (running)", "approval:any (0 of 1)", "stack: parent Fix rounding must land first"])).toEqual(["It is waiting for the test check.", "It waits for “Fix rounding” to merge first."]);
  });

  it("counts commits only when it knows them", () => {
    expect(mergeVerb("CHANGE_STATE_OPEN", 3)).toBe("wants to merge three commits into");
    expect(mergeVerb("CHANGE_STATE_OPEN", 1)).toBe("wants to merge one commit into");
    expect(mergeVerb("CHANGE_STATE_OPEN", undefined)).toBe("wants to merge into");
    expect(mergeVerb("CHANGE_STATE_LANDED", 2)).toBe("merged two commits into");
    expect(mergeVerb("CHANGE_STATE_CLOSED", undefined)).toBe("wanted to merge into");
  });

  it("prints a prompt digest as hex, whichever way the bytes carry it", () => {
    expect(digestText(bytesToBase64(new Uint8Array([0x9f, 0x2c, 0x00, 0x41])))).toBe("9f2c0041");
    expect(digestText(textToBase64("b8152e3ad0ed5346"))).toBe("b8152e3ad0ed5346");
    expect(digestText("")).toBe("");
    expect(digestText(undefined)).toBe("");
  });
});
