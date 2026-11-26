/**
 * What the author asks of a person, under "Why you" (docs/spec/web-ui.md §3, "A pull request"):
 * the brief's asks as decision buttons, and what the author still owes from the checklist, in
 * words. Nudges, never gates: nothing here blocks merging. Approving, requesting changes and
 * closing are the buttons under the title (components/pull/decisions.tsx).
 */
import Link from "next/link";
import type { BriefChecklist, ChangeRequest, Comment, Principal } from "@/lib/api/types";
import { askStates } from "@/lib/asks";
import { listing } from "@/lib/checkwords";
import { DecisionButtons } from "../change-actions";
import { Icon } from "../icons";
import { Time } from "../time";

export interface NeedsYouProps {
  repo: string;
  change: ChangeRequest;
  comments: Comment[];
  checklist?: BriefChecklist;
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

export function NeedsYou({ repo, change, comments, checklist, viewer, signInNext }: NeedsYouProps) {
  const mine = !!viewer && change.author?.id === viewer.id;
  const asks = askStates(change.brief?.asks ?? [], comments);
  const owed = owedSentence(checklist, change.author);
  if (change.state !== "CHANGE_STATE_OPEN" || (!asks.length && !owed)) return null;
  return (
    <div className="flex flex-col gap-6" data-needs-you>
      {asks.map((s) => (
        <div key={s.ask.question} className="flex flex-col gap-2" data-ask>
          <p className="text-lg font-light text-ink">{s.ask.question}</p>
          {viewer && !mine ? (
            <DecisionButtons repo={repo} crId={change.id} question={s.ask.question} options={s.ask.options} chosen={s.answer} />
          ) : (
            <p className="text-base text-ink-2">Options: {listing(s.ask.options.map((o) => "“" + o + "”"))}.</p>
          )}
          {s.answer !== undefined ? (
            <p className="flex items-start gap-1.5 text-sm text-ink-2" data-decided>
              <Icon name="check" size={14} className="mt-0.5" />
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
            <p className="text-sm text-ink-2">
              <Link href={"/signin" + (signInNext ? "?next=" + encodeURIComponent(signInNext) : "")} className="link">
                Sign in
              </Link>{" "}
              to decide.
            </p>
          ) : mine ? (
            <p className="text-sm text-ink-2">Waiting for someone to decide.</p>
          ) : null}
        </div>
      ))}
      {owed ? (
        <p className="text-base text-ink" data-owed>
          {owed}
        </p>
      ) : null}
    </div>
  );
}
