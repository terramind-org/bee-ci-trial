"use client";

import { useState } from "react";
import { CopyAsDialog } from "@/components/copy-as";
import { MenuButton, PageMenu } from "@/components/menu";
import type { Op } from "@/lib/copyas";

export function RunMenu({ op, alternatives }: { op: Op; alternatives: { key: string; op: Op; label: string }[] }) {
  const [copy, setCopy] = useState<Op>();
  return (
    <>
      <PageMenu op={op}>
        {alternatives.map((item) => (
          <MenuButton key={item.key} icon="terminal" onSelect={() => setCopy(item.op)}>{item.label}</MenuButton>
        ))}
      </PageMenu>
      {copy && <CopyAsDialog op={copy} open onOpenChange={(open) => { if (!open) setCopy(undefined); }} />}
    </>
  );
}
