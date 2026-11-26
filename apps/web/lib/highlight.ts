/**
 * Syntax highlighting with sugar-high (about 1 kB, escapes its input). Returns one HTML string
 * per line so viewers can number, anchor and diff lines. Unknown languages and very large files
 * are escaped plain text.
 */
import { highlight } from "sugar-high";

const CODE = /\.(ts|tsx|js|jsx|mjs|cjs|json|rs|go|java|kt|swift|c|h|cc|cpp|cs|css|scss|tf|proto|sh|py|rb|php|toml|ya?ml)$/i;
const LIMIT = 256 * 1024;

export function escapeHtml(s: string): string {
  return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");
}

export function isCode(path: string): boolean {
  return CODE.test(path);
}

export function highlightLines(text: string, path: string): string[] {
  const lines = text.split("\n");
  if (!isCode(path) || text.length > LIMIT) return lines.map(escapeHtml);
  const html = highlight(text).split("\n");
  // sugar-high emits exactly one line span per input line; fall back if that ever changes.
  return html.length === lines.length ? html : lines.map(escapeHtml);
}

export function languageOf(path: string): string {
  const ext = /\.([a-z0-9]+)$/i.exec(path)?.[1]?.toLowerCase() ?? "";
  const map: Record<string, string> = { ts: "TypeScript", tsx: "TypeScript JSX", js: "JavaScript", json: "JSON", md: "Markdown", toml: "TOML", yml: "YAML", yaml: "YAML", tf: "Terraform", rs: "Rust", go: "Go", py: "Python", css: "CSS", lock: "Lockfile" };
  return map[ext] ?? (ext ? ext.toUpperCase() : "Text");
}
