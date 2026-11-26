/**
 * Plain words and small lookups shared by the screens that show pull requests: Needs you, the
 * pull-request list and the pull-request page (docs/spec/web-ui.md §3). Pure functions of what
 * the pages already load; nothing here reads a clock or the network.
 */
import { base64ToBytes, hex, objectIdText } from "./api/ids";
import type { ObjectId, Principal, Ref } from "./api/types";
import { countWord, listing } from "./checkwords";
import { missingInWords, parseApproval } from "./status";

/**
 * The branch a pull request comes from: the ref, other than its target, that points at its
 * head. A pull request does not record its branch, so this is read from the repository's refs;
 * empty when no ref points there (the branch was deleted, or the refs could not be listed).
 */
export function branchOf(refs: Pick<Ref, "name" | "target">[], head: ObjectId | undefined, targetRef: string): string {
  const at = objectIdText(head);
  if (!at) return "";
  const names = refs
    .filter((r) => r.name !== targetRef && objectIdText(r.target) === at)
    .map((r) => r.name)
    .sort();
  return names[0] ?? "";
}

/** People and agents by id, from whatever lists a page has: the first entry for an id wins. */
export function directoryOf(...lists: (Principal | undefined | null)[][]): Map<string, Principal> {
  const out = new Map<string, Principal>();
  for (const list of lists) for (const p of list) if (p?.id && !out.has(p.id)) out.set(p.id, p);
  return out;
}

/**
 * The person who answers for a piece of work: a person answers for their own, and an agent's
 * owner answers for the agent. Null when the owner is not in the directory, so a screen leaves
 * the line out instead of printing an id.
 */
export function answerableFor(author: Principal | undefined, directory: Map<string, Principal>): Principal | null {
  if (!author) return null;
  if (author.kind === "PRINCIPAL_KIND_HUMAN") return author;
  if (author.kind !== "PRINCIPAL_KIND_AGENT" || !author.ownerId) return null;
  const owner = directory.get(author.ownerId);
  return owner?.kind === "PRINCIPAL_KIND_HUMAN" ? owner : null;
}

const CLASS_SUBJECT: Record<string, string> = {
  sensitive: "A change to a path the rules mark sensitive",
  logic: "A change in how the code behaves",
  dependency: "A change to the dependencies",
  docs: "A change to the documentation",
  test_only: "A change to the tests",
  generated: "A change to generated files",
  rename_only: "A change that moves files",
  comment_only: "A change to comments",
  default: "An agent's change of a kind agents may not merge on their own",
};

const approvals = (n: number) => countWord(n) + (n === 1 ? " approval" : " approvals");

/**
 * Why a pull request was sent to a person: the approval rules among `LandingEvaluation.missing`,
 * each as one sentence. Other entries (a check, a lease, a stack parent) are not about a person
 * and are returned by `otherHolds`. `owners` names the owners of a path, when the page knows them.
 */
export function whyPerson(missing: string[], owners: Record<string, string[]> = {}): string[] {
  const out: string[] = [];
  for (const entry of missing) {
    const a = parseApproval(entry);
    const bare = a ? null : /^approval:(human|any|owner)(?::(\S+))?$/.exec(entry.trim());
    if (!a && !bare) continue;
    const from = a?.from ?? (bare?.[1] as "human" | "any" | "owner");
    const scope = a?.scope ?? bare?.[2] ?? "";
    const need = a?.need ?? 1;
    const have = a?.have ?? 0;
    const has = have > 0 ? " It has " + countWord(have) + "." : "";
    if (from === "owner") {
      const who = owners[scope]?.length ? listing(owners[scope] as string[]) : "an owner of that path";
      out.push((scope ? "It touches " + scope + ", which needs " : "It needs ") + approvals(need) + " from " + who + "." + has);
    } else if (from === "human") {
      out.push((CLASS_SUBJECT[scope] ?? "This change") + " needs " + approvals(need) + " from a person." + has);
    } else {
      out.push("It needs " + approvals(need) + ", from a person or another agent." + has);
    }
  }
  return out;
}

/** What else stands in the way, besides a person: checks, leases, a stack parent. One sentence each. */
export function otherHolds(missing: string[]): string[] {
  return missing.filter((m) => !/^approval:/.test(m.trim())).map((m) => "It " + missingInWords(m) + ".");
}

/** "wants to merge 3 commits into", or without the count when the server cannot say. */
export function mergeVerb(state: string, commits: number | undefined): string {
  const n = commits ? " " + countWord(commits) + (commits === 1 ? " commit" : " commits") : "";
  if (state === "CHANGE_STATE_LANDED") return "merged" + n + " into";
  if (state === "CHANGE_STATE_CLOSED") return "wanted to merge" + n + " into";
  return "wants to merge" + n + " into";
}

/**
 * `Provenance.promptDigest` as text. The wire carries bytes (base64 in JSON): raw digest bytes
 * are shown as hex, and bytes that already spell a hex string are shown as that string. Empty
 * when there is no digest or it cannot be decoded, so the screen leaves the line out.
 */
export function digestText(bytes: string | undefined): string {
  if (!bytes) return "";
  try {
    const raw = base64ToBytes(bytes);
    if (!raw.length) return "";
    const text = String.fromCharCode(...raw);
    return /^[0-9a-f]+$/i.test(text) ? text.toLowerCase() : hex(raw);
  } catch {
    return "";
  }
}
