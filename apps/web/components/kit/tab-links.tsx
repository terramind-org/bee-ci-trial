/** Link-based tabs for sub-views that have their own URL: plain words over one hairline. */
import Link from "next/link";
import { Swap } from "../motion";
import { tabClass } from "./tabs-style";

// Settings uses the same links in a sidebar, wrapping them above the content on phones.
export function TabLinks({ items, label, sidebar = false }: { items: { href: string; label: string; active: boolean; count?: number }[]; label: string; sidebar?: boolean }) {
  return (
    <nav aria-label={label} className={sidebar ? "max-md:border-b max-md:border-line" : "mb-8 border-b border-line"}>
      <ul className={sidebar ? "flex flex-wrap gap-x-4 md:flex-col md:items-start" : "-mb-px flex gap-6 overflow-x-auto scrollbar-thin"}>
        {items.map((t) => (
          <li key={t.href} className="shrink-0">
            <Link href={t.href} aria-current={t.active ? "page" : undefined} className={tabClass(t.active)}>
              {t.label}
              {t.count !== undefined ? (
                <Swap value={t.count} className="num text-xs text-ink-3">
                  {t.count}
                </Swap>
              ) : null}
            </Link>
          </li>
        ))}
      </ul>
    </nav>
  );
}
