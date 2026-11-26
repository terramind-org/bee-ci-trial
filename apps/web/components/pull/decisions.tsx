"use client";

/**
 * The decisions on a pull request, under its title (docs/spec/web-ui.md §3, "A pull request"):
 * Approve and merge, Request changes, Close. One solid button; the other two are outlines. Each
 * goes through the kit's `ConfirmButton`, so an agent driving the page cannot press one: the
 * step waits for a person.
 *
 * Approve and merge is two calls in order: the approval (`SubmitReview`), then, when the rules
 * allow it after that approval, the merge queue (`Enqueue`). When they still do not, the approval
 * stands and the page says what remains. Nothing is merged around the rules.
 */
import Link from "next/link";
import { useRouter } from "next/navigation";
import { useState } from "react";
import { browserClient } from "@/lib/api/browser";
import { BeeError } from "@/lib/api/transport";
import { listing } from "@/lib/checkwords";
import { refusesMerge } from "@/lib/infra";
import type { PullDecisions } from "@/lib/pull-page";
import { missingInWords } from "@/lib/status";
import { LinearRefusal, MergeRefusal } from "../change-actions";
import { Button } from "../kit/button";
import { ConfirmButton, askPerson } from "../kit/confirm";
import { Field, Textarea } from "../kit/field";
import { useToast } from "../toast";

let refusalNumber = 0;

interface RefusedMerge {
  eventKey: string;
  text: string;
  reason?: string;
  linear: boolean;
}

export function PullDecisionButtons({ repo, crId, number, revision, target, decisions, signInHref }: { repo: string; crId: string; number: string; revision: number; target: string; decisions: PullDecisions; signInHref?: string }) {
  const router = useRouter();
  const toast = useToast();
  const [busy, setBusy] = useState(false);
  const [asking, setAsking] = useState(false);
  const [note, setNote] = useState("");
  const [refusal, setRefusal] = useState<RefusedMerge | null>(null);
  const ref = { fullName: repo };
  const name = "pull request #" + number + " in " + repo;

  function refused(missing: string[], reason?: string, message?: string) {
    const text = missing.length ? "Not merged yet: it " + listing(missing.map(missingInWords)) + "." : message || "The rules did not allow this merge.";
    // Each denial comes from a completed policy evaluation or merge request, never a click.
    setRefusal({ eventKey: repo + "/" + crId + "/refusal/" + ++refusalNumber, text, reason, linear: refusesMerge(missing) });
  }

  async function merge(): Promise<void> {
    try {
      const q = await browserClient().change.enqueue({ repo: ref, changeRequestId: crId });
      toast.push({ tone: "ok", title: "Queued to merge at position " + q.position, detail: "The rules are checked again at the front of the queue." });
    } catch (e) {
      if (!(e instanceof BeeError) || e.reason !== "POLICY_DENIED") throw e;
      refused(e.detail.policyDenial?.missing ?? [], e.reason, e.message);
    }
  }

  async function run(work: () => Promise<void>, failed: string) {
    setBusy(true);
    setRefusal(null);
    try {
      await work();
    } catch (e) {
      toast.error(e, failed);
    } finally {
      setBusy(false);
      router.refresh();
    }
  }

  const approveAndMerge = () =>
    run(async () => {
      await browserClient().change.submitReview({ repo: ref, changeRequestId: crId, revision, verdict: "VERDICT_APPROVE", body: "", comments: [] });
      const after = await browserClient().change.evaluateLanding({ repo: ref, changeRequestId: crId });
      if (after.allowed) return merge();
      toast.push({ tone: "ok", title: "Approved revision " + revision });
      // A concurrent close or merge is a state change, not a refusal by the rules.
      const missing = after.missing.filter((requirement) => !requirement.startsWith("state:"));
      if (missing.length || after.missing.length === 0) refused(missing);
    }, "Not approved");

  const requestChanges = () =>
    run(async () => {
      await browserClient().change.submitReview({ repo: ref, changeRequestId: crId, revision, verdict: "VERDICT_REQUEST_CHANGES", body: note.trim(), comments: [] });
      toast.push({ tone: "ok", title: "Changes requested on revision " + revision });
      setAsking(false);
      setNote("");
    }, "Request not sent");

  const close = async () => {
    if (!(await askPerson({ title: "Close this pull request?", description: "It closes without merging. Nothing is deleted, and it can be reopened.", confirmLabel: "Close it" }))) return;
    await run(async () => {
      await browserClient().change.closeChange({ repo: ref, id: crId, reason: "closed from the web" });
    }, "Not closed");
  };

  const any = decisions.primary || decisions.canRequestChanges || decisions.canClose;
  const noteId = "request-changes-" + crId;
  return (
    <div className="flex flex-col gap-4" data-testid="pull-decisions">
      {any ? (
        <div className="flex flex-wrap gap-2.5">
          {decisions.primary === "approve-and-merge" ? (
            <ConfirmButton agentConfirm={"Approves revision " + revision + " of " + name + " and merges it into " + target + " if the rules then allow it"} consequence="approving" disabled={busy} onClick={approveAndMerge}>
              {busy ? "Working…" : "Approve and merge"}
            </ConfirmButton>
          ) : decisions.primary === "merge" ? (
            <ConfirmButton agentConfirm={"Merges " + name + " into " + target + " through the merge queue"} consequence="approving" disabled={busy} onClick={() => run(merge, "Cannot merge yet")}>
              {busy ? "Working…" : "Merge"}
            </ConfirmButton>
          ) : null}
          {decisions.canRequestChanges ? (
            <Button disabled={busy} aria-expanded={asking} aria-controls={noteId} onClick={() => setAsking((v) => !v)}>
              Request changes
            </Button>
          ) : null}
          {decisions.canClose ? (
            <ConfirmButton agentConfirm={"Closes " + name + " without merging it"} consequence="destructive" variant="secondary" disabled={busy} onClick={close}>
              Close
            </ConfirmButton>
          ) : null}
        </div>
      ) : null}
      {asking ? (
        <form
          id={noteId}
          className="flex max-w-xl flex-col gap-3"
          onSubmit={(e) => {
            e.preventDefault();
            if (note.trim()) void requestChanges();
          }}
        >
          <Field label="What should change?" htmlFor={noteId + "-text"} hint="The author, person or agent, reads this before revising.">
            <Textarea id={noteId + "-text"} value={note} onChange={(e) => setNote(e.target.value)} rows={3} />
          </Field>
          <div className="flex gap-2">
            <ConfirmButton type="submit" size="sm" variant="secondary" agentConfirm={"Sends " + name + " back to its author with a request for changes"} consequence="approving" disabled={busy || !note.trim()}>
              {busy ? "Sending…" : "Send back"}
            </ConfirmButton>
            <Button size="sm" variant="ghost" onClick={() => setAsking(false)}>
              Cancel
            </Button>
          </div>
        </form>
      ) : null}
      {refusal?.linear ? <LinearRefusal eventKey={refusal.eventKey} /> : refusal ? <MergeRefusal eventKey={refusal.eventKey} text={refusal.text} reason={refusal.reason} /> : null}
      <p className="max-w-[44em] text-base text-ink-2" data-testid="decision-note">
        {signInHref && !any ? (
          <>
            <Link href={signInHref} className="link">
              Sign in
            </Link>{" "}
            to approve it, request changes or close it.
          </>
        ) : (
          decisions.note
        )}
      </p>
    </div>
  );
}
