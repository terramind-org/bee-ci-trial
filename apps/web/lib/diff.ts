/**
 * Small, deterministic line diff for the fixture adapter and the diff viewer.
 * The real server computes diffs (QueryService.Diff); this only has to be correct and stable.
 */
import type { DiffHunk } from "./api/types";

export type Op = { kind: "eq" | "del" | "add"; line: string; oldNo: number; newNo: number };

export function splitLines(text: string): string[] {
  if (text === "") return [];
  const lines = text.split("\n");
  if (lines[lines.length - 1] === "") lines.pop();
  return lines;
}

/** LCS-based edit script. Ties prefer deletions before additions, so output is stable. */
export function diffLines(a: string[], b: string[]): Op[] {
  const n = a.length;
  const m = b.length;
  // Trim common prefix and suffix to keep the table small.
  let pre = 0;
  while (pre < n && pre < m && a[pre] === b[pre]) pre++;
  let suf = 0;
  while (suf < n - pre && suf < m - pre && a[n - 1 - suf] === b[m - 1 - suf]) suf++;
  const A = a.slice(pre, n - suf);
  const B = b.slice(pre, m - suf);
  const rows = A.length + 1;
  const cols = B.length + 1;
  const t = new Uint32Array(rows * cols);
  for (let i = A.length - 1; i >= 0; i--) {
    for (let j = B.length - 1; j >= 0; j--) {
      t[i * cols + j] =
        A[i] === B[j] ? (t[(i + 1) * cols + j + 1] ?? 0) + 1 : Math.max(t[(i + 1) * cols + j] ?? 0, t[i * cols + j + 1] ?? 0);
    }
  }
  const ops: Op[] = [];
  let oldNo = 0;
  let newNo = 0;
  const push = (kind: Op["kind"], line: string) => {
    if (kind !== "add") oldNo++;
    if (kind !== "del") newNo++;
    ops.push({ kind, line, oldNo: kind === "add" ? 0 : oldNo, newNo: kind === "del" ? 0 : newNo });
  };
  for (let k = 0; k < pre; k++) push("eq", a[k] ?? "");
  let i = 0;
  let j = 0;
  while (i < A.length || j < B.length) {
    if (i < A.length && j < B.length && A[i] === B[j]) {
      push("eq", A[i] ?? "");
      i++;
      j++;
    } else if (j >= B.length || (i < A.length && (t[(i + 1) * cols + j] ?? 0) >= (t[i * cols + j + 1] ?? 0))) {
      push("del", A[i] ?? "");
      i++;
    } else {
      push("add", B[j] ?? "");
      j++;
    }
  }
  for (let k = n - suf; k < n; k++) push("eq", a[k] ?? "");
  return ops;
}

/** Groups an edit script into hunks with `context` lines around each change. */
export function toHunks(ops: Op[], context = 3): DiffHunk[] {
  const changed = ops.map((o, idx) => (o.kind === "eq" ? -1 : idx)).filter((x) => x >= 0);
  if (changed.length === 0) return [];
  const ranges: [number, number][] = [];
  for (const idx of changed) {
    const lo = Math.max(0, idx - context);
    const hi = Math.min(ops.length - 1, idx + context);
    const last = ranges[ranges.length - 1];
    if (last && lo <= last[1] + 1) last[1] = Math.max(last[1], hi);
    else ranges.push([lo, hi]);
  }
  return ranges.map(([lo, hi]) => {
    const slice = ops.slice(lo, hi + 1);
    let oldStart = 0;
    let newStart = 0;
    // Line numbers of the first line on each side, or the line before for empty sides.
    let oldBefore = 0;
    let newBefore = 0;
    for (let k = 0; k < lo; k++) {
      const o = ops[k];
      if (o && o.kind !== "add") oldBefore = o.oldNo;
      if (o && o.kind !== "del") newBefore = o.newNo;
    }
    const oldLen = slice.filter((o) => o.kind !== "add").length;
    const newLen = slice.filter((o) => o.kind !== "del").length;
    oldStart = oldLen ? oldBefore + 1 : oldBefore;
    newStart = newLen ? newBefore + 1 : newBefore;
    return {
      oldStart,
      oldLen,
      newStart,
      newLen,
      lines: slice.map((o) => (o.kind === "eq" ? " " : o.kind === "del" ? "-" : "+") + o.line),
    };
  });
}

export function diffText(oldText: string, newText: string, context = 3): DiffHunk[] {
  return toHunks(diffLines(splitLines(oldText), splitLines(newText)), context);
}

export interface HunkLine {
  kind: "eq" | "del" | "add";
  text: string;
  oldNo: number | null;
  newNo: number | null;
}

/** Expands a hunk into numbered lines. */
export function hunkLines(h: DiffHunk): HunkLine[] {
  let o = h.oldStart;
  let n = h.newStart;
  return h.lines.map((raw) => {
    const sign = raw[0];
    const text = raw.slice(1);
    if (sign === "-") return { kind: "del", text, oldNo: o++, newNo: null };
    if (sign === "+") return { kind: "add", text, oldNo: null, newNo: n++ };
    return { kind: "eq", text, oldNo: o++, newNo: n++ };
  });
}

export interface SplitRow {
  left: HunkLine | null;
  right: HunkLine | null;
}

/** Pairs deletions with following additions for side-by-side view. */
export function toSplitRows(lines: HunkLine[]): SplitRow[] {
  const rows: SplitRow[] = [];
  let i = 0;
  while (i < lines.length) {
    const l = lines[i];
    if (!l) break;
    if (l.kind === "eq") {
      rows.push({ left: l, right: l });
      i++;
      continue;
    }
    const dels: HunkLine[] = [];
    const adds: HunkLine[] = [];
    while (lines[i]?.kind === "del") dels.push(lines[i++] as HunkLine);
    while (lines[i]?.kind === "add") adds.push(lines[i++] as HunkLine);
    for (let k = 0; k < Math.max(dels.length, adds.length); k++) rows.push({ left: dels[k] ?? null, right: adds[k] ?? null });
  }
  return rows;
}

export function countChanges(hunks: DiffHunk[]): { added: number; removed: number } {
  let added = 0;
  let removed = 0;
  for (const h of hunks)
    for (const l of h.lines) {
      if (l[0] === "+") added++;
      else if (l[0] === "-") removed++;
    }
  return { added, removed };
}

/**
 * Line-origin blame across a linear series of versions. Returns, for each line of the last
 * version, the index of the version that introduced it.
 */
export function blameOrigins(versions: string[]): number[] {
  let origins: number[] = [];
  let prev: string[] = [];
  versions.forEach((text, v) => {
    const cur = splitLines(text);
    const ops = diffLines(prev, cur);
    const next: number[] = [];
    for (const op of ops) {
      if (op.kind === "eq") next.push(origins[op.oldNo - 1] ?? v);
      else if (op.kind === "add") next.push(v);
    }
    origins = next;
    prev = cur;
  });
  return origins;
}
