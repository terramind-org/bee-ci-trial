/**
 * Principal avatars: the calm, generated marks every list, header and card shows.
 *
 * Import Phosphor icons from "@phosphor-icons/react/dist/ssr" in server components (no "use
 * client" boundary, so they render on the server) and from "@phosphor-icons/react" in client
 * components. This file is a plain function of its props: no hooks, no randomness, no clock, so
 * it renders identically on the server and the client and works in either kind of component.
 *
 * Without a photo, each kind draws its own deterministic SVG seeded from the principal id:
 *   human    one or two soft round shapes in one hue, on a tint of that hue, on a round tile
 *   agent    a small geometric mark (dots, bars, arcs on a 3×3 grid) in one hue on a neutral
 *            rounded square, plus a neutral badge with the runtime's initial
 *   service  a neutral diamond on a square tile with one facet or inset in one hue
 *
 * Colour: twelve hues at one chroma, in OKLCH, so no hue shouts louder than another. Every tone is
 * a `light-dark()` pair, which follows the `color-scheme` that `[data-theme]` sets in globals.css,
 * and neutrals are the zinc tokens. Each avatar uses one hue and at most two tones. No gradients.
 */
import { useId, type ReactNode } from "react";
import type { Principal } from "@/lib/api/types";

export type AvatarKind = "human" | "agent" | "service";

export interface AvatarPrincipal {
  id: string;
  name: string;
  kind: AvatarKind;
  avatarUrl?: string;
  /** The agent runtime, e.g. "claude-code"; drawn as an initial badge on agents. */
  runtime?: string;
}

const VIEW = 40;

/** FNV-1a over UTF-16 code units; stable everywhere. */
function hash32(s: string): number {
  let h = 0x811c9dc5;
  for (let i = 0; i < s.length; i++) {
    h ^= s.charCodeAt(i);
    h = Math.imul(h, 0x01000193) >>> 0;
  }
  return h >>> 0;
}

/** A small seeded generator (mulberry32). */
function rng(seed: number): () => number {
  let a = seed >>> 0;
  return () => {
    a = (a + 0x6d2b79f5) >>> 0;
    let t = a;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

const n = (v: number) => Number(v.toFixed(2));

/** Twelve OKLCH hues, 30° apart, starting at the cobalt accent's hue (264°). */
const hueOf = (seed: number) => (264 + (seed % 12) * 30) % 360;

/** One tone of a hue: [lightness on light, lightness on dark, chroma]. */
type Tone = readonly [number, number, number];
/** The pale ground of a human tile. */
const TINT: Tone = [0.93, 0.31, 0.03];
/** The shape on a human tile: one step off its ground, never loud. */
const SOFT: Tone = [0.83, 0.47, 0.07];
/** A crisp mark on a neutral ground (agents, services). */
const MARK: Tone = [0.64, 0.72, 0.08];

const tone = (hue: number, [light, dark, chroma]: Tone) => `light-dark(oklch(${light} ${chroma} ${hue}), oklch(${dark} ${chroma} ${hue}))`;

/** Zinc neutrals from the theme tokens, with fallbacks for a page without globals.css. */
const GROUND = "var(--sunken, light-dark(#f4f4f5, #18181b))";
const EDGE = "var(--line-strong, light-dark(#d4d4d8, #3f3f46))";
const SURFACE = "var(--surface, light-dark(#ffffff, #09090b))";
const INK = "var(--ink, light-dark(#09090b, #f4f4f5))";

function Human({ seed }: { seed: number }) {
  const next = rng(seed);
  const hue = hueOf(seed);
  const fill = tone(hue, SOFT);
  // A large disc rising from one side, sometimes with a smaller one beside it, like a hill and a
  // sun: two calm masses in one tone, so the tile never holds more than two tones.
  const angle = next() * Math.PI * 2;
  const big = 15 + next() * 5;
  const reach = 15 + next() * 6;
  const bx = 20 + Math.cos(angle) * reach;
  const by = 20 + Math.sin(angle) * reach;
  const second = next() < 0.6;
  const small = 4.5 + next() * 2.5;
  const away = angle + Math.PI + (next() - 0.5) * 1.4;
  const gap = 8 + next() * 3;
  return (
    <>
      <rect width={VIEW} height={VIEW} style={{ fill: tone(hue, TINT) }} />
      <circle cx={n(bx)} cy={n(by)} r={n(big)} style={{ fill }} />
      {second ? <circle cx={n(20 + Math.cos(away) * gap)} cy={n(20 + Math.sin(away) * gap)} r={n(small)} style={{ fill }} /> : null}
    </>
  );
}

/** The agent mark's 3×3 grid: node centres 12, 20 and 28, one step apart. */
const STEP = 8;
const node = (i: number) => 12 + i * STEP;
const STROKE = 4.2;
const DOT = 3.1;

/** Quadrant directions for an arc: it joins the node to the right or left and the node below or above. */
const quad = (q: number): [number, number] => [q === 1 || q === 2 ? -1 : 1, q < 2 ? 1 : -1];

/** An arc whose two ends land on grid nodes; picks a quadrant that stays inside the grid. */
function arcAt(c: number, r: number, turn: number): Prim {
  const q = Math.floor(turn * 4);
  const [dx, dy] = quad(q);
  return { t: "arc", c: c + dx < 0 || c + dx > 2 ? 2 - c : c, r: r + dy < 0 || r + dy > 2 ? 2 - r : r, q };
}

type Prim =
  | { t: "dot"; c: number; r: number }
  | { t: "bar"; c1: number; r1: number; c2: number; r2: number }
  | { t: "arc"; c: number; r: number; q: number }
  | { t: "ring"; c: number; r: number };

/** Which grid cells (index r*3+c) a primitive covers. */
function cellsOf(p: Prim): number[] {
  if (p.t === "dot") return [p.r * 3 + p.c];
  if (p.t === "ring") return [p.r * 3 + p.c];
  if (p.t === "arc") {
    const [dx, dy] = quad(p.q);
    // The whole 2×2 block it bends through, so nothing touches the curve.
    return [p.r * 3 + p.c, p.r * 3 + p.c + dx, (p.r + dy) * 3 + p.c, (p.r + dy) * 3 + p.c + dx];
  }
  const out: number[] = [];
  const steps = Math.max(Math.abs(p.c2 - p.c1), Math.abs(p.r2 - p.r1));
  for (let i = 0; i <= steps; i++) out.push((p.r1 + Math.sign(p.r2 - p.r1) * i) * 3 + p.c1 + Math.sign(p.c2 - p.c1) * i);
  return out;
}

function agentMark(seed: number): Prim[] {
  const next = rng(seed);
  // Two to four primitives that never share a cell, anchored by one larger form (a bar, a ring or
  // an arc), so every mark has a clear figure rather than reading as scattered pixels. Only the
  // anchor may curve; the rest are dots and short bars, which keeps the mark crisp.
  const count = 2 + Math.floor(next() * 3);
  const used = new Set<number>();
  const prims: Prim[] = [];
  const free = (p: Prim) => cellsOf(p).every((c) => !used.has(c));
  const take = (p: Prim) => {
    prims.push(p);
    for (const c of cellsOf(p)) used.add(c);
  };
  const lead = next();
  if (lead < 0.5) {
    const vertical = next() < 0.5;
    const line = Math.floor(next() * 3);
    const long = next() < 0.6;
    const from = long ? 0 : Math.floor(next() * 2);
    const to = long ? 2 : from + 1;
    take(vertical ? { t: "bar", c1: line, r1: from, c2: line, r2: to } : { t: "bar", c1: from, r1: line, c2: to, r2: line });
  } else if (lead < 0.68) {
    take({ t: "ring", c: 1, r: 1 });
  } else {
    take(arcAt(Math.floor(next() * 3), Math.floor(next() * 3), next()));
  }
  for (let tries = 0; prims.length < count && tries < 24; tries++) {
    const k = next();
    const c = Math.floor(next() * 3);
    const r = Math.floor(next() * 3);
    let p: Prim;
    if (k < 0.6) p = { t: "dot", c, r };
    else {
      const vertical = next() < 0.5;
      p = vertical ? { t: "bar", c1: c, r1: Math.min(r, 1), c2: c, r2: Math.min(r, 1) + 1 } : { t: "bar", c1: Math.min(c, 1), r1: r, c2: Math.min(c, 1) + 1, r2: r };
    }
    // A ring fills the middle; keep its neighbours clear so it stays legible at 20px.
    const ring = prims.some((x) => x.t === "ring");
    if (ring && p.t !== "dot") continue;
    if (ring && cellsOf(p).some((cell) => cell !== 0 && cell !== 2 && cell !== 6 && cell !== 8)) continue;
    if (free(p)) take(p);
  }
  return prims;
}

function Agent({ seed }: { seed: number }) {
  const stroke = tone(hueOf(seed), MARK);
  const prims = agentMark(seed);
  return (
    <>
      <rect width={VIEW} height={VIEW} style={{ fill: GROUND }} />
      <g style={{ fill: stroke, stroke, strokeWidth: STROKE, strokeLinecap: "round" }}>
        {prims.map((p, i) => {
          if (p.t === "dot") return <circle key={i} cx={node(p.c)} cy={node(p.r)} r={DOT} style={{ stroke: "none" }} />;
          if (p.t === "ring") return <circle key={i} cx={node(p.c)} cy={node(p.r)} r={6.5} style={{ fill: "none", strokeWidth: STROKE - 0.8 }} />;
          if (p.t === "bar") return <line key={i} x1={node(p.c1)} y1={node(p.r1)} x2={node(p.c2)} y2={node(p.r2)} />;
          // A quarter circle of radius one grid step around its node, from one neighbour to another.
          const [dx, dy] = quad(p.q);
          const cx = node(p.c);
          const cy = node(p.r);
          const s = STEP;
          return <path key={i} d={`M${cx + dx * s} ${cy} A${s} ${s} 0 0 ${dx * dy > 0 ? 1 : 0} ${cx} ${cy + dy * s}`} style={{ fill: "none" }} />;
        })}
      </g>
    </>
  );
}

function Service({ seed }: { seed: number }) {
  const next = rng(seed);
  const accent = tone(hueOf(seed), MARK);
  const c = 20;
  const h = 13;
  const top = `${c},${c - h}`;
  const right = `${c + h},${c}`;
  const bottom = `${c},${c + h}`;
  const left = `${c - h},${c}`;
  const mid = `${c},${c}`;
  const facets = [`${top} ${right} ${mid}`, `${right} ${bottom} ${mid}`, `${bottom} ${left} ${mid}`, `${left} ${top} ${mid}`];
  // One accent: a single facet, two facing facets, or a small inset diamond at a seeded height.
  const mode = next();
  let mark: ReactNode;
  if (mode < 0.4) mark = <polygon points={facets[Math.floor(next() * 4)]} />;
  else if (mode < 0.6) {
    const f = Math.floor(next() * 2);
    mark = (
      <>
        <polygon points={facets[f]} />
        <polygon points={facets[f + 2]} />
      </>
    );
  } else {
    const r = 5 + Math.floor(next() * 3) * 1.25;
    const y = c + (Math.floor(next() * 3) - 1) * 4;
    mark = <polygon points={`${c},${n(y - r)} ${c + r},${y} ${c},${n(y + r)} ${c - r},${y}`} />;
  }
  return (
    <>
      <rect width={VIEW} height={VIEW} style={{ fill: GROUND }} />
      <polygon points={`${top} ${right} ${bottom} ${left}`} style={{ fill: "none", stroke: EDGE, strokeWidth: 1.6, strokeLinejoin: "round" }} />
      <g style={{ fill: accent }}>{mark}</g>
    </>
  );
}

/** The one-letter runtime mark: the first letter of the runtime's plain name. */
function runtimeInitial(runtime: string): string {
  const letter = runtime.trim().match(/[A-Za-z0-9]/)?.[0];
  return letter ? letter.toUpperCase() : "·";
}

export function Avatar({ principal, size = 32, className }: { principal: AvatarPrincipal; size?: number; className?: string }) {
  const { id, name, kind, avatarUrl, runtime } = principal;
  const label = `${name} (${kind})`;
  // One clip id per rendered avatar. A principal shown twice used to share one, so url(#id) found
  // the first copy, and when that copy sat in the hidden phone drawer Chrome clipped nothing.
  const clip = "av-" + useId().replace(/[^A-Za-z0-9_-]/g, "");
  const radius = kind === "human" ? "50%" : kind === "agent" ? "24%" : "12%";
  if (avatarUrl) {
    return (
      // A plain <img>: avatar hosts are arbitrary and next/image would need an allow-list for each.
      // eslint-disable-next-line @next/next/no-img-element
      <img src={avatarUrl} alt={label} role="img" aria-label={label} width={size} height={size} className={className} style={{ width: size, height: size, borderRadius: radius, objectFit: "cover", flexShrink: 0 }} />
    );
  }
  const seed = hash32(kind + ":" + id);
  const badge = kind === "agent" && runtime ? 16 : 0;
  const rx = kind === "human" ? VIEW / 2 : kind === "agent" ? VIEW * 0.24 : VIEW * 0.12;
  return (
    <svg role="img" aria-label={label} width={size} height={size} viewBox={`0 0 ${VIEW} ${VIEW}`} className={className} style={{ flexShrink: 0, overflow: "visible" }}>
      <defs>
        <clipPath id={clip}>
          <rect width={VIEW} height={VIEW} rx={rx} />
        </clipPath>
      </defs>
      <g clipPath={`url(#${clip})`}>
        {kind === "human" ? <Human seed={seed} /> : kind === "agent" ? <Agent seed={seed} /> : <Service seed={seed} />}
      </g>
      {badge > 0 && runtime ? (
        <g transform={`translate(${VIEW - badge * 0.75} ${VIEW - badge * 0.75})`}>
          <rect width={badge} height={badge} rx={badge / 2} style={{ fill: SURFACE, stroke: EDGE, strokeWidth: 1.2 }} />
          <text x={badge / 2} y={badge / 2} textAnchor="middle" dominantBaseline="central" fontSize={badge * 0.6} fontWeight={600} style={{ fill: INK, fontFamily: "var(--font-sans, system-ui), system-ui, sans-serif" }}>
            {runtimeInitial(runtime)}
          </text>
        </g>
      ) : null}
    </svg>
  );
}

/** A wire principal as an avatar. The server sends some principals with only an id, so the name falls back to it. */
export function toAvatar(p: Principal | undefined): AvatarPrincipal {
  if (!p) return { id: "unknown", name: "Someone", kind: "human" };
  const kind: AvatarKind = p.kind === "PRINCIPAL_KIND_AGENT" ? "agent" : p.kind === "PRINCIPAL_KIND_SERVICE" ? "service" : "human";
  return { id: p.id, name: p.display || p.id, kind, runtime: p.runtime || undefined };
}
