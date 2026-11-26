/**
 * The brief's asks and their answers (SPEC §11.9, docs/spec/web-ui.md §5): an answer is an ordinary
 * conversation comment `Decision on "<question>": <option>`, so agents read decisions where they
 * read every comment. This module writes that comment, reads it back, and works out which asks are
 * still open. Pure: comments are taken in the order given (oldest first, as ListComments returns).
 */
import type { BriefAsk, Comment, Principal } from "./api/types";

/** The comment body that answers `question` with `option`. */
export function decisionComment(question: string, option: string): string {
  return 'Decision on "' + oneLine(question) + '": ' + oneLine(option);
}

function oneLine(s: string): string {
  return s.replace(/\s+/g, " ").trim();
}

export interface Decision {
  question: string;
  option: string;
}

/**
 * Reads a decision from the first line of a comment. The question is matched greedily up to the
 * last `": `, so a question that itself contains quotes still parses; curly quotes are accepted
 * too, because people type them.
 */
export function parseDecision(body: string): Decision | null {
  const first = (body.split("\n")[0] ?? "").trim();
  const m = /^Decision on ["“](.+)["”]: (.+)$/.exec(first);
  if (!m) return null;
  return { question: oneLine(m[1] ?? ""), option: oneLine(m[2] ?? "") };
}

export interface AskState {
  ask: BriefAsk;
  /** The option chosen by the most recent decision comment, if any. */
  answer?: string;
  /** Who answered, and when. */
  by?: Principal;
  at?: string;
  /** The latest answer named an option the ask does not offer (the ask was edited since). */
  offList: boolean;
}

const norm = (s: string) => oneLine(s).toLowerCase();

/**
 * Each ask with its latest answer. Questions and options compare case-insensitively and with
 * whitespace collapsed; a later decision replaces an earlier one.
 */
export function askStates(asks: BriefAsk[], comments: Comment[]): AskState[] {
  const latest = new Map<string, { option: string; by?: Principal; at?: string }>();
  for (const c of comments) {
    const d = parseDecision(c.body);
    if (d) latest.set(norm(d.question), { option: d.option, by: c.author, at: c.createdAt });
  }
  return asks.map((ask) => {
    const hit = latest.get(norm(ask.question));
    if (!hit) return { ask, offList: false };
    const match = ask.options.find((o) => norm(o) === norm(hit.option));
    return { ask, answer: match ?? hit.option, by: hit.by, at: hit.at, offList: !match };
  });
}

/** The asks nobody has answered yet. */
export function openAsks(asks: BriefAsk[], comments: Comment[]): BriefAsk[] {
  return askStates(asks, comments)
    .filter((s) => s.answer === undefined)
    .map((s) => s.ask);
}
