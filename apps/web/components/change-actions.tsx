"use client";

import { useRouter } from "next/navigation";
import { useEffect, useState } from "react";
import { browserClient } from "@/lib/api/browser";
import type { ChangeState, LandingEvaluation, QueueEntry, Verdict } from "@/lib/api/types";
import { decisionComment } from "@/lib/asks";
import { CopyAs } from "./copy-as";
import { Icon } from "./icons";
import { useToast } from "./toast";
import { Badge, Button, Field, Textarea, cx } from "./ui";
import { LINEAR_MESSAGE, LINEAR_REMEDY, LINEAR_REQUIREMENT, isLinearRefusal, refusesMerge } from "@/lib/infra";
import { repoHref } from "@/lib/routes";

/** Explains a policy requirement in plain words; the raw token stays visible for agents. */
export function explainRequirement(s: string): string {
  if (s.startsWith("check:")) return "Check " + s.slice(6);
  if (s.startsWith("approval:owner:")) return "Owner approval for " + s.slice(15);
  if (s.startsWith("approval:human:default")) return "One human approval (agent change outside the agent-landable classes)";
  if (s.startsWith("approval:human:")) return "Human approvals for a " + s.slice(15);
  if (s.startsWith("approval:any")) return "One approval " + s.slice(12);
  if (s.startsWith("agent-may-land:")) return "Agents may land " + s.slice(15) + " changes on their own";
  if (s.startsWith("lease:exclusive:")) return "Author holds the exclusive lease on " + s.slice(16);
  if (s.startsWith("stack:")) return s.slice(7);
  if (s.startsWith("state:")) return "Change is " + s.slice(7);
  if (s === LINEAR_REQUIREMENT) return LINEAR_MESSAGE;
  return s;
}

/**
 * A land refused by a `require_linear` rule (SPEC §7.4): in words first, then the raw requirement
 * token an agent matches on, then what to do about it.
 */
export function LinearRefusal() {
  return (
    <div role="alert" data-testid="linear-refusal" className="rounded-md bg-bad-soft px-3 py-2.5 text-sm">
      <p className="flex items-start gap-2 font-medium text-bad">
        <Icon name="x" size={14} className="mt-0.5 shrink-0" />
        {LINEAR_MESSAGE}
      </p>
      <p className="mono mt-1.5 text-xs text-ink-3">POLICY_DENIED · {LINEAR_REQUIREMENT}</p>
      <p className="mt-1 text-ink-2">{LINEAR_REMEDY}</p>
    </div>
  );
}

export function LandPanel({ repo, crId, state, evaluation, queue }: { repo: string; crId: string; state: ChangeState; evaluation: LandingEvaluation; queue: QueueEntry | null }) {
  const router = useRouter();
  const toast = useToast();
  const [busy, setBusy] = useState(false);
  const [linearRefused, setLinearRefused] = useState(false);
  const r = { fullName: repo };
  const op = { service: "ChangeService" as const, method: "Enqueue", request: { repo: r, changeRequestId: crId }, label: "Land through the merge queue" };
  const evalOp = { service: "ChangeService" as const, method: "EvaluateLanding", request: { repo: r, changeRequestId: crId }, label: "Ask policy whether this can land" };

  async function land() {
    setBusy(true);
    try {
      const q = await browserClient().change.enqueue(op.request);
      toast.push({ tone: "ok", title: "Queued at position " + q.position, detail: "Policy is evaluated again at the front of the queue." });
      router.refresh();
    } catch (e) {
      if (isLinearRefusal(e)) setLinearRefused(true);
      else toast.error(e, "Cannot land yet");
      router.refresh();
    } finally {
      setBusy(false);
    }
  }

  if (state === "CHANGE_STATE_LANDED")
    return (
      <div className="flex items-center gap-2 rounded-md bg-ok-soft px-3 py-2.5 text-sm text-ok">
        <Icon name="check" /> Landed on main.
      </div>
    );
  if (state === "CHANGE_STATE_CLOSED") return <p className="text-sm text-ink-2">Closed without landing.</p>;

  return (
    <div className="flex flex-col gap-3" data-testid="land-panel">
      <div className="flex items-center gap-2">
        {queue ? (
          <Badge tone="accent">
            <Icon name="queue" size={12} /> In queue · #{queue.position} · {queue.state}
          </Badge>
        ) : evaluation.allowed ? (
          <Badge tone="ok">
            <Icon name="check" size={12} /> Policy allows landing
          </Badge>
        ) : (
          <Badge tone="bad">
            <Icon name="lock" size={12} /> Blocked by policy
          </Badge>
        )}
        <CopyAs compact op={evalOp} className="ml-auto" />
      </div>
      {linearRefused || refusesMerge(evaluation.missing) ? <LinearRefusal /> : null}
      {evaluation.missing.filter((m) => m !== LINEAR_REQUIREMENT).length ? (
        <div>
          <p className="mb-1 text-xs text-ink-3">Still needed</p>
          <ul className="flex flex-col gap-1">
            {evaluation.missing.filter((m) => m !== LINEAR_REQUIREMENT).map((m) => (
              <li key={m} className="flex items-start gap-2 text-sm">
                <Icon name="x" size={14} className="mt-0.5 shrink-0 text-bad" />
                <span>
                  {explainRequirement(m)}
                  <span className="mono block text-xs text-ink-3">{m}</span>
                </span>
              </li>
            ))}
          </ul>
        </div>
      ) : null}
      {evaluation.satisfied.length ? (
        <details className="group">
          <summary className="cursor-pointer text-xs text-ink-3 hover:text-ink-2">{evaluation.satisfied.length} requirements met</summary>
          <ul className="mt-1 flex flex-col gap-1">
            {evaluation.satisfied.map((m) => (
              <li key={m} className="flex items-start gap-2 text-sm text-ink-2">
                <Icon name="check" size={14} className="mt-0.5 shrink-0 text-ok" />
                {explainRequirement(m)}
              </li>
            ))}
          </ul>
        </details>
      ) : null}
      <div className="flex items-center gap-1.5">
        <Button variant="primary" className="flex-1" icon="queue" disabled={!evaluation.allowed || !!queue || busy} onClick={land} title={!evaluation.allowed ? "Blocked: " + evaluation.missing.join(", ") : undefined}>
          {queue ? "Queued" : busy ? "Queuing…" : "Land via merge queue"}
        </Button>
        <CopyAs compact op={op} />
      </div>
    </div>
  );
}

const VERDICTS: { v: Verdict; label: string; hint: string }[] = [
  { v: "VERDICT_COMMENT", label: "Comment", hint: "Feedback without a verdict" },
  { v: "VERDICT_APPROVE", label: "Approve", hint: "Counts toward land rules if policy says so" },
  { v: "VERDICT_REQUEST_CHANGES", label: "Request changes", hint: "Blocks until a new revision" },
];

export function ReviewForm({ repo, crId, revision, viewing }: { repo: string; crId: string; revision: number; viewing: number }) {
  const router = useRouter();
  const toast = useToast();
  const [verdict, setVerdict] = useState<Verdict>("VERDICT_COMMENT");
  const [body, setBody] = useState("");
  const [busy, setBusy] = useState(false);
  const req = { repo: { fullName: repo }, changeRequestId: crId, revision, verdict, body, comments: [] };
  async function submit() {
    setBusy(true);
    try {
      await browserClient().change.submitReview(req);
      toast.push({ tone: "ok", title: "Review submitted on r" + revision });
      setBody("");
      router.refresh();
    } catch (e) {
      toast.error(e, "Review not submitted");
    } finally {
      setBusy(false);
    }
  }
  return (
    <div className="flex flex-col gap-3" data-testid="review-form">
      {viewing !== revision ? <p className="rounded-md bg-warn-soft px-2.5 py-1.5 text-sm text-warn">You are viewing r{viewing}. Reviews always attach to the latest revision, r{revision}.</p> : null}
      <fieldset>
        <legend className="mb-1.5 text-sm font-medium">Verdict on r{revision}</legend>
        <div className="grid grid-cols-3 gap-1 rounded-md border border-line p-0.5">
          {VERDICTS.map((x) => (
            <label key={x.v} title={x.hint} className={cx("flex h-7 cursor-pointer items-center justify-center rounded-sm px-1 text-center text-sm", verdict === x.v ? "bg-hover font-medium text-ink" : "text-ink-2 hover:text-ink")}>
              <input type="radio" name={"verdict-" + crId} value={x.v} checked={verdict === x.v} onChange={() => setVerdict(x.v)} className="sr-only" />
              {x.label}
            </label>
          ))}
        </div>
      </fieldset>
      <Field label="Summary" htmlFor={"review-body-" + crId}>
        <Textarea id={"review-body-" + crId} value={body} onChange={(e) => setBody(e.target.value)} placeholder="What you checked and what you want changed." />
      </Field>
      <div className="flex items-center gap-1.5">
        <Button variant={verdict === "VERDICT_APPROVE" ? "primary" : "secondary"} className="flex-1" disabled={busy} onClick={submit}>
          {busy ? "Submitting…" : "Submit review"}
        </Button>
        <CopyAs compact op={{ service: "ChangeService", method: "SubmitReview", request: req, label: "Submit this review" }} />
      </div>
    </div>
  );
}

/** `[` and `]` step through revisions. */
export function RevisionKeys({ repo, crId, revision, latest }: { repo: string; crId: string; revision: number; latest: number }) {
  const router = useRouter();
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const t = e.target as HTMLElement | null;
      if (e.metaKey || e.ctrlKey || (t && /^(INPUT|TEXTAREA|SELECT)$/.test(t.tagName)) || document.querySelector("[role=dialog]")) return;
      if (e.key === "[" && revision > 1) router.push(repoHref(repo, "changes/" + crId + "/code?rev=" + (revision - 1)));
      if (e.key === "]" && revision < latest) router.push(repoHref(repo, "changes/" + crId + "/code?rev=" + (revision + 1)));
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [repo, crId, revision, latest, router]);
  return null;
}

export function CloseChange({ repo, crId }: { repo: string; crId: string }) {
  const router = useRouter();
  const toast = useToast();
  const req = { repo: { fullName: repo }, changeRequestId: crId };
  return (
    <Button
      size="sm"
      variant="ghost"
      onClick={async () => {
        if (!window.confirm("Close this change request without landing it?")) return;
        try {
          await browserClient().change.closeChange({ repo: req.repo, id: crId, reason: "closed from the web" });
          router.refresh();
        } catch (e) {
          toast.error(e);
        }
      }}
    >
      Close
    </Button>
  );
}

/**
 * The brief's ask as decision buttons (docs/spec/web-ui.md §3 "Needs you"): choosing an option
 * posts the comment `Decision on "<question>": <option>`, which agents read like any comment. The
 * current answer is pressed; choosing another posts a new decision, which replaces it.
 */
export function DecisionButtons({ repo, crId, question, options, chosen, disabled = false }: { repo: string; crId: string; question: string; options: string[]; chosen?: string; disabled?: boolean }) {
  const router = useRouter();
  const toast = useToast();
  const [busy, setBusy] = useState<string | null>(null);
  async function decide(option: string) {
    setBusy(option);
    try {
      await browserClient().change.commentChange({ repo: { fullName: repo }, changeRequestId: crId, body: decisionComment(question, option) });
      toast.push({ tone: "ok", title: "Decision posted", detail: "\u201c" + option + "\u201d" });
      router.refresh();
    } catch (e) {
      toast.error(e, "Decision not posted");
    } finally {
      setBusy(null);
    }
  }
  return (
    <div role="group" aria-label={question} className="flex flex-wrap gap-2" data-decision>
      {options.map((o) => {
        const on = chosen !== undefined && o.toLowerCase() === chosen.toLowerCase();
        return (
          <Button key={o} size="sm" variant={on ? "primary" : "secondary"} aria-pressed={on} disabled={disabled || busy !== null} onClick={() => (on ? undefined : decide(o))} icon={on ? "check" : undefined}>
            {busy === o ? "Posting\u2026" : o}
          </Button>
        );
      })}
    </div>
  );
}

/**
 * Approve, or ask for changes with a short note, on the latest revision: the two verdicts a
 * supervisor gives from the Brief tab. The full review form, with comments, is the Code tab's.
 */
export function ReviewButtons({ repo, crId, revision, approved = false }: { repo: string; crId: string; revision: number; approved?: boolean }) {
  const router = useRouter();
  const toast = useToast();
  const [asking, setAsking] = useState(false);
  const [note, setNote] = useState("");
  const [busy, setBusy] = useState(false);
  async function submit(verdict: Verdict, body: string) {
    setBusy(true);
    try {
      await browserClient().change.submitReview({ repo: { fullName: repo }, changeRequestId: crId, revision, verdict, body, comments: [] });
      toast.push({ tone: "ok", title: verdict === "VERDICT_APPROVE" ? "Approved r" + revision : "Asked for changes on r" + revision });
      setAsking(false);
      setNote("");
      router.refresh();
    } catch (e) {
      toast.error(e, "Review not submitted");
    } finally {
      setBusy(false);
    }
  }
  const noteId = "changes-note-" + crId;
  return (
    <div className="flex flex-col gap-3" data-review-buttons>
      <div className="flex flex-wrap gap-2">
        <Button variant="primary" size="sm" icon="check" disabled={busy || approved} onClick={() => submit("VERDICT_APPROVE", "")}>
          {approved ? "Approved r" + revision : "Approve r" + revision}
        </Button>
        <Button variant="secondary" size="sm" disabled={busy} aria-expanded={asking} aria-controls={noteId} onClick={() => setAsking((v) => !v)}>
          Ask for changes
        </Button>
      </div>
      {asking ? (
        <form
          id={noteId}
          className="flex flex-col gap-2"
          onSubmit={(e) => {
            e.preventDefault();
            if (note.trim()) void submit("VERDICT_REQUEST_CHANGES", note.trim());
          }}
        >
          <Field label="What should change?" htmlFor={noteId + "-text"} hint="The author, person or agent, reads this before revising.">
            <Textarea id={noteId + "-text"} value={note} onChange={(e) => setNote(e.target.value)} rows={3} />
          </Field>
          <div className="flex gap-2">
            <Button type="submit" size="sm" variant="primary" disabled={busy || !note.trim()}>
              {busy ? "Sending\u2026" : "Send"}
            </Button>
            <Button size="sm" variant="ghost" onClick={() => setAsking(false)}>
              Cancel
            </Button>
          </div>
        </form>
      ) : null}
    </div>
  );
}

/** Land through the merge queue, shown only when policy allows it. */
export function LandButton({ repo, crId }: { repo: string; crId: string }) {
  const router = useRouter();
  const toast = useToast();
  const [busy, setBusy] = useState(false);
  const [linearRefused, setLinearRefused] = useState(false);
  async function land() {
    setBusy(true);
    try {
      const q = await browserClient().change.enqueue({ repo: { fullName: repo }, changeRequestId: crId });
      toast.push({ tone: "ok", title: "Queued at position " + q.position, detail: "Policy is checked again at the front of the queue." });
      router.refresh();
    } catch (e) {
      if (isLinearRefusal(e)) setLinearRefused(true);
      else toast.error(e, "Cannot land yet");
      router.refresh();
    } finally {
      setBusy(false);
    }
  }
  return (
    <div className="flex flex-col gap-2">
      <div>
        <Button variant="primary" size="sm" icon="merge" disabled={busy} onClick={land}>
          {busy ? "Queuing\u2026" : "Land"}
        </Button>
      </div>
      {linearRefused ? <LinearRefusal /> : null}
    </div>
  );
}
