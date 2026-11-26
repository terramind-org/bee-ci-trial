/**
 * The Checks tab (docs/spec/web-ui.md §3): every check run of the viewed revision with its output
 * (title, summary, Markdown text, annotations linking to the Code tab), the evidence CI attached
 * to it, and the change's workflow runs. Each check is an anchor (`#check-<id>`), which the Brief's
 * "Verified by CI" chips link to.
 */
import Link from "next/link";
import { EvidenceCard } from "@/components/brief/evidence-gallery";
import { checkHref } from "@/components/brief/human-checks";
import { PageOp } from "@/components/copy-as";
import { RunStatusBadge, StatusIcon } from "@/components/domain";
import { Icon } from "@/components/icons";
import { Markdown } from "@/components/markdown";
import { Time } from "@/components/time";
import { EmptyState, Panel } from "@/components/ui";
import { objectIdText } from "@/lib/api/ids";
import { scopeFromParams } from "@/lib/api/server";
import type { CheckRun, Evidence } from "@/lib/api/types";
import { annotationViews, annotationsSentence, changeHref, evidenceByCheck, safeImageLink, unclaimedCiEvidence, viewedRevision } from "@/lib/change-page";
import { checksSentence } from "@/lib/checkwords";
import { duration, between } from "@/lib/format";
import { guard, param, type SearchParams } from "@/lib/page";
import { repoHref } from "@/lib/routes";
import { runSummary } from "@/lib/runsummary";
import { loadChange, loadChecks, loadEvidence, loadRuns, type Settled } from "../data";

function Annotations({ check, repo, id }: { check: CheckRun; repo: string; id: string }) {
  const items = annotationViews(check.output?.annotations);
  const more = (check.annotationsCount ?? 0) - items.length;
  if (!items.length) return null;
  return (
    <div className="flex flex-col gap-2" data-annotations>
      <p className="text-base text-ink">{annotationsSentence(items)}</p>
      <ul className="flex flex-col gap-2">
        {items.map((a, i) => (
          <li key={i} className="flex items-start gap-2">
            <Icon name={a.level === "failure" ? "x" : a.level === "warning" ? "alert" : "dot"} size={16} className={"mt-1 shrink-0 " + (a.level === "failure" ? "text-bad" : a.level === "warning" ? "text-warn" : "text-ink-3")} />
            <span className="flex min-w-0 flex-col">
              <Link href={changeHref(repo, id, "code") + "#file-" + encodeURIComponent(a.path)} className="mono text-sm text-ink underline-offset-4 hover:underline [overflow-wrap:anywhere]">
                {a.path}
                {a.line ? ":" + a.line : ""}
              </Link>
              <span className="text-base text-ink-2">
                <span className="sr-only">{a.level}: </span>
                {a.title ? <span className="font-medium text-ink">{a.title}. </span> : null}
                {a.message}
              </span>
            </span>
          </li>
        ))}
      </ul>
      {more > 0 ? <p className="text-sm text-ink-3">{more} more on the check run itself.</p> : null}
    </div>
  );
}

function CheckEntry({ c, repo, id, evidence, latest }: { c: CheckRun; repo: string; id: string; evidence: Evidence[]; latest: number }) {
  const href = checkHref(repo, c.url);
  const external = /^https:\/\//.test(href);
  const took = c.startedAt && c.finishedAt ? between(c.startedAt, c.finishedAt) : 0;
  const out = c.output;
  const images = (out?.images ?? []).flatMap((im) => {
    const url = safeImageLink(im.imageUrl);
    return url ? [{ url, alt: im.alt || im.caption || "Image", caption: im.caption ?? "" }] : [];
  });
  return (
    <article id={"check-" + c.id} aria-labelledby={"check-title-" + c.id} className="flex scroll-mt-20 flex-col gap-4 border-t border-line pt-6" data-check={c.name}>
      <header className="flex flex-col gap-1.5">
        <h3 id={"check-title-" + c.id} className="flex flex-wrap items-center gap-x-3 gap-y-1 text-lg font-semibold text-ink">
          <span>{c.name}</span>
          <StatusIcon status={c.status} withLabel />
        </h3>
        <p className="flex flex-wrap items-center gap-x-3 gap-y-1 text-sm text-ink-3">
          {c.reporter ? <span>Reported by {c.reporter.display || c.reporter.id}</span> : null}
          {c.startedAt ? (
            <span>
              Started <Time iso={c.startedAt} />
            </span>
          ) : null}
          {took >= 1000 ? <span>took {duration(took)}</span> : null}
          {c.postLand ? <span>runs after merging</span> : null}
          {external ? (
            <a href={href} target="_blank" rel="noreferrer noopener" className="inline-flex items-center gap-1 text-ink-2 underline-offset-4 hover:text-ink hover:underline">
              Details <Icon name="external" size={13} />
              <span className="sr-only">(opens in a new tab)</span>
            </a>
          ) : (
            <Link href={href} className="text-ink-2 underline-offset-4 hover:text-ink hover:underline">
              Details
            </Link>
          )}
        </p>
      </header>
      {out?.title && out.title !== c.name ? <p className="text-base font-medium text-ink">{out.title}</p> : null}
      {out?.summary ? <Markdown source={out.summary} repo={repo} /> : c.summary ? <p className="text-base text-ink-2">{c.summary}</p> : null}
      {out?.text ? (
        <details className="group" open={c.status === "CHECK_STATUS_FAILED"}>
          <summary className="flex min-h-9 cursor-pointer list-none items-center gap-2 text-base text-ink-2 hover:text-ink [&::-webkit-details-marker]:hidden">
            <Icon name="chevronRight" size={16} className="text-ink-3 transition-transform duration-[160ms] group-open:rotate-90" />
            Full output
          </summary>
          <Markdown source={out.text} repo={repo} className="mt-2" />
        </details>
      ) : null}
      <Annotations check={c} repo={repo} id={id} />
      {images.length ? (
        <ul className="flex flex-col gap-1" aria-label={"Images from " + c.name}>
          {images.map((im) => (
            <li key={im.url}>
              <a href={im.url} target="_blank" rel="noreferrer noopener" className="inline-flex items-center gap-1 text-base text-ink underline-offset-4 hover:underline">
                {im.alt}
                <Icon name="external" size={13} className="text-ink-3" />
                <span className="sr-only">(opens in a new tab)</span>
              </a>
              {im.caption && im.caption !== im.alt ? <span className="text-sm text-ink-3"> · {im.caption}</span> : null}
            </li>
          ))}
        </ul>
      ) : null}
      {evidence.length ? (
        <div className="grid grid-cols-1 gap-8 lg:grid-cols-2" data-check-evidence>
          {evidence.map((e) => (
            <EvidenceCard key={e.id} evidence={e} repo={repo} changeId={id} latest={latest} />
          ))}
        </div>
      ) : null}
    </article>
  );
}

function Failed({ what, settled }: { what: string; settled: Settled<unknown> }) {
  // Signed out on a public repo: the section shows what anonymous reads allow, without a reason.
  return settled.error && settled.error.code !== "unauthenticated" ? (
    <p className="text-base text-ink-2" role="status">
      {what} could not be loaded: <span className="mono text-sm">{settled.error.reason}</span>
    </p>
  ) : null;
}

export default async function ChecksTab({ params, searchParams }: { params: Promise<{ owner: string; repo: string; id: string }>; searchParams: SearchParams }) {
  const { id } = await params;
  const { fullName, ref } = await scopeFromParams(params);
  const sp = await searchParams;
  return guard(async () => {
    const cr = await loadChange(fullName, id);
    const latest = cr.revisions.length;
    const viewing = viewedRevision(param(sp, "rev"), latest);
    const rev = cr.revisions[viewing - 1];
    const [checks, evidence, runs] = await Promise.all([loadChecks(fullName, objectIdText(rev?.head)), loadEvidence(fullName, id), loadRuns(fullName, id)]);
    const byCheck = evidenceByCheck(evidence.value ?? []);
    const other = unclaimedCiEvidence(evidence.value ?? [], checks);
    const sorted = [...checks].sort((a, b) => (a.status === "CHECK_STATUS_FAILED" ? 0 : 1) - (b.status === "CHECK_STATUS_FAILED" ? 0 : 1) || (a.name < b.name ? -1 : a.name > b.name ? 1 : 0));
    const now = Date.now();
    return (
      <div className="flex min-w-0 flex-col gap-14">
        <PageOp op={{ service: "CheckService", method: "ListChecks", request: { repo: ref, changeset: rev?.head }, label: "List the checks on r" + viewing }} />
        <section aria-labelledby="checks-title" className="flex flex-col gap-6">
          <header className="flex flex-col gap-2">
            <h2 id="checks-title" className="text-xl font-semibold tracking-tight">
              Checks on r{viewing}
            </h2>
            <p className="text-lg text-ink" data-checks-sentence>
              {checksSentence(checks)}
            </p>
            {viewing !== latest ? (
              <p className="text-sm text-ink-3">
                An earlier revision.{" "}
                <Link href={changeHref(fullName, id, "checks")} className="text-ink-2 underline underline-offset-4 hover:text-ink">
                  Checks on r{latest}
                </Link>
              </p>
            ) : null}
          </header>
          {sorted.length ? (
            sorted.map((c) => <CheckEntry key={c.id || c.name} c={c} repo={fullName} id={id} evidence={byCheck.get(c.id) ?? []} latest={latest} />)
          ) : (
            <EmptyState icon="check" title="No checks reported for this revision">
              CI and agents report checks against a revision&rsquo;s head; they show up here as they run.
            </EmptyState>
          )}
        </section>

        {other.length || evidence.error ? (
          <Panel title="Other evidence from CI" id="ci-evidence-title">
            <Failed what="Evidence" settled={evidence} />
            <div className="grid grid-cols-1 gap-8 lg:grid-cols-2">
              {other.map((e) => (
                <EvidenceCard key={e.id} evidence={e} repo={fullName} changeId={id} latest={latest} />
              ))}
            </div>
          </Panel>
        ) : null}

        <Panel title="Workflow runs" id="runs-title">
          <Failed what="Workflow runs" settled={runs} />
          {runs.value?.length ? (
            <ul className="flex flex-col divide-y divide-line">
              {runs.value.map((r) => {
                const s = runSummary(r, now);
                return (
                  <li key={r.id}>
                    <Link href={repoHref(fullName, "runs/" + r.id)} className="-mx-2 flex min-h-11 flex-wrap items-center gap-x-3 gap-y-1 rounded-md px-2 py-2.5 hover:bg-hover">
                      <RunStatusBadge status={r.status} />
                      <span className="font-medium text-ink">
                        {r.workflow} <span className="num text-ink-3">#{r.number}</span>
                      </span>
                      <span className="min-w-0 flex-1 text-base text-ink-2">{s.sentence}</span>
                      <span className="mono text-xs text-ink-3">{objectIdText(r.head, true)}</span>
                    </Link>
                  </li>
                );
              })}
            </ul>
          ) : runs.value ? (
            <p className="text-base text-ink-2">No workflow ran for this pull request.</p>
          ) : null}
        </Panel>
      </div>
    );
  });
}
