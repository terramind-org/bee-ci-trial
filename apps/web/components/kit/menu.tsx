"use client";

/**
 * Menus, on UI SDK's Menu: a button that opens a short list of actions or links in the top
 * layer. Arrow keys, typeahead, Escape and focus return come from the primitive.
 *
 * `label` names the menu itself. `labelled` also names the button with it, for a trigger that
 * is only an icon; otherwise the trigger's own text names the button.
 */
import { Menu as UiMenu, MenuContent, MenuItem, MenuTrigger } from "@ui-sdk/react/menu";
import Link from "next/link";
import type { ReactNode } from "react";
import { Icon, type IconName } from "../icons";
import { tw } from "./cx";

export type MenuPlace = "below-end" | "below-start" | "above-start";

const PLACE: Record<MenuPlace, { side: "top" | "bottom"; align: "start" | "end" }> = {
  "below-end": { side: "bottom", align: "end" },
  "below-start": { side: "bottom", align: "start" },
  "above-start": { side: "top", align: "start" },
};

export const MENU_PANEL = "m-0 max-h-(--available-height) overflow-y-auto min-w-60 rounded-xl border border-line bg-popover p-1.5 text-ink shadow-overlay outline-none";

export function Menu({
  label,
  trigger,
  triggerClass,
  children,
  place = "below-end",
  labelled = false,
  className,
}: {
  label: string;
  trigger: ReactNode;
  triggerClass: string;
  children: ReactNode;
  place?: MenuPlace;
  labelled?: boolean;
  className?: string;
}) {
  return (
    <UiMenu>
      <MenuTrigger className={triggerClass} aria-label={labelled ? label : undefined}>
        {trigger}
      </MenuTrigger>
      <MenuContent aria-label={label} side={PLACE[place].side} align={PLACE[place].align} sideOffset={6} collisionPadding={8} className={tw(MENU_PANEL, className)}>
        {children}
      </MenuContent>
    </UiMenu>
  );
}

export const MENU_ITEM = "flex min-h-9 w-full cursor-pointer items-center gap-3 rounded-md px-2.5 text-left text-base text-ink outline-none hover:bg-hover data-[highlighted]:bg-hover data-[disabled]:cursor-not-allowed data-[disabled]:opacity-50";

export function MenuLink({ href, icon, children }: { href: string; icon?: IconName; children: ReactNode }) {
  return (
    <MenuItem render={<Link href={href} />} className={MENU_ITEM}>
      {icon ? <Icon name={icon} size={18} className="text-ink-3" /> : null}
      {children}
    </MenuItem>
  );
}

export function MenuButton({ icon, onSelect, children }: { icon?: IconName; onSelect: () => unknown; children: ReactNode }) {
  return (
    <MenuItem onSelect={() => onSelect()} className={MENU_ITEM}>
      {icon ? <Icon name={icon} size={18} className="text-ink-3" /> : null}
      {children}
    </MenuItem>
  );
}

/** A plain label inside a menu, above a group of items. */
export function MenuLabel({ children }: { children: ReactNode }) {
  return <p className="px-2.5 pt-1.5 pb-1 text-xs text-ink-3">{children}</p>;
}
