/**
 * The root of the repo as a plain list: directories first, each linking into the code browser.
 * No box around it. Each row can say what last changed it, and shows an avatar stack when an
 * agent or person holds a lease on that path or below it.
 */
import Link from "next/link";
import { Icon } from "@/components/icons";
import { Time } from "@/components/time";
import { pathText } from "@/lib/api/ids";
import type { Lease, RepoWorkItem, TreeEntry } from "@/lib/api/types";
import { bytes } from "@/lib/format";
import { repoHref } from "@/lib/routes";
import { Presence, leaseTouches } from "./presence";

/** Directories before files, then by name, so the order is the same on every load. */
export function orderTree(entries: TreeEntry[]): TreeEntry[] {
  return [...entries].sort((a, b) => Number(b.mode === "MODE_TREE") - Number(a.mode === "MODE_TREE") || pathText(a.path).localeCompare(pathText(b.path)));
}

/** Rows shown before the rest is left to the code browser. */
export const ROOT_ROWS = 12;

/** The last change to touch a path: its title and when. */
export interface LastChange {
  title: string;
  at: string;
}

export function FileTreeRoot({
  fullName,
  entries,
  lasts = {},
  leases = [],
  work = [],
  hrefFor,
  limit = ROOT_ROWS,
  empty = "The root of this repository is empty.",
  label = "Files in the repository root",
}: {
  fullName: string;
  entries: TreeEntry[];
  lasts?: Record<string, LastChange | undefined>;
  leases?: Lease[];
  work?: RepoWorkItem[];
  /** Where a path links; the home links straight into the code browser. */
  hrefFor?: (path: string) => string;
  limit?: number;
  empty?: string;
  label?: string;
}) {
  if (!entries.length) return <p className="py-3 text-base text-ink-3">{empty}</p>;
  const ordered = orderTree(entries);
  return (
    <>
      <ul className="-mx-3 flex flex-col" aria-label={label}>
        {ordered.slice(0, limit).map((e) => {
          const name = pathText(e.path);
          const dir = e.mode === "MODE_TREE";
          const href = hrefFor ? hrefFor(name) : repoHref(fullName, "code?path=" + encodeURIComponent(name));
          const last = lasts[name];
          const editing = leases.filter((l) => l.paths.some((p) => leaseTouches(pathText(p), name)));
          return (
            <li key={name} className="grid min-h-12 grid-cols-[minmax(0,1fr)_auto] items-center gap-x-4 rounded-lg px-3 hover:bg-hover md:grid-cols-[minmax(0,14rem)_minmax(0,1fr)_auto] md:gap-x-6">
              <Link href={href} data-nav-item className="flex min-h-12 min-w-0 items-center gap-3 text-md text-ink focus-visible:outline-2 focus-visible:outline-offset-[-2px] focus-visible:outline-(--focus)">
                <Icon name={e.restricted ? "lock" : dir ? "folder" : "file"} size={20} weight={dir ? "fill" : "regular"} className="shrink-0 text-ink-3" />
                <span className="mono min-w-0 truncate">
                  {name.split("/").pop()}
                  {dir ? "/" : ""}
                </span>
              </Link>
              <span className="hidden min-w-0 truncate text-base text-ink-3 md:block" title={last?.title}>
                {last?.title}
              </span>
              <span className="inline-flex items-center justify-end gap-4 text-sm whitespace-nowrap text-ink-3">
                <Presence repo={fullName} work={work} leases={editing} under={name} />
                {e.restricted ? <span title="You cannot read this path">restricted</span> : !dir ? <span className="num">{bytes(e.size)}</span> : null}
                {last?.at ? <Time iso={last.at} className="hidden w-20 text-right sm:inline" /> : null}
              </span>
            </li>
          );
        })}
      </ul>
      {ordered.length > limit ? (
        <Link href={repoHref(fullName, "code")} className="link mt-2 inline-flex min-h-6 items-center text-base">
          Show all {ordered.length} entries
        </Link>
      ) : null}
    </>
  );
}
