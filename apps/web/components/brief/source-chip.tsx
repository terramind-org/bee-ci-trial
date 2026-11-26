/**
 * The source chip and the stale badge (docs/design/tokens.md "Brief patterns"): where a piece of
 * evidence came from in words with a small icon, never colour alone, and whether it predates the
 * change's latest revision.
 */
import Link from "next/link";
import { CheckCircleIcon } from "@phosphor-icons/react/dist/ssr";
import type { Evidence } from "@/lib/api/types";
import { sourceOf, staleness } from "@/lib/evidence-view";
import { Avatar, toAvatar } from "../avatar";
import { Icon } from "../icons";

const CHIP = "inline-flex h-6 items-center gap-1 whitespace-nowrap rounded-xs bg-selected px-1.5 text-xs font-medium";

/**
 * "From the agent" (or "From <name>" for a person) with the uploader's avatar, or "Verified by CI"
 * with a check-circle in `ok`. `checkHref` links the CI chip to the check run.
 */
export function SourceChip({ evidence, checkHref }: { evidence: Pick<Evidence, "source" | "uploader" | "checkRunId">; checkHref?: string }) {
  const s = sourceOf(evidence);
  if (s.ci) {
    const body = (
      <>
        <CheckCircleIcon size={14} weight="fill" aria-hidden="true" className="shrink-0 text-ok" />
        <span className="text-ink-2">{s.label}</span>
      </>
    );
    return checkHref && evidence.checkRunId ? (
      <Link href={checkHref} className={CHIP + " hover:bg-hover"} data-source="ci" title="Open the check run that attached this">
        {body}
      </Link>
    ) : (
      <span className={CHIP} data-source="ci">
        {body}
      </span>
    );
  }
  return (
    <span className={CHIP + " text-ink-2"} data-source="uploaded">
      {evidence.uploader ? <Avatar principal={toAvatar(evidence.uploader)} size={14} /> : null}
      {s.label}
    </span>
  );
}

/** "Stale: from r2" in `warn`, with a clock icon; nothing when the evidence is current. */
export function StaleBadge({ evidence, latest }: { evidence: Pick<Evidence, "stale" | "revision">; latest: number }) {
  const s = staleness(evidence, latest);
  if (!s) return null;
  return (
    <span className="inline-flex h-6 items-center gap-1 whitespace-nowrap rounded-xs bg-warn-soft px-1.5 text-xs font-medium text-warn" title={s.title} data-stale>
      <Icon name="clock" size={13} />
      {s.label}
    </span>
  );
}
