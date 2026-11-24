"use client";

import { usePathname } from "next/navigation";
import { useEffect, useRef } from "react";
import { TabLinks } from "../ui";

export const ADMIN_TABS = [
  { href: "/admin", label: "Overview" },
  { href: "/admin/readiness", label: "Readiness" },
  { href: "/admin/enterprise", label: "Enterprise" },
  { href: "/admin/identity", label: "Identity" },
  { href: "/admin/security", label: "Security" },
  { href: "/admin/policies", label: "Policies" },
  { href: "/admin/plans", label: "Plans" },
] as const;

/** The console's sections; the one for the current path is selected. */
export function AdminTabs() {
  const path = usePathname();
  const box = useRef<HTMLDivElement>(null);
  // On a phone the strip scrolls; bring the current tab into view, scrolling the strip only.
  useEffect(() => {
    const strip = box.current?.querySelector("ul");
    const tab = strip?.querySelector<HTMLElement>('[aria-current="page"]');
    if (!strip || !tab) return;
    const left = tab.offsetLeft - strip.offsetLeft;
    strip.scrollLeft = Math.max(0, left - (strip.clientWidth - tab.offsetWidth) / 2);
  }, [path]);
  return (
    <div ref={box}>
      <TabLinks label="Admin sections" items={ADMIN_TABS.map((t) => ({ ...t, active: t.href === "/admin" ? path === "/admin" : path === t.href || path.startsWith(t.href + "/") }))} />
    </div>
  );
}
