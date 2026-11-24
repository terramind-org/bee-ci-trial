/**
 * One row of the review queue: the hero thumbnail, the title, the brief's outcome line, the status
 * sentence, and a quiet meta line (author, id, revision, age, per-check icons). The whole row is
 * one link and a j/k target.
 */
import Link from "next/link";
import { ChangeGlyph, type ChangeGlyphState, DiffClassBadge, PrincipalName, StatusIcon } from "../domain";
import { Time } from "../time";
import { Icon } from "../icons";
import type { Evidence } from "@/lib/api/types";
import { thumbnailOf } from "@/lib/evidence-view";
import { shortId } from "@/lib/format";
import type { QueueRow } from "@/lib/queue";
import { repoHref } from "@/lib/routes";
import { cx } from "../ui";

function glyphOf(row: QueueRow): ChangeGlyphState {
  const { change, queue } = row.facts;
  if (change.state === "CHANGE_STATE_LANDED") return "landed";
  if (change.state === "CHANGE_STATE_CLOSED") return "closed";
  switch (row.status.kind) {
    case "queued":
      return "queued";
    case "ready":
      return "ready";
    case "failing":
      return "failing";
    default:
      return queue ? "queued" : "blocked";
  }
}

const TONE: Record<string, string> = { ok: "text-ok", bad: "text-bad", warn: "text-warn", accent: "text-accent-ink", neutral: "text-ink-2" };

function Thumb({ hero, glyph }: { hero?: Evidence; glyph: ChangeGlyphState }) {
  const asset = hero ? thumbnailOf(hero) : undefined;
  return (
    <div className="grid h-14 w-[88px] shrink-0 place-items-center overflow-hidden rounded-md bg-sunken max-sm:h-12 max-sm:w-16" data-testid="hero-thumb">
      {asset?.url ? (
        // eslint-disable-next-line @next/next/no-img-element -- same-origin evidence thumbnail, never cropped
        <img src={asset.url} alt={asset.alt} loading="lazy" decoding="async" className="block size-full object-contain" />
      ) : (
        <ChangeGlyph state={glyph} />
      )}
    </div>
  );
}

export function QueueRowView({ row, repo, hero, showWhy }: { row: QueueRow; repo: string; hero?: Evidence; showWhy: boolean }) {
  const { change, checks } = row.facts;
  const rev = change.revisions.at(-1);
  const glyph = glyphOf(row);
  const outcome = change.brief?.outcome.trim();
  const at = row.facts.landedAt ?? rev?.createdAt;
  return (
    <Link href={repoHref(repo, "changes/" + change.id)} data-nav-item className={cx("flex items-start gap-4 py-4 transition-colors duration-[120ms] hover:bg-hover focus-visible:bg-hover", "-mx-3 px-3 rounded-md")}>
      <Thumb hero={hero} glyph={glyph} />
      <div className="flex min-w-0 flex-1 flex-col gap-1">
        <span className="text-lg font-semibold leading-6 text-ink [overflow-wrap:anywhere]">{change.title}</span>
        {outcome ? <span className="line-clamp-2 text-base text-ink-2 [overflow-wrap:anywhere]">{outcome}</span> : null}
        <span className={cx("text-base", TONE[row.status.tone] ?? "text-ink-2")} data-testid="status-sentence">
          {row.status.sentence}
        </span>
        {showWhy && row.why !== row.status.sentence ? <span className="text-base font-medium text-ink">{row.why}</span> : null}
        <span className="mt-0.5 flex min-w-0 flex-wrap items-center gap-x-3 gap-y-1 text-sm leading-6 text-ink-3">
          <PrincipalName principal={change.author} link={false} />
          <span className="num">{shortId(change.id)}</span>
          <span>r{rev?.n}</span>
          {change.dependsOn ? (
            <span className="inline-flex items-center gap-1">
              <Icon name="branch" size={12} /> stacked
            </span>
          ) : null}
          <DiffClassBadge cls={rev?.diffClass ?? ""} />
          {checks.length ? (
            <span className="inline-flex items-center gap-0.5" aria-label={"Checks: " + checks.map((x) => x.name + " " + x.status.replace("CHECK_STATUS_", "").toLowerCase()).join(", ")}>
              {checks.map((x) => (
                <StatusIcon key={x.id} status={x.status} />
              ))}
            </span>
          ) : null}
          <Time iso={at} className="num" />
        </span>
      </div>
    </Link>
  );
}
