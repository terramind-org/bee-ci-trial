/**
 * Plain words for issues (docs/spec/web-ui.md §3, "Issues"): who is working on one, and the row
 * as a line of text. An issue is how a person hands work to agents, so the first thing a row
 * says after its title is who has it. Pure functions of what the pages load.
 */
import type { ChangeRequest, Issue, Principal } from "./api/types";
import { answerableFor } from "./pulls";

export interface IssueWork {
  /** "billing-fixer is working on it", "Lin Zhou has it", "Nobody has it yet". */
  text: string;
  /** The person who answers for the work: the assigned person, or the assigned agent's owner. */
  person?: Principal;
  /** True when the text already names that person (a person has it themselves). */
  personIsAssignee: boolean;
  taken: boolean;
}

const name = (p: Principal) => p.display || p.id;

function names(people: Principal[]): string {
  const all = people.map(name);
  return all.length <= 1 ? (all[0] ?? "") : all.slice(0, -1).join(", ") + " and " + all[all.length - 1];
}

/**
 * Who is working on an issue. Agents come first: assigning one starts its work, and the person
 * it answers to is named beside it. A closed issue says it is done rather than who had it.
 */
export function issueWork(issue: Pick<Issue, "assignees" | "state">, directory: Map<string, Principal> = new Map()): IssueWork {
  const agents = issue.assignees.filter((a) => a.kind === "PRINCIPAL_KIND_AGENT");
  const others = issue.assignees.filter((a) => a.kind !== "PRINCIPAL_KIND_AGENT");
  const done = issue.state === "closed";
  if (agents.length) {
    const person = answerableFor(agents[0], directory) ?? undefined;
    return { text: names(agents) + (done ? " worked on it" : agents.length === 1 ? " is working on it" : " are working on it"), person, personIsAssignee: false, taken: true };
  }
  if (others.length) {
    const person = others.find((a) => a.kind === "PRINCIPAL_KIND_HUMAN");
    return { text: names(others) + (done ? " had it" : others.length === 1 ? " has it" : " have it"), person, personIsAssignee: !!person, taken: true };
  }
  return { text: done ? "Closed with nobody assigned" : "Nobody has it yet", personIsAssignee: false, taken: false };
}

/** The pull requests an issue links to, in the order the issue lists them; unknown ids are dropped. */
export function linkedPulls(issue: Pick<Issue, "linkedChanges">, changes: ChangeRequest[]): ChangeRequest[] {
  const by = new Map(changes.map((c) => [c.id, c]));
  return issue.linkedChanges.map((id) => by.get(id)).filter((c): c is ChangeRequest => !!c);
}

export function issueWorkText(work: IssueWork): string {
  return work.text + (work.person && !work.personIsAssignee ? " for " + name(work.person) : "");
}

const PULL_STATE: Record<string, string> = { CHANGE_STATE_OPEN: "open", CHANGE_STATE_LANDED: "merged", CHANGE_STATE_CLOSED: "closed" };

/** "pull request #105 (open)", "pull requests #105 (open) and #102 (merged)". Empty with none. */
export function pullsText(pulls: Pick<ChangeRequest, "number" | "state">[]): string {
  if (!pulls.length) return "";
  const items = pulls.map((c) => "#" + c.number + " (" + (PULL_STATE[c.state] ?? "open") + ")");
  return (items.length === 1 ? "pull request " : "pull requests ") + (items.length <= 1 ? items[0] : items.slice(0, -1).join(", ") + " and " + items[items.length - 1]);
}

/** An issue as one line of text, for the agent's view. */
export function issueLine(issue: Issue, directory: Map<string, Principal>, changes: ChangeRequest[]): string {
  const work = issueWork(issue, directory);
  const pulls = pullsText(linkedPulls(issue, changes));
  return ["#" + issue.number, issue.title, issue.state, issueWorkText(work), issue.author ? "asked by " + name(issue.author) : "", pulls, issue.type, issue.labels.join(", "), issue.subIssuesTotal ? issue.subIssuesDone + " of " + issue.subIssuesTotal + " sub-issues done" : ""].filter(Boolean).join(" · ");
}

export const ISSUE_TITLE_LIMIT = 300;
const QUICK_ISSUE_LIMIT = 120;

export interface IssueDraft {
  title: string;
  body: string;
}

export function needsIssueDetail(text: string): boolean {
  return text.trim().length > QUICK_ISSUE_LIMIT || /[\r\n]/.test(text);
}

/** Keep the original request when a paste needs more room than a title. */
export function expandIssueDraft(text: string): IssueDraft {
  const draft = text.trim();
  const newline = draft.search(/[\r\n]/);
  const first = newline === -1 ? draft : draft.slice(0, newline).trim();
  if (first.length > QUICK_ISSUE_LIMIT) {
    const space = first.lastIndexOf(" ", QUICK_ISSUE_LIMIT);
    return { title: first.slice(0, space > 0 ? space : QUICK_ISSUE_LIMIT), body: draft };
  }
  return { title: first, body: newline === -1 ? "" : draft.slice(newline).trimStart() };
}

export function issueLabels(text: string): string[] {
  return [...new Set(text.split(",").map((label) => label.trim()).filter(Boolean))];
}
