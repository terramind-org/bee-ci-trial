/**
 * "Worth a human look" (docs/spec/web-ui.md §3): at most three hunks from sensitive or logic files,
 * shown read-only in the Brief tab so a supervisor sees the code that matters without opening the
 * diff. Chosen deterministically: sensitive before logic, then hunks in paths the brief's
 * highlights or risks name, then the most changed lines, then path and position. Tests and
 * generated files are never picked.
 */
import type { BriefHighlight, DiffHunk, DiffItem } from "./api/types";
import { pathText } from "./api/ids";
import { riskRank } from "./diffclass";
import { isTestPath } from "./whatchanged";

export interface PickedHunk {
  path: string;
  diffClass: "sensitive" | "logic";
  /** Index of the hunk in its file, so the Code tab can link to it. */
  index: number;
  hunk: DiffHunk;
  added: number;
  removed: number;
  /** Why it was picked, in words: "Touches a path the policy marks sensitive." */
  reason: string;
  /** The brief highlight that names this path, when one does. */
  highlight?: string;
  /** Lines beyond `maxLines` that were cut from the hunk's end. */
  truncated: number;
}

export interface PickOptions {
  max?: number;
  /** Longest hunk shown, in lines; the rest is cut and counted in `truncated`. */
  maxLines?: number;
  highlights?: BriefHighlight[];
}

function covers(prefix: string, path: string): boolean {
  return path === prefix || path.startsWith(prefix.endsWith("/") ? prefix : prefix + "/");
}

export function worthALook(items: DiffItem[], { max = 3, maxLines = 40, highlights = [] }: PickOptions = {}): PickedHunk[] {
  const candidates: (PickedHunk & { size: number; named: boolean })[] = [];
  for (const item of items) {
    const cls = item.diffClass;
    if ((cls !== "sensitive" && cls !== "logic") || item.binary) continue;
    const path = pathText(item.path);
    if (isTestPath(path)) continue;
    const highlight = highlights.find((h) => h.paths.some((p) => covers(p, path)))?.text;
    item.hunks.forEach((hunk, index) => {
      const added = hunk.lines.filter((l) => l[0] === "+").length;
      const removed = hunk.lines.filter((l) => l[0] === "-").length;
      if (!added && !removed) return;
      const cut = Math.max(0, hunk.lines.length - maxLines);
      candidates.push({
        path,
        diffClass: cls,
        index,
        hunk: cut ? { ...hunk, lines: hunk.lines.slice(0, maxLines) } : hunk,
        added,
        removed,
        reason: cls === "sensitive" ? "Touches a path the policy marks sensitive." : "Changes how the program behaves.",
        highlight,
        truncated: cut,
        size: added + removed,
        named: !!highlight,
      });
    });
  }
  candidates.sort(
    (a, b) =>
      riskRank(b.diffClass) - riskRank(a.diffClass) ||
      Number(b.named) - Number(a.named) ||
      b.size - a.size ||
      (a.path < b.path ? -1 : a.path > b.path ? 1 : 0) ||
      a.index - b.index,
  );
  // Prefer breadth: one hunk per file first, then fill with second hunks.
  const picked: typeof candidates = [];
  const seen = new Set<string>();
  for (const c of candidates) {
    if (picked.length >= max || seen.has(c.path)) continue;
    seen.add(c.path);
    picked.push(c);
  }
  for (const c of candidates) if (picked.length < max && !picked.includes(c)) picked.push(c);
  return picked.map((c) => ({ path: c.path, diffClass: c.diffClass, index: c.index, hunk: c.hunk, added: c.added, removed: c.removed, reason: c.reason, highlight: c.highlight, truncated: c.truncated }));
}
