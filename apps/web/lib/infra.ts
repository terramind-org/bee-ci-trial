/**
 * What the screens show for the server's recent infrastructure behaviour, as pure functions of
 * the wire data: merge changesets in a land stack (SPEC §7.4), members that land empty, degraded
 * metadata (SPEC §15.1), imported history (SPEC §17.4) and source archive refusals. Nothing here
 * invents a field: each helper reads one the server returns.
 */
import { changeIdText, objectIdText } from "./api/ids";
import type { Changeset, ChangeId, Meta, ObjectId, RefLogEntry } from "./api/types";
import { BeeError } from "./api/transport";

/** The requirement id of a `require_linear` land rule, in `LandingEvaluation.missing` and `PolicyDenial.missing` (SPEC §7.4). */
export const LINEAR_REQUIREMENT = "linear";

export const LINEAR_MESSAGE = "This stack contains a merge changeset and the target requires linear history.";
export const LINEAR_REMEDY = "Rebase the merge away so every member has one parent, or ask an admin to change the land rule.";

/** True when a missing-requirement list carries the linear-history refusal. */
export function refusesMerge(missing: readonly string[] | undefined): boolean {
  return !!missing?.includes(LINEAR_REQUIREMENT);
}

/** True when a failed land was refused by `require_linear`: POLICY_DENIED naming the `linear` requirement. */
export function isLinearRefusal(e: unknown): boolean {
  return e instanceof BeeError && e.reason === "POLICY_DENIED" && refusesMerge(e.detail.policyDenial?.missing);
}

/** A write or strong read failed because the metadata store is unavailable (SPEC §15.1): `UNAVAILABLE`, not `LAND_CONTENTION`. */
export const OUTAGE_REASON = "UNAVAILABLE";

export function isOutageError(e: unknown): boolean {
  return e instanceof BeeError && e.reason === OUTAGE_REASON;
}

/** Browser event the toast raises when a write fails with the outage reason, so the repo banner can show. */
export const DEGRADED_EVENT = "bee:metadata-degraded";

/** The banner's sentence. `ageSeconds` is the server's staleness when it reports one. */
export function degradedMessage(ageSeconds: number | null): string {
  const age = ageSeconds === null ? "data that may be out of date" : "data up to " + ageSeconds + " s old";
  return "Metadata is degraded. Showing " + age + "; changes cannot be saved right now.";
}

/** One member of a land stack. */
export interface StackMember {
  /** Full changeset id text, the key. */
  id: string;
  short: string;
  /** Short ids of every parent after the first; non-empty means a merge changeset. */
  otherParents: string[];
  merge: boolean;
  /** Landed with no changeset written because it rebased to nothing under `allow_empty`. */
  empty: boolean;
}

function memberOf(id: ObjectId | undefined, parents: ObjectId[], empty: boolean): StackMember {
  return { id: objectIdText(id), short: objectIdText(id, true), otherParents: parents.slice(1).map((p) => objectIdText(p, true)), merge: parents.length > 1, empty };
}

/**
 * The stack an open change would land: the first-parent chain from `head` through the changesets
 * that are in the head and not in the target (CompareRevisions). A merge member keeps its other
 * parents, whose side branches land with it. Newest first.
 */
export function openStack(head: ObjectId | undefined, changesets: readonly Changeset[]): StackMember[] {
  const byId = new Map(changesets.map((c) => [objectIdText(c.id), c]));
  const out: StackMember[] = [];
  const seen = new Set<string>();
  let at = head ? objectIdText(head) : "";
  while (at && !seen.has(at)) {
    seen.add(at);
    const c = byId.get(at);
    if (!c) break;
    out.push(memberOf(c.id, c.parents, false));
    at = c.parents[0] ? objectIdText(c.parents[0]) : "";
  }
  return out;
}

/**
 * The ref-log entry a change landed in: the one whose landed list names one of the change's ids.
 */
export function landEntryFor(entries: readonly RefLogEntry[], changeIds: readonly ChangeId[]): RefLogEntry | undefined {
  const want = new Set(changeIds.map((c) => changeIdText(c)));
  return entries.find((e) => e.landed.some((l) => l.changeId && want.has(changeIdText(l.changeId))));
}

/**
 * The members of a landed stack, in landing order. A member landed empty when it was not skipped
 * and its result is the running tip it started from: the previous member's result, or the ref's old
 * target for the first one. No changeset was written for it (SPEC §7.4, step 3).
 */
export function landedStack(entry: RefLogEntry, parentsOf: (source: ObjectId | undefined) => ObjectId[]): StackMember[] {
  let running = objectIdText(entry.old);
  const out: StackMember[] = [];
  for (const l of entry.landed) {
    const result = objectIdText(l.result);
    const empty = !l.skipped && result === running;
    out.push(memberOf(l.source, parentsOf(l.source), empty));
    running = result;
  }
  return out;
}

/** The ignorable `git` extension of Meta (SPEC §4.3 key 64): present only on history written by the importer. */
export const GIT_EXTENSION_KEY = 64;

/** Whether a changeset's meta says it was imported: it carries the original git commit headers (SPEC §17.4). */
export function isImported(meta: Meta | undefined): boolean {
  return !!meta?.extensions?.some((x) => x.key === GIT_EXTENSION_KEY);
}

/**
 * The org's importer service principal: imports run as it, so imported history names it
 * (`<org> importer`, kind service) rather than the admin who started the import.
 */
export function isImporterPrincipal(p: { kind: string; display: string } | undefined): boolean {
  return p?.kind === "PRINCIPAL_KIND_SERVICE" && / importer$/.test(p.display);
}

/** How an archive request was answered, for the releases screen. */
export type ArchiveOutcome = { ok: true; url: string; filtered: boolean } | { ok: false; reason: string; message: string };

/** The server's words for the refusals a person can act on. Anything else shows the server's own message. */
export function archiveRefusalText(reason: string, message: string): string {
  if (reason === "POLICY_DENIED") return "You cannot read every path in this revision, so Beehive will not build an archive of it.";
  if (reason === "REDACTED") return "This revision contains redacted content, so it cannot be archived.";
  return message;
}
