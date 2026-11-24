/**
 * The Brief tab (docs/spec/web-ui.md §3): the five questions a supervisor asks, in order, as one
 * article. What is this (the outcome and a verdict line), what do you need from me, does it work
 * (evidence), is it safe, and who did it at what cost (with the post panel under it). Then what
 * changed, by area, and the conversation. `?rev` and `?since` belong to the Code tab.
 */
import Link from "next/link";
import { redirect } from "next/navigation";
import type { ReactNode } from "react";
import { CheckCircleIcon, ClockIcon, ProhibitIcon, QueueIcon, WarningCircleIcon, XCircleIcon } from "@phosphor-icons/react/dist/ssr";
import { Avatar, toAvatar } from "@/components/avatar";
import { EvidenceGallery } from "@/components/brief/evidence-gallery";
import { HumanChecks } from "@/components/brief/human-checks";
import { NeedsYou } from "@/components/brief/needs-you";
import { PreviewLink } from "@/components/brief/preview-link";
import { Team } from "@/components/brief/team";
import { WhatChanged } from "@/components/brief/what-changed";
import { WorthALook } from "@/components/brief/worth-a-look";
import { PageOp } from "@/components/copy-as";
import { Badge } from "@/components/ui";
import { InspiredByBanner } from "@/components/inspired-by-banner";
import { Markdown } from "@/components/markdown";
import { PostPanelSlot } from "@/components/post/post-panel-slot";
import { Time } from "@/components/time";
import { objectIdText } from "@/lib/api/ids";
import { currentPrincipal, scopeFromParams } from "@/lib/api/server";
import type { Review } from "@/lib/api/types";
import { changeHref, codeRedirect, conversationOf, riskSentence } from "@/lib/change-page";
import { evidenceIndex } from "@/lib/evidence-view";
import { guard, type SearchParams } from "@/lib/page";
import { statusOf, type Status } from "@/lib/status";
import { loadChange, loadChecklist, loadChecks, loadComments, loadDiff, loadEvidence, loadLanding, loadReviews, loadSessions, loadSiblings, type Settled } from "./data";
import { CommentForm } from "./comment-form";

const VERDICT_ICON = { ok: CheckCircleIcon, bad: XCircleIcon, warn: WarningCircleIcon, neutral: ClockIcon, accent: QueueIcon } as const;
const VERDICT_TONE = { ok: "bg-ok-soft text-ok", bad: "bg-bad-soft text-bad", warn: "bg-warn-soft text-warn", neutral: "bg-sunken text-ink-2", accent: "bg-accent-soft text-accent-ink" } as const;

/** Inbox's verdict line: the status sentence, toned, with its glyph. Shape and words carry it, not colour alone. */
function Verdict({ status }: { status: Status }) {
  const Glyph = status.kind === "closed" ? ProhibitIcon : VERDICT_ICON[status.tone];
  return (
    <div className={"flex items-start gap-4 rounded-xl px-5 py-4 " + VERDICT_TONE[status.tone]} data-status={status.kind}>
      <Glyph size={26} weight="fill" aria-hidden="true" className="mt-0.5 shrink-0" />
      <p className="min-w-0 text-ink">
        <span className="block text-lg font-semibold">{status.headline}</span>
        {status.detail ? <span className="block text-base text-ink-2">{status.detail[0]?.toUpperCase() + status.detail.slice(1)}.</span> : null}
      </p>
    </div>
  );
}

/** One of the brief's questions: a section with its heading, Document rhythm. */
function Question({ id, title, children, meta }: { id: string; title: string; children: ReactNode; meta?: ReactNode }) {
  return (
    <section aria-labelledby={id} className="flex min-w-0 flex-col gap-5" data-brief-section={id.replace("brief-", "")}>
      <header className="flex flex-wrap items-baseline justify-between gap-x-4 gap-y-1">
        <h2 id={id} className="text-xl font-semibold tracking-tight text-ink">
          {title}
        </h2>
        {meta ? <span className="text-sm text-ink-3">{meta}</span> : null}
      </header>
      {children}
    </section>
  );
}

/** A supplementary read that failed: its reason, in place of the section's content. */
function Unavailable({ what, settled }: { what: string; settled: Settled<unknown> }) {
  // A signed-out reader of a public repo may not read comments or sessions; that is not a failure
  // worth a sentence, the section simply has less in it.
  return settled.error && settled.error.code !== "unauthenticated" ? (
    <p className="text-base text-ink-2" role="status">
      {what} could not be loaded: <span className="mono text-sm">{settled.error.reason}</span>
    </p>
  ) : null;
}

const REVIEW_WORDS: Record<string, { label: string; tone: "ok" | "bad" | "neutral" }> = {
  VERDICT_APPROVE: { label: "approved", tone: "ok" },
  VERDICT_REQUEST_CHANGES: { label: "asked for changes", tone: "bad" },
  VERDICT_COMMENT: { label: "commented", tone: "neutral" },
};

function ReviewEntry({ r }: { r: Review }) {
  const v = REVIEW_WORDS[r.verdict] ?? { label: "reviewed", tone: "neutral" as const };
  return (
    <>
      <span className="flex flex-wrap items-baseline gap-x-2">
        <span className="font-medium text-ink">{r.reviewer?.display || r.reviewer?.id}</span>
        <Badge tone={v.tone}>{v.label}</Badge>
        <span className="text-sm text-ink-3">
          on r{r.revision} · <Time iso={r.createdAt} />
        </span>
      </span>
      {r.body ? <p className="text-base text-ink">{r.body}</p> : null}
    </>
  );
}

export default async function BriefPage({ params, searchParams }: { params: Promise<{ owner: string; repo: string; id: string }>; searchParams: SearchParams }) {
  const { id } = await params;
  const { fullName } = await scopeFromParams(params);
  const toCode = codeRedirect(fullName, id, await searchParams);
  if (toCode) redirect(toCode);
  return guard(async () => {
    const cr = await loadChange(fullName, id);
    const latest = cr.revisions.at(-1);
    const latestN = latest?.n ?? 0;
    const [{ evaluation, queue }, reviews, checks, diff, evidence, checklist, comments, sessions, siblings, me] = await Promise.all([
      loadLanding(fullName, id, cr.targetRef),
      loadReviews(fullName, id),
      loadChecks(fullName, objectIdText(latest?.head)),
      latestN ? loadDiff(fullName, cr, latestN, 0) : Promise.resolve([]),
      loadEvidence(fullName, id),
      loadChecklist(fullName, id),
      loadComments(fullName, id),
      loadSessions(fullName, id),
      loadSiblings(fullName),
      currentPrincipal(),
    ]);
    const brief = cr.brief;
    const status = statusOf({ change: cr, landing: evaluation, checks, reviews, queue });
    const items = evidence.value ?? [];
    const index = evidenceIndex(items);
    const children = siblings.filter((c) => c.dependsOn === id);
    const here = changeHref(fullName, id);
    const conversation = conversationOf(comments.value ?? [], reviews);
    const getOp = { service: "ChangeService" as const, method: "GetChange", request: { repo: { fullName }, id }, label: "Read this change request" };
    const older = brief?.writtenForRevision && brief.writtenForRevision < latestN ? brief.writtenForRevision : 0;

    return (
      <article className="flex max-w-[760px] flex-col gap-16" data-brief>
        <PageOp op={getOp} />
        {/* web-adopt: the banner loads the source post (excerpt, thumbnail) and hides itself when the reader cannot read it. */}
        <InspiredByBanner inspiredBy={cr.inspiredBy} />

        <section aria-labelledby="brief-what" className="flex flex-col gap-6" data-brief-section="what">
          <h2 id="brief-what" className="sr-only">
            What is this?
          </h2>
          {brief?.outcome ? (
            <p className="text-[22px] leading-[1.45] text-ink md:text-[24px]" data-outcome>
              {brief.outcome}
            </p>
          ) : (
            <p className="text-lg text-ink-2" data-outcome="none">
              No brief yet. The author has not said what this changes in a sentence; the description and what changed are below.
            </p>
          )}
          {older ? (
            <p className="text-sm text-ink-3" data-brief-older>
              The brief was written for r{older}; the change is at r{latestN}.
            </p>
          ) : null}
          <Verdict status={status} />
        </section>

        <Question id="brief-needs" title="Needs you">
          <NeedsYou repo={fullName} change={cr} comments={comments.value ?? []} reviews={reviews} checklist={checklist.value} landing={evaluation} queue={queue} viewer={me} signInNext={here} />
          <Unavailable what="The checklist" settled={checklist} />
        </Question>

        <Question id="brief-works" title="Does it work?" meta={items.length ? items.length + (items.length === 1 ? " piece of evidence" : " pieces of evidence") : undefined}>
          {evidence.error ? (
            <Unavailable what="Evidence" settled={evidence} />
          ) : (
            <EvidenceGallery items={items} repo={fullName} changeId={id} latest={latestN} />
          )}
          {brief?.tryItUrl ? <PreviewLink url={brief.tryItUrl} title="Try it" /> : null}
        </Question>

        <Question id="brief-safe" title="Is it safe?">
          <p className="text-lg text-ink" data-risk>
            {riskSentence(latest?.diffClass ?? "", brief)}
          </p>
          {brief?.risks.length ? (
            <ul className="flex flex-col gap-4" data-risks>
              {brief.risks.map((r, i) => (
                <li key={i} className="flex flex-col gap-1">
                  <span className="text-base text-ink">{r.text}</span>
                  {r.mitigation ? <span className="text-base text-ink-2">Mitigation: {r.mitigation}</span> : <span className="text-sm text-ink-3">No mitigation given.</span>}
                </li>
              ))}
            </ul>
          ) : null}
          {brief?.rollback ? (
            <p className="text-base text-ink-2" data-rollback>
              <span className="font-medium text-ink">Rollback: </span>
              {brief.rollback}
            </p>
          ) : null}
          <div className="flex flex-col gap-4">
            <h3 className="text-lg font-semibold text-ink">Worth a human look</h3>
            <WorthALook diff={diff} highlights={brief?.highlights} repo={fullName} changeId={id} />
          </div>
          <div className="flex flex-col gap-4">
            <h3 className="text-lg font-semibold text-ink">Checks</h3>
            <HumanChecks checks={checks} repo={fullName} />
          </div>
        </Question>

        <Question id="brief-who" title="Who did it">
          <Team change={cr} reviews={reviews} comments={comments.value} sessions={sessions.value} />
          <PostPanelSlot repo={fullName} change={cr} />
        </Question>

        <Question id="brief-changed" title="What changed">
          <WhatChanged diff={diff} highlights={brief?.highlights} repo={fullName} changeId={id} />
          {cr.body.trim() ? (
            <div className="flex flex-col gap-3">
              <h3 className="text-lg font-semibold text-ink">The description</h3>
              <Markdown source={cr.body} repo={fullName} evidence={index} />
            </div>
          ) : null}
          {children.length ? (
            <p className="text-base text-ink-2">
              Stacked on top of this:{" "}
              {children.map((c, i) => (
                <span key={c.id}>
                  {i ? ", " : null}
                  <Link href={changeHref(fullName, c.id)} className="text-ink underline underline-offset-4 hover:decoration-ink">
                    {c.title}
                  </Link>
                </span>
              ))}
            </p>
          ) : null}
        </Question>

        <Question id="brief-conversation" title="Conversation" meta={conversation.length ? conversation.length + (conversation.length === 1 ? " entry" : " entries") : undefined}>
          <Unavailable what="Comments" settled={comments} />
          {conversation.length ? (
            <ol className="flex flex-col divide-y divide-line">
              {conversation.map((c) => {
                const who = c.kind === "comment" ? c.comment.author : c.review.reviewer;
                return (
                  <li key={c.kind + ":" + (c.kind === "comment" ? c.comment.id : c.review.id)} className="flex gap-3 py-4" data-entry={c.kind}>
                    <Avatar principal={toAvatar(who)} size={28} />
                    <div className="flex min-w-0 flex-1 flex-col gap-1.5">
                      {c.kind === "comment" ? (
                        <>
                          <span className="flex flex-wrap items-baseline gap-x-2">
                            <span className="font-medium text-ink">{who?.display || who?.id || "Someone"}</span>
                            <span className="text-sm text-ink-3">
                              <Time iso={c.comment.createdAt} />
                              {c.comment.edited ? " · edited" : ""}
                            </span>
                          </span>
                          <Markdown source={c.comment.body} repo={fullName} evidence={index} />
                        </>
                      ) : (
                        <ReviewEntry r={c.review} />
                      )}
                    </div>
                  </li>
                );
              })}
            </ol>
          ) : (
            <p className="text-base text-ink-2">No comments yet. Review comments on lines are on the Code tab.</p>
          )}
          {cr.state === "CHANGE_STATE_OPEN" ? <CommentForm repo={fullName} crId={id} signedIn={!!me} next={here} /> : null}
        </Question>
      </article>
    );
  });
}
