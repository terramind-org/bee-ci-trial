/**
 * The Code tab (docs/spec/web-ui.md §3): the diff reader, moved from the old change page as it
 * was. Revisions (`?rev=n`, `[`/`]`), "changes since rN" (`?since=n`), threads, suggestions, the
 * reviews with their form, the head's provenance, and owners, leases and other open changes on
 * the paths it touches.
 */
import Link from "next/link";
import { ReviewForm, RevisionKeys } from "@/components/change-actions";
import { CopyAs, PageOp } from "@/components/copy-as";
import { DiffViewer } from "@/components/diff-viewer";
import { ImportedBadge, PrincipalMark, PrincipalName } from "@/components/domain";
import { Time } from "@/components/time";
import { Badge, Panel } from "@/components/ui";
import { objectIdText, pathBytes, pathText } from "@/lib/api/ids";
import { scopeFromParams } from "@/lib/api/server";
import { changeHref, sinceRevision, viewedRevision } from "@/lib/change-page";
import { compact, usd } from "@/lib/format";
import { isImported } from "@/lib/infra";
import { guard, param, type SearchParams } from "@/lib/page";
import { api, loadChange, loadDiff, loadHead, loadReviews, loadThreads } from "../data";

const VERDICT: Record<string, { label: string; tone: "ok" | "bad" | "neutral" }> = {
  VERDICT_APPROVE: { label: "approved", tone: "ok" },
  VERDICT_REQUEST_CHANGES: { label: "requested changes", tone: "bad" },
  VERDICT_COMMENT: { label: "commented", tone: "neutral" },
};

export default async function CodeTab({ params, searchParams }: { params: Promise<{ owner: string; repo: string; id: string }>; searchParams: SearchParams }) {
  const { id } = await params;
  const { fullName, ref } = await scopeFromParams(params);
  const sp = await searchParams;
  return guard(async () => {
    const cr = await loadChange(fullName, id);
    const latest = cr.revisions.length;
    const viewing = viewedRevision(param(sp, "rev"), latest);
    const since = sinceRevision(param(sp, "since"), viewing);
    const rev = cr.revisions[viewing - 1];
    const [items, threads, reviews, head] = await Promise.all([
      viewing ? loadDiff(fullName, cr, viewing, since) : Promise.resolve([]),
      loadThreads(fullName, id),
      loadReviews(fullName, id),
      loadHead(fullName, objectIdText(rev?.head)),
    ]);
    const paths = items.map((i) => pathText(i.path));
    const context = await (await api()).repo.getContext({ repo: ref, at: { expr: cr.targetRef }, paths: paths.map(pathBytes) });
    const prov = head?.meta?.provenance;
    const diffOp = { service: "ChangeService" as const, method: "GetChangeDiff", request: { repo: ref, changeRequestId: id, revision: viewing, sinceRevision: since || undefined, base: rev?.base, head: rev?.head }, label: "Diff r" + viewing + (since ? " since r" + since : "") };
    const others = context.openChangesTouching.filter((c) => c.id !== id);

    return (
      <div className="flex min-w-0 flex-col gap-12">
        <PageOp op={diffOp} />
        <RevisionKeys repo={fullName} crId={id} revision={viewing} latest={latest} />
        <section aria-labelledby="diff-title" className="min-w-0">
          <header className="mb-3 flex flex-wrap items-baseline justify-between gap-x-4 gap-y-1">
            <h2 id="diff-title" className="text-xl font-semibold tracking-tight">
              r{viewing}
              {since ? <span className="text-ink-3"> since r{since}</span> : null}
            </h2>
            {isImported(head?.meta) ? <ImportedBadge /> : null}
          </header>
          <DiffViewer repo={fullName} crId={id} items={items} threads={threads} revision={viewing} latest={latest} since={since} />
        </section>

        <Panel title="Reviews" id="reviews-title" actions={<CopyAs compact op={{ service: "ChangeService", method: "ListReviews", request: { repo: ref, changeRequestId: id }, label: "List reviews" }} />}>
          {reviews.length ? (
            <ol className="flex flex-col divide-y divide-line">
              {reviews.map((r) => {
                const v = VERDICT[r.verdict] ?? { label: "reviewed", tone: "neutral" as const };
                const stale = r.revision < latest && r.verdict === "VERDICT_APPROVE";
                return (
                  <li key={r.id} className="flex gap-3 py-3">
                    <PrincipalMark principal={r.reviewer} size={28} />
                    <div className="min-w-0 flex-1">
                      <p className="flex flex-wrap items-center gap-2 text-base">
                        <span className="font-medium">{r.reviewer?.display}</span>
                        <Badge tone={v.tone}>{v.label}</Badge>
                        <span className="text-ink-3">on r{r.revision}</span>
                        {stale ? <span className="text-sm text-ink-3">(stale: carried forward only if later revisions did not change content)</span> : null}
                        <Time iso={r.createdAt} className="text-sm text-ink-3" />
                      </p>
                      {r.body ? <p className="mt-1 text-base text-ink">{r.body}</p> : null}
                    </div>
                  </li>
                );
              })}
            </ol>
          ) : (
            <p className="text-base text-ink-2">No reviews yet.</p>
          )}
          {cr.state === "CHANGE_STATE_OPEN" ? (
            <div className="mt-4 border-t border-line pt-4">
              <ReviewForm repo={fullName} crId={id} revision={latest} viewing={viewing} />
            </div>
          ) : null}
        </Panel>

        <Panel title="Provenance" id="prov-title" actions={<CopyAs compact op={{ service: "QueryService", method: "GetChangesets", request: { repo: ref, ids: rev?.head ? [rev.head] : [], includeMeta: true }, label: "Read provenance" }} />}>
          {prov ? (
            <dl className="grid grid-cols-[auto_1fr] gap-x-6 gap-y-2 text-base">
              <dt className="text-ink-3">Runtime</dt>
              <dd className="mono text-sm">{prov.runtime}</dd>
              <dt className="text-ink-3">Model</dt>
              <dd className="mono text-sm">{prov.model}</dd>
              <dt className="text-ink-3">Session</dt>
              <dd className="mono truncate text-sm" title={prov.session}>
                {prov.session}
              </dd>
              <dt className="text-ink-3">On behalf of</dt>
              <dd>{prov.principalChain.length > 1 ? (cr.author?.display ?? "") + " → owner" : "self"}</dd>
              <dt className="text-ink-3">Cost</dt>
              <dd className="num">
                {usd(prov.cost?.usdMicros ?? 0)} · {compact(prov.cost?.inputTokens ?? 0)} in / {compact(prov.cost?.outputTokens ?? 0)} out
              </dd>
              <dt className="text-ink-3">Ran locally</dt>
              <dd>{prov.checksRun.map((c) => c.name + " (" + c.result + ")").join(", ") || "nothing reported"}</dd>
            </dl>
          ) : (
            <p className="text-base text-ink-2">Written by a person; no agent provenance on this revision.</p>
          )}
          <p className="mt-3 text-sm text-ink-3">Claims other than the principal chain are the client&apos;s and shown as such.</p>
        </Panel>

        <Panel title="Before you touch these paths" id="ctx-title" actions={<CopyAs compact op={{ service: "RepoService", method: "GetContext", request: { repo: ref, at: { expr: cr.targetRef }, paths: paths.map(pathBytes) }, label: "Repo context for these paths" }} />}>
          <div className="grid grid-cols-1 gap-8 text-base md:grid-cols-2">
            <div>
              <h3 className="mb-2 text-sm text-ink-3">Owners</h3>
              {context.owners.length ? (
                <ul className="flex flex-col gap-1">
                  {context.owners.map((o) => (
                    <li key={pathText(o.path)} className="flex justify-between gap-3">
                      <span className="mono truncate text-sm">{pathText(o.path)}</span>
                      <span className="shrink-0 text-ink-2">{o.owners.join(", ") || "none"}</span>
                    </li>
                  ))}
                </ul>
              ) : (
                <p className="text-ink-2">No owners recorded.</p>
              )}
            </div>
            <div>
              <h3 className="mb-2 text-sm text-ink-3">Leases</h3>
              {context.activeLeases.length ? (
                <ul className="flex flex-col gap-1.5">
                  {context.activeLeases.map((l) => (
                    <li key={l.id} className="flex items-center justify-between gap-2">
                      <PrincipalName principal={l.holder} showRuntime={false} />
                      <Badge tone={l.mode === "LEASE_MODE_EXCLUSIVE" ? "bad" : "neutral"}>{l.mode === "LEASE_MODE_EXCLUSIVE" ? "exclusive" : "advisory"}</Badge>
                    </li>
                  ))}
                </ul>
              ) : (
                <p className="text-ink-2">None</p>
              )}
            </div>
            {others.length ? (
              <div className="md:col-span-2">
                <h3 className="mb-2 text-sm text-ink-3">Other open change requests on these paths</h3>
                <ul className="flex flex-col gap-1">
                  {others.map((c) => (
                    <li key={c.id}>
                      <Link href={changeHref(fullName, c.id)} className="link">
                        {c.title}
                      </Link>
                    </li>
                  ))}
                </ul>
              </div>
            ) : null}
          </div>
        </Panel>
      </div>
    );
  });
}
