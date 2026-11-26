"use client";

import { useRouter } from "next/navigation";
import { useState, type ReactNode } from "react";
import { adminWrite, type AdminResult, type AdminWrite } from "@/app/(shell)/admin/actions";
import { ConfirmButton as MarkedButton } from "../kit/confirm";
import { Modal } from "../kit/dialog";
import { useToast } from "../toast";
import { Button, cx } from "../ui";

export interface ActionFailure {
  reason: string;
  message: string;
  unimplemented: boolean;
}

/**
 * Runs an admin write through its server action. `busy` and `error` drive the form; a success
 * toasts and refreshes the page's data, a failure shows the server's SPEC §12.2 reason.
 */
export function useAdminAction() {
  const router = useRouter();
  const toast = useToast();
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<ActionFailure | null>(null);

  async function run<K extends AdminWrite>(name: K, request: Parameters<typeof adminWrite<K>>[1], success: string, opts: { detail?: string; refresh?: boolean } = {}): Promise<Extract<AdminResult<Awaited<ReturnType<typeof adminWrite<K>>> extends AdminResult<infer D> ? D : never>, { ok: true }> | null> {
    setBusy(true);
    setError(null);
    try {
      const res = await adminWrite(name, request);
      if (!res.ok) {
        setError(res);
        toast.push({ tone: "bad", title: (res.unimplemented ? "Not available on this server" : "That did not work") + " · " + res.reason, detail: res.message });
        return null;
      }
      toast.push({ tone: "ok", title: success, detail: opts.detail });
      if (opts.refresh !== false) router.refresh();
      return res as never;
    } finally {
      setBusy(false);
    }
  }
  return { run, busy, error, clear: () => setError(null) };
}

/** The server's reason under a form. An UNIMPLEMENTED answer says so plainly. */
export function ActionError({ error }: { error: ActionFailure | null }) {
  if (!error) return null;
  return (
    <p role="alert" data-testid="action-error" className="text-sm text-bad [overflow-wrap:anywhere]">
      {error.unimplemented ? "Not available on this server. " : ""}
      {error.message} <span className="mono text-xs text-ink-3">reason: {error.reason}</span>
    </p>
  );
}

/**
 * A button that asks first. The dialog names what will happen; the confirm button does it. Both
 * buttons are marked for agents (`agentConfirm`, by default the dialog's question), so an agent
 * driving the page cannot answer for the operator.
 */
export function ConfirmButton({
  label,
  title,
  description,
  confirmLabel,
  agentConfirm,
  variant = "danger",
  size = "sm",
  disabled,
  onConfirm,
  children,
}: {
  label: string;
  title: string;
  description: ReactNode;
  confirmLabel: string;
  /** What confirming does, as a sentence for agents. Defaults to the confirm label and the question. */
  agentConfirm?: string;
  variant?: "danger" | "primary" | "secondary";
  size?: "sm" | "md";
  disabled?: boolean;
  onConfirm: () => void | Promise<void>;
  children?: ReactNode;
}) {
  const [open, setOpen] = useState(false);
  const sentence = agentConfirm ?? confirmLabel + ". " + title;
  const consequence = variant === "danger" ? "destructive" : "approving";
  return (
    <>
      <MarkedButton agentConfirm={sentence} consequence={consequence} size={size} variant={variant} disabled={disabled} onClick={() => setOpen(true)}>
        {label}
      </MarkedButton>
      <Modal open={open} onOpenChange={setOpen} title={title} description={description} hideClose className="top-[12vh] w-[min(460px,calc(100vw-24px))]">
        {children}
        <div className="mt-5 flex justify-end gap-2.5">
          <Button onClick={() => setOpen(false)}>Cancel</Button>
          <MarkedButton
            agentConfirm={sentence}
            consequence={consequence}
            variant={variant === "danger" ? "danger" : "primary"}
            onClick={async () => {
              setOpen(false);
              await onConfirm();
            }}
          >
            {confirmLabel}
          </MarkedButton>
        </div>
      </Modal>
    </>
  );
}

/** A labelled checkbox row. */
export function Check({ id, label, hint, checked, onChange, disabled }: { id: string; label: string; hint?: ReactNode; checked: boolean; onChange: (v: boolean) => void; disabled?: boolean }) {
  return (
    <div className="flex items-start gap-2.5">
      <input id={id} type="checkbox" checked={checked} disabled={disabled} onChange={(e) => onChange(e.target.checked)} className="mt-0.5 size-4 accent-[var(--accent)]" />
      <div className="min-w-0">
        <label htmlFor={id} className="text-sm font-medium">
          {label}
        </label>
        {hint ? <p className="text-xs text-ink-3">{hint}</p> : null}
      </div>
    </div>
  );
}

/** A value to read out and copy: the SAML service-provider URLs, a SCIM endpoint, a DNS record. */
export function CopyValue({ label, value }: { label: string; value: string }) {
  const toast = useToast();
  return (
    <div className="flex items-center gap-2">
      <span className="w-32 shrink-0 text-sm text-ink-3">{label}</span>
      <span className="mono min-w-0 flex-1 truncate text-sm" title={value} data-value>
        {value || "—"}
      </span>
      {value ? (
        <Button
          size="sm"
          variant="ghost"
          aria-label={"Copy " + label}
          onClick={async () => {
            await navigator.clipboard.writeText(value);
            toast.push({ tone: "ok", title: "Copied " + label });
          }}
        >
          Copy
        </Button>
      ) : null}
    </div>
  );
}

/** The set of fields that changed between what the server holds and what the form says. */
export function changed<T extends object>(before: T, after: T): Partial<T> {
  const out: Partial<T> = {};
  for (const k of Object.keys(after) as (keyof T)[]) if (before[k] !== after[k]) out[k] = after[k];
  return out;
}

export const formClass = "flex flex-col gap-3.5 px-3.5 py-3.5";
export const actionsRow = "flex flex-wrap items-center gap-2";
export { cx };
