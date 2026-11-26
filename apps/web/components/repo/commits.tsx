/**
 * Recent commits on a repository's home: the message, the short id, and under it who wrote it.
 * An agent's commit names the agent and the person it answers to; a person's commit names the
 * person. The yellow mark sits beside the person, as everywhere.
 */
import Link from "next/link";
import { Mark, Sep } from "@/components/parts";
import { Time } from "@/components/time";
import { objectIdText } from "@/lib/api/ids";
import type { Changeset, Principal } from "@/lib/api/types";
import { runtimeName } from "@/lib/team";

export interface CommitView {
  /** Full text id, for keys and titles. */
  id: string;
  /** The short id people quote: ten hex digits. */
  short: string;
  title: string;
  /** Who wrote it: the agent's or the person's name. */
  author: string;
  agent: boolean;
  /** The person who answers for it, when known: the author, or the agent's owner. */
  person?: string;
  runtime: string;
  model: string;
  at?: string;
  /** A short label before the id, when the row stands for something: "r2" for a revision's head. */
  note?: string;
}

/**
 * One changeset as a row. An agent's commit carries provenance, whose principal chain is the
 * agent and then the person behind it; the person's name comes from `directory` when the page
 * could read the organisation's members, and is left out otherwise.
 */
export function commitView(c: Changeset, directory: Map<string, Principal> = new Map()): CommitView {
  const meta = c.meta;
  const prov = meta?.provenance;
  const agent = !!prov;
  const owner = agent ? prov?.principalChain.slice(1).map((id) => directory.get(id)).find((p) => p?.kind === "PRINCIPAL_KIND_HUMAN") : undefined;
  return {
    id: objectIdText(c.id),
    short: objectIdText(c.id, true).replace(/^c\d_/, ""),
    title: (meta?.message ?? "").split("\n")[0]?.trim() || "(no message)",
    author: meta?.author || meta?.creator || "",
    agent,
    person: agent ? owner?.display : meta?.author || undefined,
    runtime: runtimeName(prov?.runtime ?? ""),
    model: prov?.model ?? "",
    at: meta?.committerTime?.time ?? meta?.authorTime?.time,
  };
}

/** The same row as one line of text, for the agent's view. */
export function commitLine(c: CommitView): string {
  const who = c.agent ? c.author + (c.person ? " for " + c.person : "") + (c.model ? " · " + c.model : "") : c.person || c.author;
  return (c.note ? c.note + " · " : "") + c.short + " · " + c.title + (who ? " · " + who : "") + (c.at ? " · " + c.at : "");
}

export function CommitRows({ commits, label = "Recent commits", hrefFor }: { commits: CommitView[]; label?: string; hrefFor?: (c: CommitView) => string | undefined }) {
  return (
    <ul aria-label={label} className="flex flex-col divide-y divide-line" data-testid="commits">
      {commits.map((c) => {
        const href = hrefFor?.(c);
        return (
          <li key={c.id} className="grid grid-cols-[minmax(0,1fr)_auto] gap-x-5 gap-y-0.5 py-[11px] text-md">
            <span className="min-w-0 [overflow-wrap:anywhere]">
              {href ? (
                <Link href={href} className="hover:underline hover:underline-offset-4">
                  {c.title}
                </Link>
              ) : (
                c.title
              )}
            </span>
            <span className="text-right text-ink-2">
              {c.note ? <span className="mr-2 text-xs">{c.note}</span> : null}
              <span className="mono" title={c.id}>
                {c.short}
              </span>
            </span>
            <span className="col-span-2 flex flex-wrap items-center gap-x-2 gap-y-0.5 text-xs text-ink-2">
              {c.agent ? (
                <>
                  <span>{c.author}</span>
                  {c.person ? (
                    <>
                      <Sep />
                      <span className="inline-flex items-center gap-1.5">
                        for <Mark>{c.person}</Mark>
                      </span>
                    </>
                  ) : null}
                  {c.runtime ? (
                    <>
                      <Sep />
                      <span>{c.runtime}</span>
                    </>
                  ) : null}
                </>
              ) : c.person ? (
                <Mark>{c.person}</Mark>
              ) : null}
              {c.at ? (
                <>
                  <Sep />
                  <Time iso={c.at} />
                </>
              ) : null}
            </span>
          </li>
        );
      })}
    </ul>
  );
}
