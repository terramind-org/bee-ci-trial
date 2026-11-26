/**
 * The evidence gallery (docs/spec/web-ui.md §3 "Does it work?", docs/design/tokens.md "Evidence
 * card"): hero first, then by order. Each card is a figure: the media at full width, never cropped,
 * then one meta line (title, source chip, revision, stale badge) and the caption. The hero spans
 * the column; the rest sit two per row from 1024px. Cards are j/k targets.
 */
import { FileTextIcon } from "@phosphor-icons/react/dist/ssr";
import type { ReactNode } from "react";
import type { Evidence, EvidenceAsset } from "@/lib/api/types";
import { assetOf, galleryOrder, isImage, kindOf, mainAssets, staleness, titleOf } from "@/lib/evidence-view";
import { bytes } from "@/lib/format";
import { repoHref } from "@/lib/routes";
import { EmptyState, cx } from "../ui";
import { BenchmarkChart } from "./benchmark-chart";
import { CompareSlider } from "./compare-slider";
import { EvidenceVideo } from "./evidence-video";
import { PreviewLink } from "./preview-link";
import { SourceChip, StaleBadge } from "./source-chip";
import { TestReport } from "./test-report";

function Picture({ asset, eager, className }: { asset: EvidenceAsset; eager: boolean; className?: string }) {
  const ratio = asset.width && asset.height ? asset.width + " / " + asset.height : "16 / 10";
  return (
    <div className={cx("overflow-hidden rounded-lg bg-sunken", className)} style={{ aspectRatio: ratio }}>
      {/* eslint-disable-next-line @next/next/no-img-element -- same-origin evidence, never cropped */}
      <img src={asset.url} alt={asset.alt} className="block size-full object-contain" loading={eager ? "eager" : "lazy"} decoding="async" width={asset.width || undefined} height={asset.height || undefined} />
    </div>
  );
}

function Document({ asset, title }: { asset: EvidenceAsset; title: string }) {
  const type = asset.contentType === "application/pdf" ? "PDF" : asset.contentType.startsWith("text/") ? "Text" : "File";
  return (
    <a href={asset.url} className="flex items-center gap-3 rounded-lg bg-sunken px-4 py-3 text-base text-ink hover:bg-hover" download>
      <FileTextIcon size={22} aria-hidden="true" className="shrink-0 text-ink-2" />
      <span className="min-w-0 flex-1 truncate font-medium">{title}</span>
      <span className="text-sm text-ink-3">
        {type}
        {Number(asset.sizeBytes) ? ", " + bytes(asset.sizeBytes) : ""}
      </span>
    </a>
  );
}

/** The media part of a card, by kind. Missing assets fall back to a sentence, never a broken image. */
export function EvidenceMedia({ evidence, eager = false }: { evidence: Evidence; eager?: boolean }) {
  const title = titleOf(evidence);
  const missing = <p className="rounded-lg bg-sunken px-4 py-6 text-base text-ink-3">The file for this evidence is not available.</p>;
  switch (kindOf(evidence)) {
    case "EVIDENCE_KIND_BEFORE_AFTER": {
      const before = assetOf(evidence, "before");
      const after = assetOf(evidence, "after");
      return before && after ? <CompareSlider before={before} after={after} title={title} eager={eager} /> : missing;
    }
    case "EVIDENCE_KIND_VIDEO":
    case "EVIDENCE_KIND_AUDIO": {
      const main = assetOf(evidence, "main");
      return main ? <EvidenceVideo media={main} poster={assetOf(evidence, "poster")} captions={assetOf(evidence, "captions")} title={title} audio={kindOf(evidence) === "EVIDENCE_KIND_AUDIO"} /> : missing;
    }
    case "EVIDENCE_KIND_BENCHMARK":
      return <BenchmarkChart series={evidence.spec?.benchmark ?? []} title={title} />;
    case "EVIDENCE_KIND_TEST_REPORT":
      return evidence.spec?.testReport ? <TestReport report={evidence.spec.testReport} /> : missing;
    case "EVIDENCE_KIND_PREVIEW_LINK":
      return <PreviewLink url={evidence.spec?.url ?? ""} title={title} />;
    case "EVIDENCE_KIND_IMAGE": {
      const images = mainAssets(evidence).filter(isImage);
      if (!images.length) return missing;
      if (images.length === 1 && images[0]) return <Picture asset={images[0]} eager={eager} />;
      return (
        <div className="grid grid-cols-2 gap-2" data-image-set>
          {images.map((a, i) => (
            <Picture key={a.url + i} asset={a} eager={eager && i < 2} />
          ))}
        </div>
      );
    }
    case "EVIDENCE_KIND_DOCUMENT": {
      const main = assetOf(evidence, "main");
      return main ? <Document asset={main} title={title} /> : missing;
    }
    default: {
      const main = mainAssets(evidence).find(isImage);
      return main ? <Picture asset={main} eager={eager} /> : missing;
    }
  }
}

export interface GalleryContext {
  /** owner/name; with `changeId`, CI chips link to the Checks tab. */
  repo?: string;
  changeId?: string;
  /** The change's latest revision, for the stale badge's words. */
  latest: number;
}

export function EvidenceCard({ evidence, hero = false, repo, changeId, latest }: { evidence: Evidence; hero?: boolean } & GalleryContext) {
  const stale = staleness(evidence, latest);
  const caption = evidence.spec?.caption.trim();
  const checkHref = repo && changeId && evidence.checkRunId ? repoHref(repo, "changes/" + changeId + "/checks#check-" + evidence.checkRunId) : undefined;
  return (
    <figure
      id={"evidence-" + evidence.id}
      tabIndex={-1}
      data-nav-item
      data-evidence={kindOf(evidence).replace("EVIDENCE_KIND_", "").toLowerCase()}
      className={cx("group/card flex min-w-0 flex-col gap-3 rounded-lg outline-offset-4", hero && "lg:col-span-2")}
    >
      <div className={cx("transition-opacity duration-[160ms]", stale && "opacity-60 group-hover/card:opacity-100 group-focus/card:opacity-100 group-focus-within/card:opacity-100")}>
        <EvidenceMedia evidence={evidence} eager={hero} />
      </div>
      <figcaption className="flex flex-col gap-1.5">
        <span className="flex flex-wrap items-center gap-x-2 gap-y-1 text-sm">
          <span className="font-medium text-ink">{titleOf(evidence)}</span>
          <SourceChip evidence={evidence} checkHref={checkHref} />
          <span className="num text-ink-3" title={"Captured for revision " + evidence.revision}>
            r{evidence.revision}
          </span>
          <StaleBadge evidence={evidence} latest={latest} />
        </span>
        {caption ? <span className="max-w-[72ch] text-base text-ink-2">{caption.slice(0, 1000)}</span> : null}
      </figcaption>
    </figure>
  );
}

export function EvidenceGallery({ items, repo, changeId, latest, empty }: { items: Evidence[]; empty?: ReactNode } & GalleryContext) {
  const ordered = galleryOrder(items);
  if (!ordered.length)
    return (
      empty ?? (
        <EmptyState icon="eye" title="No evidence yet">
          Screenshots, videos, benchmarks and test reports that the author or CI attaches show up here.
        </EmptyState>
      )
    );
  return (
    <div className="grid grid-cols-1 gap-x-8 gap-y-10 lg:grid-cols-2" data-evidence-gallery>
      {ordered.map((e, i) => (
        <EvidenceCard key={e.id} evidence={e} hero={i === 0 && (!!e.spec?.hero || ordered.length === 1)} repo={repo} changeId={changeId} latest={latest} />
      ))}
    </div>
  );
}
