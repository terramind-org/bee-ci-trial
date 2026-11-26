"use client";

/**
 * Controls that need a person, and the one dialog that asks them.
 *
 * `ConfirmButton` is the single wrapper for a control that approves or destroys something. It
 * sets UI SDK's `data-agent-confirm` to a sentence saying what pressing it does, so an agent
 * driving the page (the bridge in components/agent-bridge.tsx, or the browser's own through
 * WebMCP) sees the sentence in its snapshot and cannot press the control on its own: the step
 * waits for a person's answer, and without one fails with "Needs a person".
 *
 * `askPerson` puts a question to the person in the kit's alert dialog and resolves with their
 * answer. It serves both a person's own destructive clicks (in place of `window.confirm`) and
 * the agent bridge. `ConfirmHost` renders that dialog; app/layout.tsx mounts it once.
 */
import { AlertDialog, AlertDialogBackdrop, AlertDialogClose, AlertDialogContent, AlertDialogDescription, AlertDialogTitle, createConfirm } from "@ui-sdk/react/alert-dialog";
import { useState } from "react";
import { Button, buttonClass, type ButtonProps } from "./button";
import { DIALOG_PANEL } from "./dialog";

/** What a marked control does to the world. The marker components/kit/confirm.test.ts scans for. */
export type Consequence = "destructive" | "approving";

export type ConfirmButtonProps = ButtonProps & {
  /** One sentence, in plain words, saying what pressing this does: "Approves and merges pull request #41". */
  agentConfirm: string;
  /** Why it needs a person. Destructive controls default to the `danger` look, approving ones to solid ink. */
  consequence: Consequence;
};

export function ConfirmButton({ agentConfirm, consequence, variant, ...rest }: ConfirmButtonProps) {
  return <Button variant={variant ?? (consequence === "destructive" ? "danger" : "primary")} data-agent-confirm={agentConfirm} data-consequence={consequence} {...rest} />;
}

export interface Question {
  /** The question, as a sentence: "Revoke this token?". */
  title: string;
  /** What agreeing does, and what cannot be undone. */
  description?: string;
  /** The agreeing button's words. */
  confirmLabel?: string;
  cancelLabel?: string;
  /** Styles the agreeing button as destructive. */
  destructive?: boolean;
}

type Confirm = ReturnType<typeof createConfirm<Question>>;
let shared: Confirm | null = null;
const confirmer = (): Confirm => (shared ??= createConfirm<Question>());

/**
 * Asks the person and resolves `true` only when they agree; Cancel, Escape and an aborted
 * `signal` (the agent bridge's timeout) all resolve `false`. Questions asked at once queue.
 */
export function askPerson(question: Question, opts: { signal?: AbortSignal } = {}): Promise<boolean> {
  const confirm = confirmer();
  const { signal } = opts;
  if (signal?.aborted) return Promise.resolve(false);
  const asked = confirm(question);
  if (!signal) return asked;
  const close = () => confirm.controller.close();
  signal.addEventListener("abort", close, { once: true });
  return asked.finally(() => signal.removeEventListener("abort", close));
}

/** The dialog `askPerson` opens. Mount once, near the root. */
export function ConfirmHost() {
  const [confirm] = useState(confirmer);
  return (
    <AlertDialog controller={confirm.controller}>
      <AlertDialogBackdrop />
      <AlertDialogContent<Question> className={DIALOG_PANEL + " top-[18vh] w-[min(460px,calc(100vw-24px))] p-6"}>
        {(state) => (
          <>
            <AlertDialogTitle className="text-lg font-normal tracking-tight text-ink">{state.payload?.title}</AlertDialogTitle>
            {state.payload?.description ? <AlertDialogDescription className="mt-2 text-base text-ink-2">{state.payload.description}</AlertDialogDescription> : null}
            <div className="mt-6 flex flex-wrap justify-end gap-2.5">
              <AlertDialogClose className={buttonClass("secondary")}>{state.payload?.cancelLabel ?? "Cancel"}</AlertDialogClose>
              <AlertDialogClose value="confirm" className={buttonClass(state.payload?.destructive ? "danger" : "primary")}>
                {state.payload?.confirmLabel ?? "Continue"}
              </AlertDialogClose>
            </div>
          </>
        )}
      </AlertDialogContent>
    </AlertDialog>
  );
}
