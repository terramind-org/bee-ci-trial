"use client";

/**
 * Menus. The menu itself is the kit's (components/kit/menu.tsx, on UI SDK) and is re-exported
 * here under its first names. `PageMenu` is a page's single "…" menu, the one place *Copy as*
 * lives (docs/design/tokens.md, "One … menu per page").
 */
import { useState, type ReactNode } from "react";
import type { Op } from "@/lib/copyas";
import { CopyAsDialog, PageOp } from "./copy-as";
import { Icon } from "./icons";
import { buttonClass } from "./kit/button";
import { Menu, MenuButton } from "./kit/menu";

export { MENU_ITEM, Menu, MenuButton, MenuLabel, MenuLink } from "./kit/menu";

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
      <Menu label={label} labelled triggerClass={buttonClass("ghost", "sm", "size-10 px-0")} trigger={<Icon name="more" size={22} weight="bold" />}>
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
