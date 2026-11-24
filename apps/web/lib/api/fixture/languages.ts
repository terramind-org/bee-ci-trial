/**
 * Language statistics for a tree: bytes by file extension, the way the repo home and the
 * language bar read them. Pure function of the files.
 */
import type { LanguageStatJson } from "../../gen/bee/v1/insights_pb";

type Kind = "LANGUAGE_TYPE_PROGRAMMING" | "LANGUAGE_TYPE_MARKUP" | "LANGUAGE_TYPE_DATA" | "LANGUAGE_TYPE_PROSE";

interface Lang {
  name: string;
  color: string;
  type: Kind;
}

const P: Kind = "LANGUAGE_TYPE_PROGRAMMING";
const M: Kind = "LANGUAGE_TYPE_MARKUP";
const D: Kind = "LANGUAGE_TYPE_DATA";
const T: Kind = "LANGUAGE_TYPE_PROSE";

const TS: Lang = { name: "TypeScript", color: "#3178c6", type: P };
const JS: Lang = { name: "JavaScript", color: "#f1e05a", type: P };
const BY_EXT: Record<string, Lang> = {
  ts: TS,
  tsx: TS,
  js: JS,
  mjs: JS,
  rs: { name: "Rust", color: "#dea584", type: P },
  py: { name: "Python", color: "#3572a5", type: P },
  go: { name: "Go", color: "#00add8", type: P },
  sh: { name: "Shell", color: "#89e051", type: P },
  tf: { name: "HCL", color: "#844fba", type: P },
  css: { name: "CSS", color: "#563d7c", type: M },
  html: { name: "HTML", color: "#e34c26", type: M },
  json: { name: "JSON", color: "#292929", type: D },
  toml: { name: "TOML", color: "#9c4221", type: D },
  yml: { name: "YAML", color: "#cb171e", type: D },
  yaml: { name: "YAML", color: "#cb171e", type: D },
  md: { name: "Markdown", color: "#083fa1", type: T },
};

export interface LanguageOptions {
  /** Count prose (Markdown) too. */
  includeDocumentation?: boolean;
  /** Count data formats (JSON, YAML, TOML) too. */
  includeGenerated?: boolean;
}

export interface LanguageSummary {
  languages: LanguageStatJson[];
  totalBytes: number;
  /** The biggest programming language, "" when there is none. */
  primary: string;
}

const LOCKFILES = new Set(["bun.lock", "package-lock.json", "Cargo.lock"]);
const encoder = new TextEncoder();

export function languageStats(files: ReadonlyMap<string, string>, opts: LanguageOptions = {}): LanguageSummary {
  const acc = new Map<string, { lang: Lang; bytes: number; files: number }>();
  for (const [path, text] of files) {
    if (path.startsWith(".bee/") || path.startsWith(".github/") || LOCKFILES.has(path.split("/").pop() ?? "")) continue;
    const ext = path.includes(".") ? (path.split(".").pop() ?? "").toLowerCase() : "";
    const lang = BY_EXT[ext];
    if (!lang) continue;
    if (lang.type === T && !opts.includeDocumentation) continue;
    if (lang.type === D && !opts.includeGenerated) continue;
    const row = acc.get(lang.name) ?? { lang, bytes: 0, files: 0 };
    row.bytes += encoder.encode(text).length;
    row.files++;
    acc.set(lang.name, row);
  }
  const rows = [...acc.values()].sort((a, b) => b.bytes - a.bytes || a.lang.name.localeCompare(b.lang.name));
  const total = rows.reduce((n, r) => n + r.bytes, 0);
  // One decimal each; the largest absorbs the rounding so the bar sums to exactly 100.
  const pct = rows.map((r) => Math.round((r.bytes * 1000) / total) / 10);
  if (rows.length) pct[0] = Math.round((pct[0] ?? 0) * 10 + (1000 - pct.reduce((n, p) => n + Math.round(p * 10), 0))) / 10;
  const languages = rows.map((r, i) => ({ name: r.lang.name, byteCount: String(r.bytes), fileCount: r.files, percentage: pct[i] ?? 0, color: r.lang.color, type: r.lang.type }));
  return { languages, totalBytes: total, primary: rows.find((r) => r.lang.type === P)?.lang.name ?? rows[0]?.lang.name ?? "" };
}
