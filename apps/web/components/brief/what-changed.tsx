/**
 * "What changed" (docs/spec/web-ui.md §3): areas in sentences, riskiest first. Each area opens to
 * its files, which link to the Code tab; the brief's highlights for that area sit under it.
 */
import Link from "next/link";
import type { BriefHighlight, DiffItem } from "@/lib/api/types";
import { classInfo } from "@/lib/diffclass";
import { repoHref } from "@/lib/routes";
import { changeSize, fromDiff, whatChanged, type ChangedFile } from "@/lib/whatchanged";
import { Icon } from "../icons";

function FileRow({ f, href }: { f: ChangedFile; href?: string }) {
  const path = <span className="mono min-w-0 truncate text-sm text-ink [overflow-wrap:anywhere]">{f.path}</span>;
  return (
    <li className="flex items-center gap-3 py-1">
      {href ? (
        <Link href={href} className="flex min-w-0 flex-1 items-center gap-2 hover:underline">
          {path}
        </Link>
      ) : (
        <span className="flex min-w-0 flex-1">{path}</span>
      )}
      <span className="shrink-0 text-xs text-ink-3">{f.change === "modified" ? classInfo(f.diffClass).label : f.change === "added" ? "New" : "Removed"}</span>
      {f.binary ? (
        <span className="w-20 shrink-0 text-right text-xs text-ink-3">binary</span>
      ) : (
        <span className="num w-20 shrink-0 text-right text-xs">
          <span className="text-ok">+{f.added}</span> <span className="text-bad">{"−" + f.removed}</span>
        </span>
      )}
    </li>
  );
}

export function WhatChanged({ diff, highlights = [], repo, changeId }: { diff: DiffItem[]; highlights?: BriefHighlight[]; repo?: string; changeId?: string }) {
  const files = fromDiff(diff);
  if (!files.length) return <p className="text-base text-ink-3">This revision changes no files.</p>;
  const areas = whatChanged(files, highlights);
  const code = (path: string) => (repo && changeId ? repoHref(repo, "changes/" + changeId + "/code#file-" + encodeURIComponent(path)) : undefined);
  return (
    <div className="flex flex-col gap-2" data-what-changed>
      <p className="text-sm text-ink-3">{changeSize(files)}</p>
      <ul className="flex flex-col">
        {areas.map((a) => (
          <li key={a.area || "."} className="border-b border-line last:border-b-0">
            <details className="group">
              <summary className="flex min-h-11 cursor-pointer list-none items-start gap-2 py-2.5 [&::-webkit-details-marker]:hidden">
                <Icon name="chevronRight" size={16} className="mt-1 text-ink-3 transition-transform duration-[160ms] group-open:rotate-90" />
                <span className="flex min-w-0 flex-col gap-0.5">
                  <span className="text-base text-ink">{a.sentence}</span>
                  {a.highlights.map((h) => (
                    <span key={h} className="text-sm text-ink-2">
                      {h}
                    </span>
                  ))}
                </span>
              </summary>
              <ul className="mb-3 ml-6 flex flex-col">
                {[...a.files, ...a.tests].map((f) => (
                  <FileRow key={f.path} f={f} href={code(f.path)} />
                ))}
              </ul>
            </details>
          </li>
        ))}
      </ul>
    </div>
  );
}
