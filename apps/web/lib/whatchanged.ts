/**
 * "What changed", grouped by area in sentences rather than files (docs/spec/web-ui.md §3):
 *   "Behaviour changes in src/billing (3 files), with tests."
 * An area is a directory near the top of the tree; tests join the area whose code they test when
 * that is evident from their name or folder. Ordered by risk, then by area. Pure.
 */
import type { BriefHighlight, DiffItem } from "./api/types";
import { pathText } from "./api/ids";
import { countChanges } from "./diff";
import { isDiffClass, riskRank } from "./diffclass";

/** The per-path facts this module needs; `fromDiff` builds them from DiffItems. */
export interface ChangedFile {
  path: string;
  diffClass: string;
  added: number;
  removed: number;
  /** "added", "deleted" or "modified". */
  change: "added" | "deleted" | "modified";
  binary: boolean;
}

export function fromDiff(items: DiffItem[]): ChangedFile[] {
  return items.map((d) => {
    const { added, removed } = countChanges(d.hunks);
    return {
      path: pathText(d.path),
      diffClass: d.diffClass,
      added,
      removed,
      change: !d.old ? "added" : !d.new ? "deleted" : "modified",
      binary: d.binary,
    };
  });
}

/** Folders that hold several projects, so their areas are one level deeper. */
const CONTAINERS = new Set(["apps", "packages", "crates", "services", "libs", "modules", "cmd", "internal"]);
const TEST_DIRS = new Set(["test", "tests", "__tests__", "spec", "specs", "e2e"]);
const TEST_FILE = /(\.|_)(test|spec)\.[^./]+$|_test\.go$/;

export function isTestPath(path: string): boolean {
  return TEST_FILE.test(path) || path.split("/").slice(0, -1).some((s) => TEST_DIRS.has(s));
}

/** The area a path belongs to: "src/billing", "apps/web/lib", or "" for the top of the repo. */
export function areaOf(path: string): string {
  const all = path.split("/").slice(0, -1);
  // Test folders are dropped so tests/billing/x.test.ts sits with billing, unless nothing else is left.
  const dirs = all.some((s) => !TEST_DIRS.has(s)) ? all.filter((s) => !TEST_DIRS.has(s)) : all.slice(0, 1);
  if (dirs.length === 0) return "";
  const depth = CONTAINERS.has(dirs[0] ?? "") ? 3 : 2;
  return dirs.slice(0, depth).join("/");
}

function stem(path: string): string {
  const base = path.split("/").pop() ?? path;
  return base.replace(/(\.|_)(test|spec)(?=\.)/, "").replace(/_test(?=\.go$)/, "").replace(/\.[^.]+$/, "");
}

export interface Area {
  /** "" is the top of the repo. */
  area: string;
  /** The highest-risk class among its non-test files (or its tests when it has only tests). */
  diffClass: string;
  /** "Behaviour changes in src/billing (3 files), with tests." */
  sentence: string;
  files: ChangedFile[];
  tests: ChangedFile[];
  added: number;
  removed: number;
  /** Brief highlights whose paths fall in this area, in brief order. */
  highlights: string[];
}

const KIND: Record<string, string> = {
  sensitive: "Sensitive changes",
  logic: "Behaviour changes",
  dependency: "Dependency updates",
  rename_only: "Files moved",
  test_only: "Test changes",
  comment_only: "Comment changes",
  docs: "Documentation changes",
  generated: "Generated files",
};

function placeOf(area: string): string {
  return area ? "in " + area : "at the top of the repo";
}

function sentenceOf(area: string, cls: string, files: ChangedFile[], tests: ChangedFile[]): string {
  const kind = KIND[cls] ?? "Changes";
  const n = files.length || tests.length;
  const count = " (" + n + (n === 1 ? " file" : " files") + ")";
  const added = files.filter((f) => f.change === "added").length;
  const deleted = files.filter((f) => f.change === "deleted").length;
  const extras: string[] = [];
  if (added && added === files.length) extras.push(added === 1 ? "a new file" : "all new");
  else if (deleted && deleted === files.length) extras.push(deleted === 1 ? "a file removed" : "all removed");
  if (files.length && tests.length) extras.push("with tests");
  else if (files.length && (cls === "logic" || cls === "sensitive")) extras.push("without tests");
  return kind + " " + placeOf(area) + count + (extras.length ? ", " + extras.join(", ") : "") + ".";
}

/**
 * Groups changed files into areas. A test file joins the area of a changed code file with the same
 * stem (invoice.ts and invoice.test.ts), else its own area (its folder without the test folder), which
 * holds tests alone when no code there changed.
 */
export function whatChanged(files: ChangedFile[], highlights: BriefHighlight[] = []): Area[] {
  const sorted = [...files].sort((a, b) => (a.path < b.path ? -1 : a.path > b.path ? 1 : 0));
  const code = sorted.filter((f) => !isTestPath(f.path) && f.diffClass !== "test_only");
  const tests = sorted.filter((f) => isTestPath(f.path) || f.diffClass === "test_only");
  const areas = new Map<string, { files: ChangedFile[]; tests: ChangedFile[] }>();
  const slot = (a: string) => {
    let s = areas.get(a);
    if (!s) areas.set(a, (s = { files: [], tests: [] }));
    return s;
  };
  for (const f of code) slot(areaOf(f.path)).files.push(f);
  const byStem = new Map<string, string>();
  for (const f of code) if (!byStem.has(stem(f.path))) byStem.set(stem(f.path), areaOf(f.path));
  for (const t of tests) {
    const own = areaOf(t.path);
    const home = byStem.get(stem(t.path)) ?? own;
    slot(home).tests.push(t);
  }
  const out: Area[] = [];
  for (const [area, { files: fs, tests: ts }] of areas) {
    const classes = (fs.length ? fs : ts).map((f) => f.diffClass);
    let cls = fs.length ? "generated" : "test_only";
    for (const c of classes) if (isDiffClass(c) && riskRank(c) > riskRank(cls)) cls = c;
    const all = [...fs, ...ts];
    const inArea = new Set(all.map((f) => f.path));
    out.push({
      area,
      diffClass: cls,
      sentence: sentenceOf(area, fs.length ? cls : "test_only", fs, ts),
      files: fs,
      tests: ts,
      added: all.reduce((n, f) => n + f.added, 0),
      removed: all.reduce((n, f) => n + f.removed, 0),
      highlights: highlights.filter((h) => h.paths.some((p) => inArea.has(p) || [...inArea].some((q) => q.startsWith(p.endsWith("/") ? p : p + "/")))).map((h) => h.text),
    });
  }
  return out.sort((a, b) => riskRank(b.diffClass) - riskRank(a.diffClass) || (a.area < b.area ? -1 : a.area > b.area ? 1 : 0));
}

/** One line for the whole change: "12 files in 4 areas, +210 −34". */
export function changeSize(files: ChangedFile[]): string {
  const areas = whatChanged(files).length;
  const added = files.reduce((n, f) => n + f.added, 0);
  const removed = files.reduce((n, f) => n + f.removed, 0);
  return files.length + (files.length === 1 ? " file" : " files") + " in " + areas + (areas === 1 ? " area" : " areas") + ", +" + added + " −" + removed;
}
