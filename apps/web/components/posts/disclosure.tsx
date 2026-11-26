"use client";

import { useId, useState, type ReactNode } from "react";
import { Button } from "@/components/kit";

export function PostDisclosure({ label, openLabel = label, children }: { label: string; openLabel?: string; children: ReactNode }) {
  const id = useId();
  const [open, setOpen] = useState(false);
  return (
    <div>
      <Button variant="ghost" size="sm" className="-ml-3.5" aria-expanded={open} aria-controls={id} onClick={() => setOpen(!open)}>
        {open ? openLabel : label}
      </Button>
      <div id={id} hidden={!open}>{children}</div>
    </div>
  );
}
