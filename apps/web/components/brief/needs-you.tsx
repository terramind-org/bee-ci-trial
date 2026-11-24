/**
 * "Needs you" (docs/spec/web-ui.md §3 "What do you need from me?"): the brief's asks as decision
 * buttons, approve or ask for changes, Land when policy allows it, and what the author still owes
 * from the checklist, in words. Nudges, never gates: nothing here blocks landing.
 */
import Link from "next/link";
import type { BriefChecklist, ChangeRequest, Comment, LandingEvaluation, Principal, QueueEntry, Review } from "@/lib/api/types";
import { askStates } from "@/lib/asks";
import { listing } from "@/lib/checkwords";
import { DecisionButtons, LandButton, ReviewButtons } from "../change-actions";
import { Icon } from "../icons";
import { Time } from "../time";

export interface NeedsYouProps {
  repo: string;
  change: ChangeRequest;
  comments: Comment[];
  reviews: Review[];
  checklist?: BriefChecklist;
  landing?: LandingEvaluation;
  queue?: QueueEntry | null;
  /** Null when signed out. */
  viewer: Principal | null;
  /** Where "Sign in" goes back to. */
  signInNext?: string;
}

/** "The agent still owes: a screenshot of the UI change and a rollback." */
export function owedSentence(checklist: BriefChecklist | undefined, author: Principal | undefined): string | null {
  const owed = (checklist?.items ?? []).filter((i) => !i.satisfied).map((i) => i.detail);
  if (!owed.length) return null;
  const who = author?.kind === "PRINCIPAL_KIND_AGENT" ? "The agent" : "The author";
  return who + " still owes: " + listing(owed) + ".";
}

export function NeedsYou({ repo, change, comments, reviews, checklist, landing, queue, viewer, signInNext }: NeedsYouProps) {
  const open = change.state === "CHANGE_STATE_OPEN";
  const latest = change.revisions.at(-1)?.n ?? 0;
  const mine = !!viewer && change.author?.id === viewer.id;
  const asks = askStates(change.brief?.asks ?? [], comments);
  const owed = owedSentence(checklist, change.author);
  const approved = !!viewer && reviews.some((r) => r.reviewer?.id === viewer.id && r.verdict === "VERDICT_APPROVE" && r.revision === latest);
  const canReview = open && !!viewer && !mine;
  const canLand = open && !!viewer && !!landing?.allowed && !queue;

  if (!open)
    return (
      <p className="text-base text-ink-2" data-needs-you="none">
        Nothing: this change {change.state === "CHANGE_STATE_LANDED" ? "merged" : "was closed"}.
      </p>
    );

  const nothing = !asks.some((a) => a.answer === undefined) && !owed && !canReview && !canLand && !queue;
  return (
    <div className="flex flex-col gap-6" data-needs-you>
      {asks.map((s) => (
        <div key={s.ask.question} className="flex flex-col gap-2" data-ask>
          <p className="text-lg text-ink">{s.ask.question}</p>
          {viewer && !mine ? (
            <DecisionButtons repo={repo} crId={change.id} question={s.ask.question} options={s.ask.options} chosen={s.answer} />
          ) : (
            <p className="text-base text-ink-2">Options: {listing(s.ask.options.map((o) => "“" + o + "”"))}.</p>
          )}
          {s.answer !== undefined ? (
            <p className="flex items-start gap-1.5 text-sm text-ink-3" data-decided>
              <Icon name="check" size={14} className="mt-0.5 text-ok" />
              <span>
                {"Decided: \u201c" + s.answer + "\u201d" + (s.by ? " by " + (s.by.display || s.by.id) : "")}
                {s.at ? (
                  <>
                    {", "}
                    <Time iso={s.at} />
                  </>
                ) : null}
                {s.offList ? ", which is no longer one of the options" : ""}
              </span>
            </p>
          ) : !viewer ? (
            <p className="text-sm text-ink-3">
              <Link href={"/signin" + (signInNext ? "?next=" + encodeURIComponent(signInNext) : "")} className="text-ink-2 underline hover:text-ink">
                Sign in
              </Link>{" "}
              to decide.
            </p>
          ) : mine ? (
            <p className="text-sm text-ink-3">Waiting for someone to decide.</p>
          ) : null}
        </div>
      ))}

      {owed ? (
        <p className="flex items-start gap-2 text-base text-ink" data-owed>
          <Icon name="clock" size={18} className="mt-0.5 text-warn" />
          <span>{owed}</span>
        </p>
      ) : null}

      {canReview ? <ReviewButtons repo={repo} crId={change.id} revision={latest} approved={approved} /> : null}

      {queue ? (
        <p className="flex items-center gap-2 text-base text-ink-2">
          <Icon name="queue" size={18} />
          In the merge queue at position {queue.position}.
        </p>
      ) : canLand ? (
        <div className="flex flex-col gap-2">
          <p className="text-base text-ink-2">Policy allows landing it now.</p>
          <LandButton repo={repo} crId={change.id} />
        </div>
      ) : null}

      {nothing ? <p className="text-base text-ink-2">Nothing needs you right now.</p> : null}
    </div>
  );
}
