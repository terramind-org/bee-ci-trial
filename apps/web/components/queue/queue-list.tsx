"use client";

import { useState, type ReactNode } from "react";
import type { ChangeState } from "@/lib/api/types";
import { Icon } from "../icons";
import { PullListLandings } from "../pull/landing";
import { EmptyState, Kbd } from "../ui";

export interface QueueListGroup {
  key: string;
  /** Null for a flat list (the Merged, Closed and All tabs). */
  label: string | null;
  empty: string;
  items: { key: string; text: string; node: ReactNode }[];
}

/**
 * The pull-request list's groups with one quiet text filter over every row. `/` focuses the
 * filter. A group with no rows keeps its heading and says so in a sentence, so an empty "Needs
 * you" reads as good news rather than as a missing section; while filtering, empty groups fold away.
 */
export function QueueList({ groups, label, placeholder, empty, landing }: { groups: QueueListGroup[]; label: string; placeholder: string; empty: ReactNode; landing: { scope: string; states: Record<string, ChangeState> } }) {
  const [q, setQ] = useState("");
  const terms = q.toLowerCase().split(/\s+/).filter(Boolean);
  const shown = groups.map((g) => ({ ...g, items: g.items.filter((i) => terms.every((t) => i.text.toLowerCase().includes(t))) }));
  const total = groups.reduce((n, g) => n + g.items.length, 0);
  const matches = shown.reduce((n, g) => n + g.items.length, 0);
  return (
    // Reset the observation when the filter changes. Revealing a previously hidden merged row
    // should show its resting picture, even if it merged while the list was filtered.
    <PullListLandings scope={JSON.stringify([landing.scope, q])} states={landing.states}>
      <div>
        <div className="mb-8 flex max-w-md items-center gap-2 text-ink-2">
          <Icon name="search" size={16} />
          <input data-filter value={q} onChange={(e) => setQ(e.target.value)} placeholder={placeholder} aria-label={label} className="h-8 min-w-0 flex-1 bg-transparent text-base text-ink outline-none placeholder:text-ink-3" />
          <Kbd>/</Kbd>
        </div>
        {total === 0 && groups.every((g) => g.label === null) ? (
          empty
        ) : q && matches === 0 ? (
          <EmptyState title="No matches">Nothing contains “{q}”.</EmptyState>
        ) : (
          <div className="flex flex-col gap-9">
            {shown.map((g) => {
              if (q && g.items.length === 0) return null;
              const id = "queue-" + g.key;
              return (
                <section key={g.key} aria-labelledby={g.label ? id : undefined} aria-label={g.label ? undefined : label.replace(/^Filter /, "")} data-group={g.key}>
                  {g.label ? (
                    <h2 id={id} className="mb-3 flex items-baseline gap-2 text-xs font-medium tracking-normal text-ink-2">
                      {g.label}
                      <span className="num font-normal">{g.items.length}</span>
                    </h2>
                  ) : null}
                  {g.items.length ? (
                    <ul className="border-t border-line">
                      {g.items.map((i) => (
                        <li key={i.key}>{i.node}</li>
                      ))}
                    </ul>
                  ) : (
                    <p className="border-t border-line py-4 text-base text-ink-2">{g.empty}</p>
                  )}
                </section>
              );
            })}
          </div>
        )}
      </div>
    </PullListLandings>
  );
}
