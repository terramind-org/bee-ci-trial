/**
 * A pull request's overview (docs/spec/web-ui.md §3, "A pull request"): the familiar page,
 * reordered for a reviewer who did not write the code. Under the title, the merge sentence, the
 * person who answers and the decisions (the layout's), one reading column answers in order:
 *
 *   Why you         the rule that sent it to a person, from `LandingEvaluation.missing`
 *   Checks          one line; the checks and the evidence are one click away
 *   Files changed   at most three places worth a person's eyes; the full diff on demand
 *   Commits         what it would merge
 *   Who did it      agent, person, model and runtime, cost, and the prompt digest
 *
 * then the description and the conversation. `?rev` and `?since` belong to Files changed.
 * In the agent's view the same facts are plain text, followed by the tools this page calls.
 */
import Link from "next/link";
import { redirect } from "next/navigation";
import type { ReactNode } from "react";
import { Avatar, toAvatar } from "@/components/avatar";
import { PageFacts } from "@/components/bee/page-facts";
import { EvidenceGallery } from "@/components/brief/evidence-gallery";
import { HumanChecks } from "@/components/brief/human-checks";
import { NeedsYou } from "@/components/brief/needs-you";
import { PreviewLink } from "@/components/brief/preview-link";
import { RuleList } from "@/components/brief/rules";
import { Team } from "@/components/brief/team";
import { WhatChanged } from "@/components/brief/what-changed";
import { WhoDidIt } from "@/components/brief/who-did-it";
import { WorthALook } from "@/components/brief/worth-a-look";
import { PageOp } from "@/components/copy-as";
import { InspiredByBanner } from "@/components/inspired-by-banner";
import { Markdown } from "@/components/markdown";
import { Quiet, Sect, State, type Shape } from "@/components/parts";
import { PostPanelSlot } from "@/components/post/post-panel-slot";
import { CommitRows, commitLine, commitView } from "@/components/repo/commits";
import { pullSafetyLink } from "@/components/safety/pull-record";
import { Time } from "@/components/time";
import { objectIdText } from "@/lib/api/ids";
import { currentPrincipal, scopeFromParams, serverClient } from "@/lib/api/server";
import type { Principal, Review } from "@/lib/api/types";
import { changeHref, codeHref, codeRedirect, conversationOf, riskSentence } from "@/lib/change-page";
import { capitalize, checksSentence, countWord } from "@/lib/checkwords";
import { evidenceIndex } from "@/lib/evidence-view";
import { worthALook } from "@/lib/hunks";
import { guard, type SearchParams } from "@/lib/page";
import { decisionsFor, pullFacts, whoDidIt } from "@/lib/pull-page";
import { answerableFor, otherHolds, whyPerson } from "@/lib/pulls";
import { repoHref } from "@/lib/routes";
import { statusOf, type StatusKind } from "@/lib/status";
import { pageTools } from "@/lib/tools";
import { fromDiff } from "@/lib/whatchanged";
import { requestMode } from "../../../../../request-mode";
import { CommentForm } from "./comment-form";
import { loadBranch, loadChange, loadChecklist, loadChecks, loadComments, loadCommits, loadDiff, loadEvidence, loadHead, loadLanding, loadPeople, loadReviews, loadSessions, loadSiblings, type Settled } from "./data";

const STATE_SHAPE: Record<StatusKind, Shape> = { landed: "merged", closed: "closed", queued: "running", ready: "passed", failing: "failed", checking: "running", review: "waiting", blocked: "waiting" };

/** A supplementary read that failed: its reason, in place of the section's content. */
function Unavailable({ what, settled }: { what: string; settled: Settled<unknown> }) {
  // A signed-out reader of a public repo may not read comments or sessions; that is not a failure
  // worth a sentence, the section simply has less in it.
  return settled.error && settled.error.code !== "unauthenticated" ? (
    <p className="text-base text-ink-2" role="status">
      {what} could not be loaded: <span className="mono">{settled.error.reason}</span>
    </p>
  ) : null;
}

const REVIEW_WORDS: Record<string, string> = { VERDICT_APPROVE: "approved", VERDICT_REQUEST_CHANGES: "requested changes on", VERDICT_COMMENT: "commented on" };

function ReviewEntry({ r }: { r: Review }) {
  return (
    <>
      <span className="flex flex-wrap items-baseline gap-x-2">
        <span className="text-ink">
          <span className="font-medium">{r.reviewer?.display || r.reviewer?.id}</span> {REVIEW_WORDS[r.verdict] ?? "reviewed"} r{r.revision}
        </span>
        <span className="text-xs text-ink-2">
          <Time iso={r.createdAt} />
        </span>
      </span>
      {r.body ? <p className="text-base text-ink">{r.body}</p> : null}
    </>
  );
}

/** Something one click away: a quiet summary that opens in place. */
function More({ summary, children, open, testId }: { summary: ReactNode; children: ReactNode; open?: boolean; testId?: string }) {
  return (
    <details className="group mt-3" open={open} data-testid={testId}>
      <summary className="inline-flex min-h-6 cursor-pointer list-none items-center text-sm text-ink-2 hover:text-ink [&::-webkit-details-marker]:hidden">
        <span className="underline decoration-line-strong underline-offset-[3px]">{summary}</span>
      </summary>
      <div className="mt-4">{children}</div>
    </details>
  );
}

export default async function PullRequestPage({ params, searchParams }: { params: Promise<{ owner: string; repo: string; id: string }>; searchParams: SearchParams }) {
  const { id } = await params;
  const { fullName, owner } = await scopeFromParams(params);
  const toCode = codeRedirect(fullName, id, await searchParams);
  if (toCode) redirect(toCode);
  return guard(async () => {
    const cr = await loadChange(fullName, id);
    const latest = cr.revisions.at(-1);
    const latestN = latest?.n ?? 0;
    const open = cr.state === "CHANGE_STATE_OPEN";
    const [{ evaluation, queue }, reviews, checks, diff, evidence, checklist, comments, sessions, siblings, me, commits, branch, head, { mode }] = await Promise.all([
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
      loadCommits(fullName, cr).catch(() => ({ known: false, commits: [] })),
      loadBranch(fullName, cr),
      // The head's provenance carries the prompt digest; a head that cannot be read has none.
      loadHead(fullName, objectIdText(latest?.head)).catch(() => undefined),
      requestMode(),
    ]);
    // The linked work is supplementary and may be hidden by the Safety evidence scope.
    const [people, safety] = await Promise.all([
      me ? loadPeople(owner) : new Map<string, Principal>(),
      cr.author?.kind === "PRINCIPAL_KIND_AGENT" ? serverClient().then((api) => pullSafetyLink(api, fullName, cr)) : undefined,
    ]);
    const brief = cr.brief;
    const status = statusOf({ change: cr, landing: evaluation, checks, reviews, queue });
    const items = evidence.value ?? [];
    const index = evidenceIndex(items);
    const children = siblings.filter((c) => c.dependsOn === id);
    const here = changeHref(fullName, id);
    const conversation = conversationOf(comments.value ?? [], reviews);
    const getOp = { service: "ChangeService" as const, method: "GetChange", request: { repo: { fullName }, id }, label: "Read this pull request" };
    const older = brief?.writtenForRevision && brief.writtenForRevision < latestN ? brief.writtenForRevision : 0;
    const answers = answerableFor(cr.author, people);
    const who = whoDidIt({ change: cr, answers, sessions: sessions.value, provenance: head?.meta?.provenance, reviews, comments: comments.value });
    // When the server cannot list the commits, the rows are the revisions' heads: say which.
    const byRevision = !commits.known && commits.commits.length === cr.revisions.length;
    const commitViews = commits.commits.map((c, i) => ({ ...commitView(c, people), note: byRevision ? "r" + cr.revisions[i]?.n : undefined }));
    const rules = whyPerson(evaluation.missing);
    const holds = otherHolds(evaluation.missing);
    const files = fromDiff(diff);
    const picks = worthALook(diff, { highlights: brief?.highlights });

    if (mode === "bee") {
      const groups = pullFacts({
        repo: fullName,
        change: cr,
        status,
        branch,
        answers,
        commits: { known: commits.known, lines: commitViews.map(commitLine) },
        landing: open ? evaluation : undefined,
        checks,
        evidence: items,
        diff,
        reviews,
        comments: comments.value ?? [],
        checklist: checklist.value,
        who,
        decisions: decisionsFor({ change: cr, viewer: me, reviews, landing: evaluation, queue }),
        href: here,
        changeMdHref: repoHref(fullName, "changes/" + id + "/change.md"),
      });
      const withSafety = safety ? groups.map((group) => group.title === "who did it"
        ? { ...group, facts: [...(group.facts ?? []), { label: "Safety", value: <Link className="link" href={safety.href}>{safety.verdict}</Link> }] }
        : group) : groups;
      return <PageFacts page="pull" title="overview" groups={withSafety} tools={pageTools("pull")} level={2} />;
    }

    return (
      <article className="flex max-w-[860px] flex-col gap-10" data-brief>
        <PageOp op={getOp} />
        {/* web-adopt: the banner loads the source post (excerpt, thumbnail) and hides itself when the reader cannot read it. */}
        <InspiredByBanner inspiredBy={cr.inspiredBy} />

        <section aria-labelledby="pull-what" className="flex flex-col gap-4" data-brief-section="what">
          <h2 id="pull-what" className="sr-only">
            What this is
          </h2>
          {brief?.outcome ? (
            <p className="max-w-[36em] text-lg font-light text-ink md:text-[20px] md:leading-[1.5]" data-outcome>
              {brief.outcome}
            </p>
          ) : (
            <p className="max-w-[36em] text-md text-ink-2" data-outcome="none">
              The author has not said in a sentence what this does. The description and the files are below.
            </p>
          )}
          <p className="flex flex-wrap items-center gap-x-2 gap-y-1 text-base text-ink-2" data-status={status.kind}>
            <State shape={STATE_SHAPE[status.kind]} testId="status-sentence">
              {status.headline}
            </State>
            {status.detail ? <span>{capitalize(status.detail)}.</span> : null}
            {older ? (
              <span data-brief-older>
                The sentence above was written for r{older}; this is r{latestN}.
              </span>
            ) : null}
          </p>
        </section>

        {open ? (
          <Sect id="pull-why" title="Why you" className="scroll-mt-10">
            <div className="flex flex-col gap-5" data-brief-section="why">
              {rules.length || holds.length ? (
                <div className="flex flex-col gap-1.5" data-testid="why-you">
                  {rules.map((line) => (
                    <p key={line} className="max-w-[40em] text-md text-ink">
                      {line}
                    </p>
                  ))}
                  {holds.map((line) => (
                    <p key={line} className="max-w-[40em] text-md text-ink-2">
                      {line}
                    </p>
                  ))}
                </div>
              ) : (
                <p className="max-w-[40em] text-md text-ink" data-testid="why-you">
                  {evaluation.allowed ? (cr.author?.kind === "PRINCIPAL_KIND_AGENT" ? "Every rule passes. An agent does not merge it alone, so it waits for a person." : "Every rule passes. It can be merged.") : "No rule names a person here."}
                </p>
              )}
              <NeedsYou repo={fullName} change={cr} comments={comments.value ?? []} checklist={checklist.value} viewer={me} signInNext={here} />
              <Unavailable what="The checklist" settled={checklist} />
              <More summary="Show every rule" testId="every-rule">
                <RuleList evaluation={evaluation} />
              </More>
            </div>
          </Sect>
        ) : null}

        <Sect id="pull-checks" title="Checks">
          <div data-brief-section="checks">
            <p className="max-w-[40em] text-md text-ink">
              {checksSentence(checks)}
              {items.length ? " " + capitalize(countWord(items.length)) + (items.length === 1 ? " piece of evidence is" : " pieces of evidence are") + " attached." : ""}
            </p>
            {checks.length ? (
              <More summary="Show the checks" testId="show-checks">
                <HumanChecks checks={checks} repo={fullName} summary={false} />
                <p className="mt-3 text-sm">
                  <Link href={changeHref(fullName, id, "checks")} className="link">
                    Check output and logs
                  </Link>
                </p>
              </More>
            ) : null}
            {evidence.error ? (
              <Unavailable what="Evidence" settled={evidence} />
            ) : items.length ? (
              <More summary={"Show the evidence (" + items.length + ")"} testId="show-evidence">
                <EvidenceGallery items={items} repo={fullName} changeId={id} latest={latestN} />
              </More>
            ) : null}
            {brief?.tryItUrl ? (
              <div className="mt-4">
                <PreviewLink url={brief.tryItUrl} title="Try it" />
              </div>
            ) : null}
          </div>
        </Sect>

        <Sect id="pull-files" title="Files changed">
          <div className="flex flex-col gap-5" data-brief-section="files">
            <p className="max-w-[40em] text-md text-ink">
              {picks.length ? (picks.length === 1 ? "One place is" : capitalize(countWord(picks.length)) + " places are") + " worth your eyes." + (files.length > picks.length ? " The rest is one click away." : "") : files.length ? "Nothing here changes behaviour or touches a sensitive path." : "This revision changes no files."}
            </p>
            {picks.length ? <WorthALook diff={diff} highlights={brief?.highlights} repo={fullName} changeId={id} /> : null}
            <div className="flex flex-col gap-1.5" data-risk-block>
              <p className="max-w-[40em] text-base text-ink-2" data-risk>
                {riskSentence(latest?.diffClass ?? "", brief)}
              </p>
              {brief?.risks.length || brief?.rollback ? (
                <More summary="Risks and rollback" testId="show-risks">
                  {brief?.risks.length ? (
                    <ul className="flex flex-col gap-4" data-risks>
                      {brief.risks.map((r, i) => (
                        <li key={i} className="flex flex-col gap-1">
                          <span className="text-base text-ink">{r.text}</span>
                          {r.mitigation ? <span className="text-base text-ink-2">Mitigation: {r.mitigation}</span> : <span className="text-sm text-ink-2">No mitigation given.</span>}
                        </li>
                      ))}
                    </ul>
                  ) : null}
                  {brief?.rollback ? (
                    <p className="mt-4 text-base text-ink-2" data-rollback>
                      <span className="font-medium text-ink">Rollback: </span>
                      {brief.rollback}
                    </p>
                  ) : null}
                </More>
              ) : null}
            </div>
            {files.length ? (
              <div>
                <More summary={"Show all " + files.length + (files.length === 1 ? " file" : " files")} testId="show-files">
                  <WhatChanged diff={diff} highlights={brief?.highlights} repo={fullName} changeId={id} />
                </More>
                <p className="mt-3 text-sm">
                  <Link href={codeHref(fullName, id)} className="link">
                    Open the full diff
                  </Link>
                </p>
              </div>
            ) : null}
          </div>
        </Sect>

        <Sect id="pull-commits" title="Commits">
          <div data-brief-section="commits">
            {commitViews.length ? (
              <>
                <CommitRows commits={commitViews} label="Commits in this pull request" hrefFor={(c) => codeHref(fullName, id, c.note ? Number(c.note.slice(1)) : undefined)} />
                {!commits.known ? <Quiet className="mt-3">This server does not list the commits between the branch and {cr.targetRef}; these are the head of each revision.</Quiet> : null}
              </>
            ) : (
              <Quiet>The commits could not be read. The revisions are on Files changed.</Quiet>
            )}
            {children.length ? (
              <Quiet className="mt-3">
                Stacked on top of this:{" "}
                {children.map((c, i) => (
                  <span key={c.id}>
                    {i ? ", " : null}
                    <Link href={changeHref(fullName, c.id)} className="link">
                      #{c.number} {c.title}
                    </Link>
                  </span>
                ))}
              </Quiet>
            ) : null}
          </div>
        </Sect>

        <Sect id="pull-who" title="Who did it">
          <div className="flex flex-col gap-6" data-brief-section="who">
            <WhoDidIt who={who} safety={safety} />
            <Unavailable what="The agent's sessions" settled={sessions} />
            {reviews.length || (comments.value ?? []).length ? (
              <More summary="Everyone on this pull request" testId="show-team">
                <Team change={cr} reviews={reviews} comments={comments.value} sessions={sessions.value} />
              </More>
            ) : null}
          </div>
        </Sect>

        {cr.body.trim() ? (
          <Sect id="pull-description" title="Description">
            <Markdown source={cr.body} repo={fullName} evidence={index} />
          </Sect>
        ) : null}

        <div id="post" className="scroll-mt-10 border-t border-line pt-5 empty:hidden" data-brief-section="post">
          <PostPanelSlot repo={fullName} change={cr} />
        </div>

        <Sect id="pull-conversation" title={"Conversation" + (conversation.length ? " · " + conversation.length : "")}>
          <div className="flex flex-col gap-5" data-brief-section="conversation">
            <Unavailable what="Comments" settled={comments} />
            {conversation.length ? (
              <ol className="flex flex-col divide-y divide-line">
                {conversation.map((c) => {
                  const whoSaid = c.kind === "comment" ? c.comment.author : c.review.reviewer;
                  return (
                    <li key={c.kind + ":" + (c.kind === "comment" ? c.comment.id : c.review.id)} className="flex gap-3 py-4 first:pt-0" data-entry={c.kind}>
                      <Avatar principal={toAvatar(whoSaid)} size={28} />
                      <div className="flex min-w-0 flex-1 flex-col gap-1.5">
                        {c.kind === "comment" ? (
                          <>
                            <span className="flex flex-wrap items-baseline gap-x-2">
                              <span className="font-medium text-ink">{whoSaid?.display || whoSaid?.id || "Someone"}</span>
                              <span className="text-xs text-ink-2">
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
              <Quiet>No comments yet. Comments on lines are on Files changed.</Quiet>
            )}
            {open ? <CommentForm repo={fullName} crId={id} signedIn={!!me} next={here} /> : null}
          </div>
        </Sect>
      </article>
    );
  });
}
