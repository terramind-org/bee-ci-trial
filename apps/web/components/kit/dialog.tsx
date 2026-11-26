"use client";

/**
 * Dialogs, on UI SDK's Dialog. They open in the browser's top layer, so there is no portal and
 * no z-index; the backdrop and the enter and exit motion are styled once in app/globals.css.
 *
 * `Modal` is the common case: a titled panel with a close button, controlled by `open`. The
 * parts are exported too, for a dialog with its own trigger or layout (the navigation drawer).
 */
import { Dialog, DialogBackdrop, DialogClose, DialogContent, DialogDescription, DialogTitle, DialogTrigger } from "@ui-sdk/react/dialog";
import type { ComponentProps, ReactNode } from "react";
import { Icon } from "../icons";
import { buttonClass } from "./button";
import { tw } from "./cx";

/** A panel in the top layer: centred across, placed from the top, on the popover surface. */
export const DIALOG_PANEL =
  "fixed inset-x-0 top-[10vh] bottom-auto m-0 mx-auto max-h-[80vh] w-[min(600px,calc(100vw-24px))] overflow-y-auto rounded-2xl border border-line bg-popover p-5 text-ink shadow-overlay outline-none";

type ContentProps = ComponentProps<typeof DialogContent>;

export interface ModalProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  title: ReactNode;
  /** One or two sentences under the title, read out with the dialog. */
  description?: ReactNode;
  /** Keeps the title for screen readers only, when the content names itself (the palette). */
  hideTitle?: boolean;
  /** Leaves out the header's close button, for a dialog whose own buttons close it. */
  hideClose?: boolean;
  /** Classes for the panel: width, position and padding replace the defaults. */
  className?: string;
  initialFocus?: ContentProps["initialFocus"];
  children: ReactNode;
}

export function Modal({ open, onOpenChange, title, description, hideTitle = false, hideClose = false, className, initialFocus, children }: ModalProps) {
  return (
    <Dialog open={open} onOpenChange={(next) => onOpenChange(next)}>
      <DialogBackdrop />
      <DialogContent className={tw(DIALOG_PANEL, className)} initialFocus={initialFocus}>
        {hideTitle ? (
          <DialogTitle className="sr-only">{title}</DialogTitle>
        ) : (
          <div className="mb-4 flex items-start justify-between gap-3">
            <div className="min-w-0">
              <DialogTitle className="text-lg font-normal tracking-tight text-ink">{title}</DialogTitle>
              {description ? <DialogDescription className="mt-1 text-base text-ink-2">{description}</DialogDescription> : null}
            </div>
            {hideClose ? null : (
              <DialogClose className={buttonClass("ghost", "sm", "size-9 px-0")} aria-label="Close">
                <Icon name="x" size={18} />
              </DialogClose>
            )}
          </div>
        )}
        {hideTitle && description ? <DialogDescription className="sr-only">{description}</DialogDescription> : null}
        {children}
      </DialogContent>
    </Dialog>
  );
}

export { Dialog, DialogBackdrop, DialogClose, DialogContent, DialogDescription, DialogTitle, DialogTrigger };
