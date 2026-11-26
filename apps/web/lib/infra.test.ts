import { createElement as h } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { afterEach, describe, expect, it, vi } from "vitest";
import { wireOf } from "./api/ops";
import { BeeError } from "./api/transport";
import { objectIdText, parseObjectId } from "./api/ids";
import type { Changeset, ObjectId, RefLogEntry } from "./api/types";
import { DegradedBanner } from "../components/degraded-banner";
import { ImportedBadge, PrincipalName } from "../components/domain";
import { RuleList } from "../components/brief/rules";
import { LinearRefusal } from "../components/change-actions";
import { StackList } from "../app/(repo)/[owner]/[repo]/changes/[id]/stack-panel";
import { ArchiveRefusal } from "../app/(repo)/[owner]/[repo]/releases/archive-links";
import {
  DEGRADED_EVENT,
  LINEAR_MESSAGE,
  archiveRefusalText,
  degradedMessage,
  explainRequirement,
  isImported,
  isImporterPrincipal,
  isLinearRefusal,
  isOutageError,
  landEntryFor,
  landedStack,
  openStack,
} from "./infra";

vi.mock("next/navigation", () => ({ useRouter: () => ({ refresh: () => {}, push: () => {} }) }));

const oid = (n: number): ObjectId => parseObjectId("c1_" + n.toString(16).padStart(2, "0").padEnd(64, "0")) as ObjectId;
const short = (n: number) => objectIdText(oid(n), true);
const cid = (n: number) => ({ raw: Buffer.from(n.toString(16).padStart(32, "0"), "hex").toString("base64") });
const cs = (id: number, parents: number[]): Changeset => ({ id: oid(id), parents: parents.map(oid), delta: [], filtered: false, generation: "1" });

describe("linear history refusal", () => {
  it("says it in words, then the raw token, then the remedy", () => {
    const html = renderToStaticMarkup(h(LinearRefusal));
    expect(html).toContain("This pull request contains a merge commit and the target branch requires linear history.");
    expect(html).toContain("POLICY_DENIED · linear");
    expect(html).toContain("Rebase the merge away");
    expect(html).toContain("ask an admin to change the rule");
    expect(explainRequirement("linear")).toBe(LINEAR_MESSAGE);
  });

  it("shows among the rules when EvaluateLanding names the `linear` requirement, and keeps the other requirements", () => {
    const evaluation = { allowed: false, satisfied: ["check:lint"], missing: ["linear", "check:test"], diffClass: "" };
    const html = renderToStaticMarkup(h(RuleList, { evaluation }));
    expect(html).toContain('data-testid="linear-refusal"');
    expect(html).toContain("Check test");
    // The raw requirement stays beside the words, for an agent to match on.
    expect(html).toContain(">check:test<");
    expect(html).toContain(">check:lint<");
    expect(html.match(/linear history/g)?.length).toBe(1);
    // The list is read-only: the decisions are the buttons under the title.
    expect(html).not.toContain("<button");
    const clean = renderToStaticMarkup(h(RuleList, { evaluation: { ...evaluation, allowed: true, missing: [] } }));
    expect(clean).not.toContain("linear-refusal");
  });

  it("recognises a refused land by reason and requirement, and nothing else", () => {
    const denied = (missing: string[]) => new BeeError("permission_denied", { reason: "POLICY_DENIED", message: "denied", policyDenial: { rule: "main", missing } });
    expect(isLinearRefusal(denied(["linear"]))).toBe(true);
    expect(isLinearRefusal(denied(["check:test"]))).toBe(false);
    expect(isLinearRefusal(new BeeError("failed_precondition", { reason: "MERGE_IN_STACK", message: "x" }))).toBe(false);
    expect(isLinearRefusal(new Error("linear"))).toBe(false);
  });
});

describe("stack members", () => {
  it("marks a merge member with the glyph, the label and its other parents as short ids", () => {
    // 4 is a merge of 3 (first parent) and 7; 3 sits on the target tip 1.
    const members = openStack(oid(4), [cs(4, [3, 7]), cs(3, [1]), cs(7, [1])]);
    expect(members.map((m) => m.merge)).toEqual([true, false]);
    expect(members[0]?.otherParents).toEqual([short(7)]);
    const html = renderToStaticMarkup(h(StackList, { members }));
    expect(html).toContain(">merge<");
    expect(html).toContain("also parents:");
    expect(html).toContain(short(7));
    expect(html.match(/data-testid="stack-member"/g)?.length).toBe(2);
    expect(html.match(/data-icon="branch"/g)?.length).toBe(1);
  });

  it("follows the first parent only, so side branches land with their merge instead of listing", () => {
    const members = openStack(oid(4), [cs(7, [1]), cs(3, [1]), cs(4, [3, 7])]);
    expect(members).toHaveLength(2);
    expect(members.some((m) => m.short === short(7))).toBe(false);
  });
});

describe("landed empty", () => {
  const entry = (landed: RefLogEntry["landed"]): RefLogEntry => ({ version: "9", old: oid(1), new: oid(3), principal: "p", landed });
  const l = (change: number, source: number, result: number, skipped = false) => ({ changeId: cid(change), source: oid(source), result: oid(result), skipped });

  it("is a member whose result is the running tip it started from", () => {
    const members = landedStack(entry([l(1, 10, 2), l(2, 11, 2), l(3, 12, 3)]), () => []);
    expect(members.map((m) => m.empty)).toEqual([false, true, false]);
    const html = renderToStaticMarkup(h(StackList, { members }));
    expect(html.match(/merged empty \(no changes after rebase\)/g)?.length).toBe(1);
  });

  it("is not a skipped member, and the first member compares with the ref's old target", () => {
    expect(landedStack(entry([l(1, 10, 1)]), () => [])[0]?.empty).toBe(true);
    expect(landedStack(entry([l(1, 10, 1, true)]), () => [])[0]?.empty).toBe(false);
  });

  it("finds the ref-log entry that names the change", () => {
    const hit = entry([l(5, 10, 2)]);
    expect(landEntryFor([entry([l(6, 10, 2)]), hit], [cid(5)])).toBe(hit);
    expect(landEntryFor([hit], [cid(9)])).toBeUndefined();
  });
});

describe("degraded banner", () => {
  it("says how old the data is when the server says", () => {
    const html = renderToStaticMarkup(h(DegradedBanner, { degraded: true, ageSeconds: 42 }));
    expect(html).toContain("Metadata is degraded. Showing data up to 42 s old; changes cannot be saved right now.");
    expect(html).toContain('data-testid="degraded-banner"');
  });

  it("does not make up a number when the server gave none, and stays out of the way when healthy", () => {
    expect(renderToStaticMarkup(h(DegradedBanner, { degraded: true, ageSeconds: null }))).toContain("Showing data that may be out of date; changes cannot be saved right now.");
    expect(renderToStaticMarkup(h(DegradedBanner, { degraded: false, ageSeconds: null }))).toBe("");
    expect(degradedMessage(0)).toContain("up to 0 s old");
  });

  it("is raised by a write that fails with UNAVAILABLE, not by contention", () => {
    expect(isOutageError(new BeeError("unavailable", { reason: "UNAVAILABLE", message: "store down", retryable: true }))).toBe(true);
    expect(isOutageError(new BeeError("unavailable", { reason: "LAND_CONTENTION", message: "busy", retryable: true }))).toBe(false);
    expect(DEGRADED_EVENT).toBe("bee:metadata-degraded");
  });
});

describe("imports", () => {
  const importer = { id: "p1", kind: "PRINCIPAL_KIND_SERVICE" as const, display: "acme importer", ownerId: "", runtime: "", externalIds: [] };

  it("names the importer service principal as '<org> importer' with the service diamond", () => {
    const html = renderToStaticMarkup(h(PrincipalName, { principal: importer }));
    expect(html).toContain("acme importer");
    expect(html).toContain("acme importer (service)");
    // The service avatar: a square tile (rx 12% of the 40-unit view), not a human's circle.
    expect(html).toContain('rx="4.8"');
    expect(isImporterPrincipal(importer)).toBe(true);
    expect(isImporterPrincipal({ ...importer, kind: "PRINCIPAL_KIND_HUMAN" })).toBe(false);
  });

  it("reads import provenance from the git extension of the meta, and only that", () => {
    const meta = { author: "a", committer: "c", creator: "p1", message: "m" };
    expect(isImported({ ...meta, extensions: [{ key: 64, value: "Zm9v" }] })).toBe(true);
    expect(isImported({ ...meta, extensions: [{ key: 65, value: "Zm9v" }] })).toBe(false);
    expect(isImported(meta)).toBe(false);
    expect(isImported(undefined)).toBe(false);
    expect(renderToStaticMarkup(h(ImportedBadge))).toContain(">imported<");
  });
});

describe("epochs", () => {
  it("GetServerInfo carries the repo, so the epochs are that repo's", () => {
    const w = wireOf("ObjectService", "GetServerInfo", { repo: { fullName: "acme/api" } });
    expect(w.method).toBe("GetServerInfo");
    expect(w.request).toEqual({ repo: { fullName: "acme/api" } });
  });
});

describe("source archive refusals", () => {
  it("prints the reason and the server's words in place of the link", () => {
    const denied = renderToStaticMarkup(h(ArchiveRefusal, { reason: "POLICY_DENIED", message: "x" }));
    expect(denied).toContain("You cannot read every path in this revision");
    expect(denied).toContain("POLICY_DENIED");
    const redacted = renderToStaticMarkup(h(ArchiveRefusal, { reason: "REDACTED", message: "x" }));
    expect(redacted).toContain("contains redacted content");
    expect(redacted).not.toContain("<a ");
    expect(archiveRefusalText("UNAVAILABLE", "metadata is down")).toBe("metadata is down");
  });
});

describe("requestSourceArchive", () => {
  afterEach(() => {
    vi.resetModules();
    vi.unstubAllGlobals();
    vi.doUnmock("@/lib/api/server");
  });

  async function withServer(getSourceArchiveUrl: () => Promise<unknown>) {
    vi.resetModules();
    vi.doMock("@/lib/api/server", () => ({ serverClient: async () => ({ release: { getSourceArchiveUrl } }) }));
    return (await import("../app/(repo)/[owner]/[repo]/releases/archive-actions")).requestSourceArchive;
  }
  const TAR = "SOURCE_ARCHIVE_FORMAT_TAR_GZ" as const;

  it("returns the server's reason when the caller cannot read every path", async () => {
    const ask = await withServer(async () => {
      throw new BeeError("permission_denied", { reason: "POLICY_DENIED", message: "cannot read /secret" });
    });
    expect(await ask("acme/api", "c1_aa", TAR)).toEqual({ ok: false, reason: "POLICY_DENIED", message: "cannot read /secret" });
  });

  it("turns a redacted tree's 404 into REDACTED instead of a link that fails", async () => {
    const ask = await withServer(async () => ({ url: "http://srv/archives/x/snap.tar.gz?sig=1", filtered: false }));
    vi.stubGlobal("fetch", async () => new Response("REDACTED: config.yml is redacted; this tree cannot be archived", { status: 404 }));
    const out = await ask("acme/api", "c1_aa", TAR);
    expect(out).toMatchObject({ ok: false, reason: "REDACTED" });
  });

  it("hands over the URL when the archive is served, and refuses a malformed request without calling", async () => {
    const call = vi.fn(async () => ({ url: "http://srv/a.zip?sig=1", filtered: true }));
    const ask = await withServer(call);
    vi.stubGlobal("fetch", async () => new Response("zip", { status: 200 }));
    expect(await ask("acme/api", "c1_aa", "SOURCE_ARCHIVE_FORMAT_ZIP")).toEqual({ ok: true, url: "http://srv/a.zip?sig=1", filtered: true });
    expect(await ask("not a repo", "c1_aa", TAR)).toMatchObject({ ok: false, reason: "INVALID_ARGUMENT" });
    expect(call).toHaveBeenCalledTimes(1);
  });
});
