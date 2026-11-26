/**
 * A deliberately small Markdown renderer for bodies, notes and READMEs: headings, paragraphs,
 * lists, fenced code, tables, inline code, emphasis, links and evidence images. Everything is
 * escaped first and links are restricted to http(s), relative paths and anchors, so output is safe
 * by construction. The only image it ever loads is `![alt](evidence:<id>)` for an evidence item of
 * the change being shown, through the same-origin evidence route (docs/spec/web-ui.md §4); any
 * other image renders as a link, and an id that does not resolve renders its alt text.
 * Rich rendering (Mermaid, math, notebooks) is a later task (PRODUCT §4.24).
 */
import type { EvidenceIndex } from "@/lib/evidence-view";
import { escapeHtml } from "@/lib/highlight";
import { EVIDENCE_ROUTE } from "@/lib/media/evidence-url";
import { repoHref } from "@/lib/routes";

/** Same-origin evidence media only; anything else in the index is ignored. */
function evidenceImage(id: string, alt: string, evidence: EvidenceIndex | undefined): string {
  const hit = evidence?.get(id);
  if (!hit || !hit.url.startsWith(EVIDENCE_ROUTE)) return alt;
  return '<img src="' + escapeHtml(hit.url) + '" alt="' + (alt || escapeHtml(hit.alt)) + '" loading="lazy" decoding="async" class="block max-w-full rounded-lg bg-sunken" data-evidence-image="' + escapeHtml(id) + '">';
}

const SAFE_HREF = /^(https?:\/\/|\/|#|\.\/)/;

/**
 * `repo` (owner/name) makes `#12` link to that repo's issue; without it, the root-level path.
 * `evidence` resolves `evidence:<id>` images.
 */
function inline(s: string, repo?: string, evidence?: EvidenceIndex): string {
  // NUL marks held spans below, so it may never come from the source.
  let out = escapeHtml(s.replace(/\u0000/g, ""));
  // Code spans and images are rendered first and held aside, so later rules (emphasis, links,
  // issue numbers) never rewrite their contents or their URLs.
  const held: string[] = [];
  const hold = (html: string) => "\u0000" + (held.push(html) - 1) + "\u0000";
  out = out.replace(/`([^`]+)`/g, (_, c: string) => hold("<code>" + c + "</code>"));
  // Images before links: `![alt](src)` would otherwise read as "!" and a link.
  out = out.replace(/!\[([^\]]*)\]\(([^)\s]+)\)/g, (_, alt: string, src: string) => {
    if (src.startsWith("evidence:")) return hold(evidenceImage(src.slice(9), alt, evidence));
    const safe = SAFE_HREF.test(src) && !/^javascript:/i.test(src) ? src : "#";
    return hold('<a href="' + safe + '"' + (safe.startsWith("http") ? ' rel="noreferrer noopener" target="_blank"' : "") + ">" + (alt || "image") + "</a>");
  });
  out = out.replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>");
  out = out.replace(/(^|[\s(])_([^_]+)_(?=$|[\s).,])/g, "$1<em>$2</em>");
  out = out.replace(/\[([^\]]+)\]\(([^)\s]+)\)/g, (_, text: string, href: string) => {
    const safe = SAFE_HREF.test(href) && !/^javascript:/i.test(href) ? href : "#";
    return '<a href="' + safe + '"' + (safe.startsWith("http") ? ' rel="noreferrer noopener" target="_blank"' : "") + ">" + text + "</a>";
  });
  const issues = repo ? repoHref(repo, "work") : "/work";
  out = out.replace(/(^|\s)#(\d+)\b/g, '$1<a href="' + escapeHtml(issues) + '/$2">#$2</a>');
  return out.replace(/\u0000(\d+)\u0000/g, (_, i: string) => held[Number(i)] ?? "");
}

export function renderMarkdown(src: string, repo?: string, evidence?: EvidenceIndex, headingOffset?: number): string {
  const lines = src.replace(/\r\n/g, "\n").split("\n");
  const out: string[] = [];
  const startsTable = (i: number) => /^\|.*\|\s*$/.test(lines[i] ?? "") && /^\|[\s:|-]+\|\s*$/.test(lines[i + 1] ?? "");
  let i = 0;
  let previousHeading = headingOffset ?? 1;
  while (i < lines.length) {
    const line = lines[i] ?? "";
    if (/^```/.test(line)) {
      const body: string[] = [];
      i++;
      while (i < lines.length && !/^```/.test(lines[i] ?? "")) body.push(lines[i++] ?? "");
      i++;
      out.push("<pre><code>" + escapeHtml(body.join("\n")) + "</code></pre>");
      continue;
    }
    const h = /^(#{1,3})\s+(.*)$/.exec(line);
    if (h) {
      const requestedLevel = (h[1] ?? "#").length + (headingOffset ?? 1);
      // Embedded Markdown starts below its containing section and cannot skip a heading level.
      const level = Math.min(6, requestedLevel, headingOffset === undefined ? 6 : previousHeading + 1);
      previousHeading = level;
      out.push("<h" + level + ">" + inline(h[2] ?? "", repo, evidence) + "</h" + level + ">");
      i++;
      continue;
    }
    if (startsTable(i)) {
      const cells = (l: string) => l.trim().replace(/^\||\|$/g, "").split("|").map((c) => c.trim());
      const head = cells(line);
      i += 2;
      const rows: string[][] = [];
      while (i < lines.length && /^\|.*\|\s*$/.test(lines[i] ?? "")) rows.push(cells(lines[i++] ?? ""));
      out.push(
        "<table><thead><tr>" + head.map((c) => "<th>" + inline(c, repo, evidence) + "</th>").join("") + "</tr></thead><tbody>" + rows.map((r) => "<tr>" + r.map((c) => "<td>" + inline(c, repo, evidence) + "</td>").join("") + "</tr>").join("") + "</tbody></table>",
      );
      continue;
    }
    if (/^\s*[-*]\s+/.test(line) || /^\s*\d+\.\s+/.test(line)) {
      const ordered = /^\s*\d+\./.test(line);
      const items: string[] = [];
      while (i < lines.length && (/^\s*([-*]|\d+\.)\s+/.test(lines[i] ?? "") || (/^\s{2,}\S/.test(lines[i] ?? "") && items.length))) {
        const l = lines[i++] ?? "";
        if (/^\s*([-*]|\d+\.)\s+/.test(l)) items.push(l.replace(/^\s*([-*]|\d+\.)\s+/, ""));
        else items[items.length - 1] += " " + l.trim();
      }
      const tag = ordered ? "ol" : "ul";
      out.push("<" + tag + ">" + items.map((t) => "<li>" + inline(t, repo, evidence) + "</li>").join("") + "</" + tag + ">");
      continue;
    }
    if (line.trim() === "") {
      i++;
      continue;
    }
    // A literal pipe is prose unless it starts a complete table header. Always consume
    // the current line before looking for the next block.
    const para = [line];
    i++;
    while (i < lines.length && (lines[i] ?? "").trim() !== "" && !/^(```|#{1,3}\s|\s*[-*]\s|\s*\d+\.\s)/.test(lines[i] ?? "") && !startsTable(i)) para.push(lines[i++] ?? "");
    out.push("<p>" + inline(para.join(" "), repo, evidence) + "</p>");
  }
  return out.join("\n");
}

/** `evidence` is the change's evidence (`evidenceIndex` in lib/evidence-view.ts), for `evidence:` images. */
export function Markdown({ source, className, repo, evidence, headingOffset }: { source: string; className?: string; repo?: string; evidence?: EvidenceIndex; headingOffset?: number }) {
  if (!source.trim()) return <p className="text-ink-3 italic">No description.</p>;
  return <div className={"prose-bee " + (className ?? "")} dangerouslySetInnerHTML={{ __html: renderMarkdown(source, repo, evidence, headingOffset) }} />;
}
