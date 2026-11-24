"use client";

import { useState, type ReactNode } from "react";
import { Icon } from "../icons";
import { EmptyState, Kbd } from "../ui";

export interface QueueListGroup {
  key: string;
  /** Null for a flat list (the Landed, Closed and All tabs). */
  label: string | null;
  empty: string;
  items: { key: string; text: string; node: ReactNode }[];
}

/**
 * The review queue's groups with one text filter over every row. `/` focuses the filter. A group
 * with no rows keeps its heading and says so in a sentence, so an empty "Needs you" reads as good
 * news rather than as a missing section; while filtering, empty groups fold away.
 */
export function QueueList({ groups, label, placeholder, empty }: { groups: QueueListGroup[]; label: string; placeholder: string; empty: ReactNode }) {
  const [q, setQ] = useState("");
  const terms = q.toLowerCase().split(/\s+/).filter(Boolean);
  const shown = groups.map((g) => ({ ...g, items: g.items.filter((i) => terms.every((t) => i.text.toLowerCase().includes(t))) }));
  const total = groups.reduce((n, g) => n + g.items.length, 0);
  const matches = shown.reduce((n, g) => n + g.items.length, 0);
  return (
    <div>
      <div className="mb-6 flex items-center gap-2 border-b border-line pb-2">
        <Icon name="search" className="text-ink-3" />
        <input data-filter value={q} onChange={(e) => setQ(e.target.value)} placeholder={placeholder} aria-label={label} className="h-8 flex-1 bg-transparent text-base outline-none placeholder:text-ink-3" />
        <Kbd>/</Kbd>
      </div>
      {total === 0 && groups.every((g) => g.label === null) ? (
        empty
      ) : q && matches === 0 ? (
        <EmptyState title="No matches" icon="search">
          Nothing contains “{q}”.
        </EmptyState>
      ) : (
        <div className="flex flex-col gap-10">
          {shown.map((g) => {
            if (q && g.items.length === 0) return null;
            const id = "queue-" + g.key;
            return (
              <section key={g.key} aria-labelledby={g.label ? id : undefined} data-group={g.key}>
                {g.label ? (
                  <h2 id={id} className="mb-1 flex items-baseline gap-2 text-xl font-semibold tracking-tight text-ink">
                    {g.label}
                    <span className="num text-base font-normal text-ink-3">{g.items.length}</span>
                  </h2>
                ) : null}
                {g.items.length ? (
                  <ul className="divide-y divide-line">
                    {g.items.map((i) => (
                      <li key={i.key}>{i.node}</li>
                    ))}
                  </ul>
                ) : (
                  <p className="py-3 text-base text-ink-3">{g.empty}</p>
                )}
              </section>
            );
          })}
        </div>
      )}
    </div>
  );
}
