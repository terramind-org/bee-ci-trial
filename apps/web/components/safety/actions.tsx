"use client";

import { useRouter } from "next/navigation";
import { useId, useRef, useState } from "react";
import { askPerson, ConfirmButton, Field, Select, Textarea } from "@/components/kit";
import { useToast } from "@/components/toast";
import { browserClient } from "@/lib/api/browser";
import type { RepoRef, SafetyRef, SafetyVerdict } from "@/lib/api/types";
import { RESUME_DESCRIPTION, STOP_DESCRIPTION, VERDICTS } from "./model";

export type Reviewer = { id: string; label: string };

type DecisionProps = {
  label: string;
  description: string;
  destructive?: boolean;
  disabled?: boolean;
  run: () => Promise<unknown>;
};

/** A human decision has the same confirmation and failure behaviour wherever it appears. */
function Decision({ label, description, destructive = false, disabled, run }: DecisionProps) {
  const router = useRouter();
  const toast = useToast();
  const [busy, setBusy] = useState(false);
  const pending = useRef(false);

  async function decide() {
    if (pending.current || disabled) return;
    pending.current = true;
    setBusy(true);
    try {
      if (!await askPerson({ title: label + "?", description, confirmLabel: label })) return;
      await run();
      router.refresh();
    } catch (error) {
      toast.error(error, "Could not save the decision");
    } finally {
      pending.current = false;
      setBusy(false);
    }
  }

  return <ConfirmButton size="sm" variant="secondary" disabled={disabled || busy} agentConfirm={description} consequence={destructive ? "destructive" : "approving"} onClick={decide}>{label}</ConfirmButton>;
}

function Note({ id, label, value, onChange, maxLength }: { id: string; label: string; value: string; onChange: (value: string) => void; maxLength: number }) {
  return <Field htmlFor={id} label={label}>
    <Textarea id={id} value={value} onChange={(event) => onChange(event.currentTarget.value)} maxLength={maxLength} rows={2} />
  </Field>;
}

export function FindingActions({ work, findingId }: { work: SafetyRef; findingId: string }) {
  const id = useId();
  const [reason, setReason] = useState("");
  const resolve = (state: "SAFETY_FINDING_STATE_DISMISSED" | "SAFETY_FINDING_STATE_CONFIRMED") =>
    browserClient().safety.resolveFinding({ work, findingId, state, reason: reason.trim() });
  return <div className="mt-3 grid max-w-md gap-2">
    <Note id={id} label="Reason for confirming or dismissing" value={reason} onChange={setReason} maxLength={2000} />
    <div className="flex flex-wrap gap-2">
      <Decision label="Dismiss" description="Dismisses this finding and records your reason." disabled={!reason.trim()} run={() => resolve("SAFETY_FINDING_STATE_DISMISSED")} />
      <Decision label="Confirm" description="Confirms this finding and records your reason." disabled={!reason.trim()} run={() => resolve("SAFETY_FINDING_STATE_CONFIRMED")} />
    </div>
  </div>;
}

export function VerdictAction({ work, current }: { work: SafetyRef; current: SafetyVerdict }) {
  const id = useId();
  const [value, setValue] = useState(current);
  const [note, setNote] = useState("");
  return <div className="grid max-w-md justify-items-start gap-3">
    <div className="grid w-full gap-3">
      <Field htmlFor={id} label="Verdict">
        <Select id={id} value={value} onChange={(event) => setValue(event.currentTarget.value as SafetyVerdict)}>
          <option value="SAFETY_VERDICT_UNSPECIFIED" disabled>Choose a verdict</option>
          {VERDICTS.map(([key, name]) => <option key={key} value={key}>{name}</option>)}
        </Select>
      </Field>
      <Note id={id + "-note"} label="Your note" value={note} onChange={setNote} maxLength={2000} />
    </div>
    <Decision label="Set verdict" description="Sets your verdict on this work and records your note." disabled={!note.trim() || value === "SAFETY_VERDICT_UNSPECIFIED"} run={async () => {
      await browserClient().safety.setVerdict({ work, verdict: value, note: note.trim() });
      setNote("");
    }} />
  </div>;
}

export function ReviewAction({ work, reviewers }: { work: SafetyRef; reviewers: Reviewer[] }) {
  const id = useId();
  const [reviewerId, setReviewer] = useState("");
  if (!reviewers.length) return <p className="text-base text-ink-2">No independent reviewer is available in this organisation.</p>;
  return <div className="grid max-w-md justify-items-start gap-3">
    <div className="w-full">
      <Field htmlFor={id} label="Independent reviewer">
        <Select id={id} value={reviewerId} onChange={(event) => setReviewer(event.currentTarget.value)}>
          <option value="" disabled>Choose a reviewer</option>
          {reviewers.map(({ id, label }) => <option key={id} value={id}>{label}</option>)}
        </Select>
      </Field>
    </div>
    <Decision label="Ask for an independent review" description="Sends a review request to the independent reviewer you chose." disabled={!reviewers.some((r) => r.id === reviewerId)} run={async () => {
      await browserClient().safety.requestSafetyReview({ work, reviewerId });
      setReviewer("");
    }} />
  </div>;
}

export function AcknowledgeAction({ repo, id }: { repo: RepoRef; id: string }) {
  return <Decision label="Acknowledge" description="Removes this item from your attention list and records that you saw it." run={() => browserClient().safety.acknowledgeSafetyAttention({ repo, id })} />;
}

export function StopAction({ org, frozen }: { org: string; frozen: boolean }) {
  const id = useId();
  const [reason, setReason] = useState("");
  const label = frozen ? "Resume" : "Stop everything";
  return <div className="grid max-w-md justify-items-start gap-3">
    <div className="w-full">
      <Note id={id} label={frozen ? "Reason for resuming" : "Reason for stopping"} value={reason} onChange={setReason} maxLength={2000} />
    </div>
    <Decision label={label} description={frozen ? RESUME_DESCRIPTION : STOP_DESCRIPTION} destructive={!frozen} disabled={!reason.trim()} run={async () => {
      const api = browserClient().safety;
      const input = { org, reason: reason.trim() };
      if (frozen) await api.resumeEverything(input);
      else await api.stopEverything(input);
      setReason("");
    }} />
  </div>;
}
