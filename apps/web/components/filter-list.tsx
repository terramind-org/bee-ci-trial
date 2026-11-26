"use client";

import { useRef, useState, type ReactNode } from "react";
import { Icon } from "./icons";
import { Input } from "./kit";
import { useFlip, usePresence } from "./motion";
import { EmptyState, Kbd } from "./ui";

/** Client-side text filter over server-rendered rows. `/` focuses it. */
export function FilterList<T extends { key: string; text: string; node: ReactNode }>({ items, placeholder, empty, label, quiet }: { items: T[]; placeholder: string; empty: ReactNode; label: string; quiet?: boolean }) {
  const [q, setQ] = useState("");
  const terms = q.toLowerCase().split(/\s+/).filter(Boolean);
  const shown = items.filter((i) => terms.every((t) => i.text.toLowerCase().includes(t)));
  const list = useRef<HTMLUListElement>(null);
  const rows = usePresence(shown, (i) => i.key, q);
  useFlip(list);
  // Issues uses the kit field and a single-line result message.
  const FilterInput = quiet ? Input : "input";
  return (
    <div>
      <div className="flex items-center gap-2 border-b border-line px-3.5 py-2">
        <Icon name="search" className="text-ink-3" />
        <FilterInput
          data-filter
          value={q}
          onChange={(e) => setQ(e.target.value)}
          placeholder={placeholder}
          aria-label={label}
          className={quiet ? "h-9 min-w-0 flex-1 px-3 text-sm" : "h-7 flex-1 bg-transparent text-sm outline-none placeholder:text-ink-3"}
        />
        <Kbd>/</Kbd>
      </div>
      {shown.length === 0 ? (
        items.length === 0 ? (
          empty
        ) : quiet ? (
          <p role="status" className="py-5 text-base text-ink-2">No matching issues.</p>
        ) : (
          <EmptyState title="No matches" icon="search">
            Nothing contains “{q}”.
          </EmptyState>
        )
      ) : (
        <ul ref={list} className="divide-y divide-line">
          {rows.map(({ item, key, leaving }) =>
            leaving ? (
              <li key={key} data-key={key} aria-hidden="true" className="pointer-events-none animate-leave">
                {item.node}
              </li>
            ) : (
              <li key={key} data-key={key}>
                {item.node}
              </li>
            ),
          )}
        </ul>
      )}
    </div>
  );
}
