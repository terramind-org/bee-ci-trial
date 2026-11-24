"use client";

/**
 * Menus. `Menu` is a small accessible menu button; `PageMenu` is a page's single "…" menu, the
 * one place *Copy as* lives (docs/design/tokens.md, "One … menu per page").
 */
import Link from "next/link";
import { useEffect, useId, useRef, useState, type ReactNode } from "react";
import type { Op } from "@/lib/copyas";
import { CopyAsDialog, PageOp } from "./copy-as";
import { Icon, type IconName } from "./icons";
import { buttonClass, cx } from "./ui";

/**
 * Enter/Space/ArrowDown open it on the first item, arrows move, Escape and outside clicks close
 * it and return focus to the button. `labelled` names the button with `label` (for icon-only
 * triggers); otherwise the trigger's own text names it.
 */
export function Menu({
  label,
  trigger,
  triggerClass,
  children,
  place = "below-end",
  labelled = false,
}: {
  label: string;
  trigger: ReactNode;
  triggerClass: string;
  children: ReactNode;
  place?: "below-end" | "below-start" | "above-start";
  labelled?: boolean;
}) {
  const [open, setOpen] = useState(false);
  const id = useId();
  const box = useRef<HTMLDivElement>(null);
  const button = useRef<HTMLButtonElement>(null);
  const items = () => [...(box.current?.querySelectorAll<HTMLElement>("[role=menuitem]") ?? [])];
  useEffect(() => {
    if (!open) return;
    items()[0]?.focus();
    const away = (e: MouseEvent) => {
      if (!box.current?.contains(e.target as Node) && !button.current?.contains(e.target as Node)) setOpen(false);
    };
    document.addEventListener("mousedown", away);
    return () => document.removeEventListener("mousedown", away);
  }, [open]);
  return (
    <div className="relative">
      <button
        ref={button}
        type="button"
        aria-haspopup="menu"
        aria-expanded={open}
        aria-controls={open ? id : undefined}
        aria-label={labelled ? label : undefined}
        className={triggerClass}
        onClick={() => setOpen((v) => !v)}
        onKeyDown={(e) => {
          if (e.key === "ArrowDown") {
            e.preventDefault();
            setOpen(true);
          }
        }}
      >
        {trigger}
      </button>
      {open ? (
        <div
          ref={box}
          id={id}
          role="menu"
          aria-label={label}
          className={cx(
            "absolute z-50 min-w-60 rounded-xl border border-line bg-popover p-1.5 shadow-overlay animate-enter",
            place === "below-end" ? "top-[calc(100%+6px)] right-0" : place === "below-start" ? "top-[calc(100%+6px)] left-0" : "bottom-[calc(100%+6px)] left-0 right-0 min-w-0",
          )}
          onClick={(e) => {
            if ((e.target as HTMLElement).closest("[role=menuitem]")) setOpen(false);
          }}
          onKeyDown={(e) => {
            const list = items();
            const i = list.indexOf(document.activeElement as HTMLElement);
            if (e.key === "Escape" || e.key === "Tab") {
              if (e.key === "Escape") e.preventDefault();
              setOpen(false);
              button.current?.focus();
            } else if (e.key === "ArrowDown" || e.key === "ArrowUp") {
              e.preventDefault();
              const n = e.key === "ArrowDown" ? (i + 1) % list.length : (i - 1 + list.length) % list.length;
              list[n]?.focus();
            } else if (e.key === "Home" || e.key === "End") {
              e.preventDefault();
              list[e.key === "Home" ? 0 : list.length - 1]?.focus();
            }
          }}
        >
          {children}
        </div>
      ) : null}
    </div>
  );
}

export const MENU_ITEM = "flex h-9 w-full items-center gap-3 rounded-md px-2.5 text-left text-base text-ink outline-none hover:bg-hover focus-visible:bg-hover";

export function MenuLink({ href, icon, children }: { href: string; icon: IconName; children: ReactNode }) {
  return (
    <Link href={href} role="menuitem" tabIndex={-1} className={MENU_ITEM}>
      <Icon name={icon} size={18} className="text-ink-3" />
      {children}
    </Link>
  );
}

export function MenuButton({ icon, onSelect, children }: { icon: IconName; onSelect: () => void; children: ReactNode }) {
  return (
    <button type="button" role="menuitem" tabIndex={-1} className={MENU_ITEM} onClick={onSelect}>
      <Icon name={icon} size={18} className="text-ink-3" />
      {children}
    </button>
  );
}

/** A plain label inside a menu, above a group of items. */
export function MenuLabel({ children }: { children: ReactNode }) {
  return <p className="px-2.5 pt-1.5 pb-1 text-sm text-ink-3">{children}</p>;
}

/**
 * The page's one "…" menu. *Copy as* opens the page's operation (`op`) as a CLI command, an HTTP
 * call or an MCP tool call, and `y` opens the same thing anywhere on the page. `children` adds
 * page-specific items above Copy as (e.g. a link to the raw file). Menus in a layout (the repo
 * header) pass `register={false}`, so the page under them keeps `y`.
 */
export function PageMenu({ op, children, label = "More actions", register = true }: { op: Op; children?: ReactNode; label?: string; register?: boolean }) {
  const [copy, setCopy] = useState(false);
  return (
    <>
      {register ? <PageOp op={op} /> : null}
      <Menu label={label} labelled triggerClass={buttonClass("ghost", "md", "size-10 px-0")} trigger={<Icon name="more" size={22} weight="bold" />}>
        {children}
        <MenuButton icon="link" onSelect={() => void navigator.clipboard?.writeText(window.location.href)}>
          Copy link
        </MenuButton>
        <MenuButton icon="terminal" onSelect={() => setCopy(true)}>
          Copy as CLI, HTTP or MCP
        </MenuButton>
      </Menu>
      {copy ? <CopyAsDialog op={op} open={copy} onOpenChange={setCopy} /> : null}
    </>
  );
}
