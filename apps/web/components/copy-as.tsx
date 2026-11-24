"use client";

import * as Dialog from "@radix-ui/react-dialog";
import dynamic from "next/dynamic";
import { useEffect, useState, useSyncExternalStore } from "react";
import type { Op } from "@/lib/copyas";
import { Icon } from "./icons";
import { Kbd, buttonClass, cx } from "./ui";

/** Loaded when a dialog opens: the panel needs every service's descriptors (see copy-as-panel.tsx). */
const CopyAsPanel = dynamic(() => import("./copy-as-panel").then((m) => m.CopyAsPanel), {
  ssr: false,
  loading: () => <span aria-hidden="true" className="block h-[148px] animate-pulse rounded-md bg-sunken" />,
});

/* ---------- page op registry: `y` opens Copy-as for the page's primary operation ---------- */

let pageOp: Op | null = null;
const subs = new Set<() => void>();
function setPageOp(op: Op | null) {
  pageOp = op;
  for (const s of subs) s();
}
export function usePageOp(): Op | null {
  return useSyncExternalStore(
    (cb) => {
      subs.add(cb);
      return () => subs.delete(cb);
    },
    () => pageOp,
    () => null,
  );
}

/** Declares the page's primary operation (what `y` copies). Renders nothing. */
export function PageOp({ op }: { op: Op }) {
  const key = JSON.stringify(op);
  useEffect(() => {
    setPageOp(JSON.parse(key) as Op);
    return () => setPageOp(null);
  }, [key]);
  return null;
}

/* ---------- the dialog ---------- */

export function CopyAsDialog({ op, open, onOpenChange }: { op: Op; open: boolean; onOpenChange: (v: boolean) => void }) {
  return (
    <Dialog.Root open={open} onOpenChange={onOpenChange}>
      <Dialog.Portal>
        <Dialog.Overlay className="fixed inset-0 z-40 bg-black/50 data-[state=open]:animate-enter" />
        <Dialog.Content className="fixed top-[12vh] left-1/2 z-50 w-[min(680px,calc(100vw-24px))] -translate-x-1/2 rounded-lg border border-line bg-popover p-4 shadow-overlay data-[state=open]:animate-enter">
          <div className="mb-3 flex items-start justify-between gap-3">
            <div>
              <Dialog.Title className="text-md font-semibold">{op.label ?? "Copy as"}</Dialog.Title>
              <Dialog.Description className="text-sm text-ink-2">The same operation for agents: CLI, HTTP or MCP. All three behave identically.</Dialog.Description>
            </div>
            <Dialog.Close className={buttonClass("ghost", "sm")} aria-label="Close">
              <Icon name="x" size={14} />
            </Dialog.Close>
          </div>
          <CopyAsPanel op={op} />
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  );
}

/** The affordance shown next to actions and in page headers. */
export function CopyAs({ op, compact = false, className }: { op: Op; compact?: boolean; className?: string }) {
  const [open, setOpen] = useState(false);
  return (
    <>
      <button
        type="button"
        onClick={() => setOpen(true)}
        className={cx(buttonClass("ghost", "sm"), className)}
        aria-label={compact ? "Copy as CLI, HTTP or MCP: " + (op.label ?? op.method) : undefined}
        title={compact ? "Copy as CLI / HTTP / MCP" : undefined}
        data-copy-as={op.service + "/" + op.method}
      >
        <Icon name="terminal" size={14} />
        {compact ? null : <span>Copy as</span>}
      </button>
      {open ? <CopyAsDialog op={op} open={open} onOpenChange={setOpen} /> : null}
    </>
  );
}

/** Mounted once in the shell: `y` opens Copy-as for the current page's operation. */
export function GlobalCopyAs() {
  const op = usePageOp();
  const [open, setOpen] = useState(false);
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "y" || e.metaKey || e.ctrlKey || e.altKey) return;
      const t = e.target as HTMLElement | null;
      if (t && (t.isContentEditable || /^(INPUT|TEXTAREA|SELECT)$/.test(t.tagName))) return;
      if (document.querySelector("[role=dialog]")) return;
      if (!pageOp) return;
      e.preventDefault();
      setOpen(true);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);
  if (!op || !open) return null;
  return <CopyAsDialog op={op} open={open} onOpenChange={setOpen} />;
}

export function CopyAsHint() {
  return (
    <span className="hidden items-center gap-1 text-xs text-ink-3 md:inline-flex">
      <Kbd>y</Kbd> copy as
    </span>
  );
}
