"use client";

import { useRouter } from "next/navigation";
import { useEffect, useState } from "react";
import { Refusal } from "@/components/bees/pieces";
import { browserClient } from "@/lib/api/browser";
import type { Verdict } from "@/lib/api/types";
import { decisionComment } from "@/lib/asks";
import { CopyAs } from "./copy-as";
import { Icon } from "./icons";
import { ConfirmButton } from "./kit/confirm";
import { useToast } from "./toast";
import { Button, Field, Textarea, cx } from "./ui";
import { LINEAR_MESSAGE, LINEAR_REMEDY, LINEAR_REQUIREMENT } from "@/lib/infra";
import { repoHref } from "@/lib/routes";

/**
 * A merge refused by a `require_linear` rule (SPEC §7.4): in words first, then the raw
 * requirement token an agent matches on, then what to do about it.
 */
export function LinearRefusal({ eventKey }: { eventKey?: string }) {
  return (
    <div role="alert" data-testid="linear-refusal" className="rounded-md bg-bad-soft px-3 py-2.5 text-sm">
      {eventKey ? <Refusal eventKey={eventKey} text="The target branch requires linear history." /> : null}
      <p className="flex items-start gap-2 font-medium text-bad">
        <Icon name="x" size={14} className="mt-0.5 shrink-0" />
        {LINEAR_MESSAGE}
      </p>
      <p className="mono mt-1.5 text-xs text-ink-3">POLICY_DENIED · {LINEAR_REQUIREMENT}</p>
      <p className="mt-1 text-ink-2">{LINEAR_REMEDY}</p>
    </div>
  );
}

/** The confirmed rule and its text are rendered together. Only the illustration moves. */
export function MergeRefusal({ eventKey, text, reason }: { eventKey: string; text: string; reason?: string }) {
  return (
    <div data-testid="merge-refusal" className="flex items-start gap-2 text-base text-ink-2">
      <Refusal eventKey={eventKey} text={text} />
      <p>{text}{reason ? <> <span className="mono text-xs">{reason}</span></> : null}</p>
    </div>
  );
}

const VERDICTS: { v: Verdict; label: string; hint: string }[] = [
  { v: "VERDICT_COMMENT", label: "Comment", hint: "Feedback without a verdict" },
  { v: "VERDICT_APPROVE", label: "Approve", hint: "Counts toward the rules for merging" },
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
        <ConfirmButton
          agentConfirm={"Submits a review on revision " + revision + " of this pull request in " + repo + ", with the verdict chosen above: approving counts toward merging it"}
          consequence="approving"
          variant={verdict === "VERDICT_APPROVE" ? "primary" : "secondary"}
          className="flex-1"
          disabled={busy}
          onClick={submit}
        >
          {busy ? "Submitting…" : "Submit review"}
        </ConfirmButton>
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

/**
 * The brief's ask as decision buttons (docs/spec/web-ui.md §3 "Needs you"): choosing an option
 * posts the comment `Decision on "<question>": <option>`, which agents read like any comment. The
 * current answer is pressed (a tick and a tinted ground, not solid: the screen's one solid button
 * is the decision under the title); choosing another posts a new decision, which replaces it.
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
          <Button key={o} size="sm" className={on ? "bg-selected" : undefined} aria-pressed={on} disabled={disabled || busy !== null} onClick={() => (on ? undefined : decide(o))} icon={on ? "check" : undefined}>
            {busy === o ? "Posting\u2026" : o}
          </Button>
        );
      })}
    </div>
  );
}
