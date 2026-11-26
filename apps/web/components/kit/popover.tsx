"use client";

/**
 * A popover, on UI SDK's Popover: a button that opens a small panel beside it, for content a
 * menu cannot hold (commands to copy, a short form). It opens in the top layer, takes focus,
 * closes on Escape or an outside press, and returns focus to its button.
 */
import { Popover as UiPopover, PopoverContent, PopoverTitle, PopoverTrigger } from "@ui-sdk/react/popover";
import type { ReactNode } from "react";
import { tw } from "./cx";

export const POPOVER_PANEL = "m-0 max-h-(--available-height) overflow-y-auto rounded-xl border border-line bg-popover p-4 text-ink shadow-overlay outline-none";

export function Popover({
  title,
  trigger,
  triggerClass,
  children,
  align = "end",
  side = "bottom",
  className,
}: {
  /** Names the panel; read by screen readers, not shown. */
  title: string;
  trigger: ReactNode;
  triggerClass: string;
  children: ReactNode;
  align?: "start" | "center" | "end";
  side?: "top" | "bottom";
  className?: string;
}) {
  return (
    <UiPopover>
      <PopoverTrigger className={triggerClass}>{trigger}</PopoverTrigger>
      <PopoverContent side={side} align={align} sideOffset={6} collisionPadding={8} className={tw(POPOVER_PANEL, className)}>
        <PopoverTitle className="sr-only">{title}</PopoverTitle>
        {children}
      </PopoverContent>
    </UiPopover>
  );
}
