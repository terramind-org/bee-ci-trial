"use client";

/**
 * Before and after as one figure (docs/spec/web-ui.md §4, docs/design/tokens.md "Compare slider"):
 * both images stacked in one well, the after image clipped at a divider the pointer drags. The
 * handle is a `role="slider"` with `aria-valuenow` 0-100: arrow keys move it by 5, Page Up and
 * Page Down by 25, Home and End to the ends. Dragging is direct, without easing. Each label sits in
 * its own side's outer corner; when the divider leaves that side too narrow to hold it, the label
 * moves beside the divider, on the far side, with a caret pointing back, so neither is ever hidden.
 */
import { ArrowsLeftRightIcon } from "@phosphor-icons/react";
import { useCallback, useRef, useState, type KeyboardEvent, type PointerEvent } from "react";
import type { EvidenceAsset } from "@/lib/api/types";

/** Below this share of the width, a side's label moves beside the divider. */
const NARROW = 16;

export function clampPosition(n: number): number {
  return Math.min(100, Math.max(0, Math.round(n)));
}

/** The new position after a key, or null when the key does not move the divider. */
export function keyStep(key: string, pos: number): number | null {
  switch (key) {
    case "ArrowLeft":
    case "ArrowDown":
      return clampPosition(pos - 5);
    case "ArrowRight":
    case "ArrowUp":
      return clampPosition(pos + 5);
    case "PageDown":
      return clampPosition(pos - 25);
    case "PageUp":
      return clampPosition(pos + 25);
    case "Home":
      return 0;
    case "End":
      return 100;
    default:
      return null;
  }
}

const LABEL = "pointer-events-none absolute top-2 z-10 rounded-xs bg-popover/80 px-1.5 py-0.5 text-xs font-medium text-ink";

export function CompareSlider({ before, after, title, initial = 50, eager = false }: { before: EvidenceAsset; after: EvidenceAsset; title: string; initial?: number; eager?: boolean }) {
  const [pos, setPos] = useState(() => clampPosition(initial));
  const well = useRef<HTMLDivElement>(null);
  const dragging = useRef(false);
  const w = after.width || before.width;
  const h = after.height || before.height;

  const fromPointer = useCallback((clientX: number) => {
    const box = well.current?.getBoundingClientRect();
    if (!box || box.width === 0) return;
    setPos(clampPosition(((clientX - box.left) / box.width) * 100));
  }, []);

  const down = (e: PointerEvent<HTMLDivElement>) => {
    if (e.button !== 0) return;
    dragging.current = true;
    e.currentTarget.setPointerCapture(e.pointerId);
    fromPointer(e.clientX);
  };
  const move = (e: PointerEvent<HTMLDivElement>) => {
    if (dragging.current) fromPointer(e.clientX);
  };
  const up = (e: PointerEvent<HTMLDivElement>) => {
    dragging.current = false;
    if (e.currentTarget.hasPointerCapture(e.pointerId)) e.currentTarget.releasePointerCapture(e.pointerId);
  };
  const key = (e: KeyboardEvent<HTMLDivElement>) => {
    const next = keyStep(e.key, pos);
    if (next === null) return;
    e.preventDefault();
    setPos(next);
  };

  const beforeNarrow = pos < NARROW;
  const afterNarrow = 100 - pos < NARROW;
  const img = "absolute inset-0 size-full object-contain select-none";

  return (
    <div className="flex flex-col gap-2" data-compare>
      <div
        ref={well}
        className="relative touch-pan-y overflow-hidden rounded-lg bg-sunken select-none"
        style={{ aspectRatio: w && h ? w + " / " + h : "16 / 10", cursor: "ew-resize" }}
        onPointerDown={down}
        onPointerMove={move}
        onPointerUp={up}
        onPointerCancel={up}
      >
        {/* eslint-disable-next-line @next/next/no-img-element -- same-origin evidence, sized by the well */}
        <img src={before.url} alt="" draggable={false} className={img} loading={eager ? "eager" : "lazy"} width={before.width || undefined} height={before.height || undefined} />
        {/* eslint-disable-next-line @next/next/no-img-element -- same-origin evidence, sized by the well */}
        <img src={after.url} alt="" draggable={false} className={img} style={{ clipPath: "inset(0 0 0 " + pos + "%)" }} loading={eager ? "eager" : "lazy"} width={after.width || undefined} height={after.height || undefined} />
        <span className={LABEL} style={beforeNarrow ? { left: "calc(" + pos + "% + 24px)" } : { left: 8 }} data-label="before">
          {beforeNarrow ? "‹ Before" : "Before"}
        </span>
        <span className={LABEL} style={afterNarrow ? { right: "calc(" + (100 - pos) + "% + 24px)" } : { right: 8 }} data-label="after">
          {afterNarrow ? "After ›" : "After"}
        </span>
        <div aria-hidden="true" className="absolute inset-y-0 w-0.5 -translate-x-1/2 bg-ink" style={{ left: pos + "%" }} />
        <div
          role="slider"
          tabIndex={0}
          aria-label={"Compare before and after: " + title}
          aria-valuemin={0}
          aria-valuemax={100}
          aria-valuenow={pos}
          aria-valuetext={pos + "% before, " + (100 - pos) + "% after"}
          aria-orientation="horizontal"
          onKeyDown={key}
          className="absolute top-1/2 z-20 grid size-8 -translate-x-1/2 -translate-y-1/2 place-items-center rounded-full bg-surface text-ink ring-1 ring-line-strong"
          style={{ left: pos + "%" }}
        >
          <ArrowsLeftRightIcon size={16} aria-hidden="true" />
        </div>
      </div>
      <p className="text-sm text-ink-3">
        <span className="font-medium text-ink-2">Before:</span> {before.alt || "no description"}. <span className="font-medium text-ink-2">After:</span> {after.alt || "no description"}.
      </p>
    </div>
  );
}
