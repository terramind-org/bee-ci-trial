/**
 * "Worth a human look" (docs/spec/web-ui.md §3): up to three sensitive or logic hunks, read-only,
 * each with why it was picked. The full diff, with threads and suggestions, is the Code tab's.
 */
import Link from "next/link";
import type { BriefHighlight, DiffItem } from "@/lib/api/types";
import { hunkLines } from "@/lib/diff";
import { worthALook, type PickedHunk } from "@/lib/hunks";
import { repoHref } from "@/lib/routes";
import { cx } from "../ui";

function Hunk({ pick, href }: { pick: PickedHunk; href?: string }) {
  const lines = hunkLines(pick.hunk);
  return (
    <figure className="flex min-w-0 flex-col gap-2" data-hunk={pick.path + "#" + pick.index}>
      <figcaption className="flex flex-col gap-0.5">
        <span className="flex flex-wrap items-baseline gap-x-2">
          {href ? (
            <Link href={href} className="mono text-sm text-ink hover:underline [overflow-wrap:anywhere]">
              {pick.path}
            </Link>
          ) : (
            <span className="mono text-sm text-ink [overflow-wrap:anywhere]">{pick.path}</span>
          )}
          <span className="num text-xs text-ink-3">
            lines {pick.hunk.newStart}–{pick.hunk.newStart + Math.max(pick.hunk.newLen - 1, 0)}
          </span>
        </span>
        <span className={cx("text-sm", pick.diffClass === "sensitive" ? "text-warn" : "text-ink-2")}>
          {pick.reason}
          {pick.highlight ? " The brief says: “" + pick.highlight + "”" : ""}
        </span>
      </figcaption>
      {/* Focusable so a keyboard can scroll long lines (axe scrollable-region-focusable; B9). */}
      <div className="overflow-x-auto rounded-lg bg-sunken py-2 scrollbar-thin" tabIndex={0} role="region" aria-label={"Lines from " + pick.path}>
        <table className="mono w-full border-collapse text-xs leading-5">
          <caption className="sr-only">
            Lines {pick.added} added and {pick.removed} removed in {pick.path}
          </caption>
          <tbody>
            {lines.map((l, i) => (
              <tr key={i} className={l.kind === "add" ? "bg-add" : l.kind === "del" ? "bg-del" : undefined}>
                <td className="w-10 pr-2 pl-3 text-right text-ink-3 select-none">{l.oldNo ?? ""}</td>
                <td className="w-10 pr-2 text-right text-ink-3 select-none">{l.newNo ?? ""}</td>
                <td className="w-4 text-ink-3 select-none" aria-label={l.kind === "add" ? "added" : l.kind === "del" ? "removed" : undefined}>
                  {l.kind === "add" ? "+" : l.kind === "del" ? "−" : ""}
                </td>
                <td className="pr-3 whitespace-pre text-ink">{l.text}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      {pick.truncated ? (
        <p className="text-sm text-ink-3">
          {pick.truncated} more {pick.truncated === 1 ? "line" : "lines"} in this hunk.{" "}
          {href ? (
            <Link href={href} className="text-ink-2 underline hover:text-ink">
              Read it in the Code tab
            </Link>
          ) : null}
        </p>
      ) : null}
    </figure>
  );
}

export function WorthALook({ diff, highlights = [], repo, changeId }: { diff: DiffItem[]; highlights?: BriefHighlight[]; repo?: string; changeId?: string }) {
  const picks = worthALook(diff, { highlights });
  if (!picks.length) return <p className="text-base text-ink-2">Nothing in this revision changes behaviour or touches a sensitive path.</p>;
  const href = (p: PickedHunk) => (repo && changeId ? repoHref(repo, "changes/" + changeId + "/code#file-" + encodeURIComponent(p.path)) : undefined);
  return (
    <div className="flex flex-col gap-8" data-worth-a-look>
      {picks.map((p) => (
        <Hunk key={p.path + "#" + p.index} pick={p} href={href(p)} />
      ))}
    </div>
  );
}
