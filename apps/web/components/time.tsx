"use client";

import { useEffect, useState } from "react";
import { absoluteTime, relativeTime } from "@/lib/format";

/** Relative time that stays fresh. The absolute UTC time is in the title and the datetime. */
export function Time({ iso, className }: { iso: string | undefined; className?: string }) {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const t = setInterval(() => setNow(Date.now()), 30_000);
    return () => clearInterval(t);
  }, []);
  if (!iso) return null;
  return (
    <time dateTime={iso} title={absoluteTime(iso)} className={className} suppressHydrationWarning>
      {relativeTime(iso, now)}
    </time>
  );
}
