"use client";

/**
 * Video and audio evidence (docs/spec/web-ui.md §4): nothing downloads until the viewer presses
 * play. First the poster (or a plain frame with the title), then a native player with
 * `preload="none"` and the captions as `<track kind="captions" default>`.
 */
import { PlayIcon, SpeakerHighIcon } from "@phosphor-icons/react";
import { useState } from "react";
import type { EvidenceAsset } from "@/lib/api/types";
import { duration } from "@/lib/format";

export function EvidenceVideo({ media, poster, captions, title, audio = false }: { media: EvidenceAsset; poster?: EvidenceAsset; captions?: EvidenceAsset; title: string; audio?: boolean }) {
  const [loaded, setLoaded] = useState(false);
  const w = media.width || poster?.width;
  const h = media.height || poster?.height;
  const ms = Number(media.durationMs);
  const length = ms > 0 ? duration(ms) : "";
  const track = captions?.url ? <track kind="captions" src={captions.url} srcLang="en" label="English" default /> : null;

  if (audio) {
    return loaded ? (
      <audio controls preload="none" autoPlay src={media.url} className="w-full" data-evidence-media="audio">
        {track}
      </audio>
    ) : (
      <button type="button" onClick={() => setLoaded(true)} className="flex h-14 w-full items-center gap-3 rounded-lg bg-sunken px-4 text-left text-base text-ink hover:bg-hover" aria-label={"Play " + title + (length ? ", " + length : "")} data-evidence-play>
        <span className="grid size-9 place-items-center rounded-full bg-surface ring-1 ring-line-strong">
          <SpeakerHighIcon size={18} aria-hidden="true" />
        </span>
        <span className="min-w-0 flex-1 truncate">{title}</span>
        {length ? <span className="num text-sm text-ink-3">{length}</span> : null}
      </button>
    );
  }

  const ratio = w && h ? w + " / " + h : "16 / 9";
  if (loaded)
    return (
      <video controls preload="none" autoPlay playsInline poster={poster?.url || undefined} src={media.url} className="block w-full rounded-lg bg-sunken" style={{ aspectRatio: ratio }} width={w || undefined} height={h || undefined} aria-label={title} data-evidence-media="video">
        {track}
      </video>
    );
  // The CC and length badges sit beside the button, not in it: inside, axe reads them as the
  // button's visible text and flags the name as a mismatch (label-content-name-mismatch; B9).
  return (
    <div className="group relative">
      <button type="button" onClick={() => setLoaded(true)} className="relative block w-full overflow-hidden rounded-lg bg-sunken text-left" style={{ aspectRatio: ratio }} aria-label={"Play video: " + title + (length ? ", " + length : "") + (captions?.url ? ", with captions" : "")} data-evidence-play>
        {poster?.url ? (
          // eslint-disable-next-line @next/next/no-img-element -- same-origin evidence poster
          <img src={poster.url} alt="" className="absolute inset-0 size-full object-contain" loading="lazy" width={poster.width || undefined} height={poster.height || undefined} />
        ) : (
          <span className="absolute inset-0 grid place-items-center p-6 text-center text-lg text-ink-2">{title}</span>
        )}
        <span className="absolute inset-0 grid place-items-center">
          <span className="grid size-14 place-items-center rounded-full bg-surface text-ink ring-1 ring-line-strong transition-transform duration-[120ms] group-hover:scale-105 group-active:scale-95">
            <PlayIcon size={24} weight="fill" aria-hidden="true" />
          </span>
        </span>
      </button>
      {length || captions?.url ? (
        <span aria-hidden="true" className="pointer-events-none absolute right-2 bottom-2 flex gap-1">
          {captions?.url ? <span className="rounded-xs bg-popover/80 px-1.5 py-0.5 text-xs font-medium text-ink">CC</span> : null}
          {length ? <span className="num rounded-xs bg-popover/80 px-1.5 py-0.5 text-xs text-ink">{length}</span> : null}
        </span>
      ) : null}
    </div>
  );
}
