/**
 * Generated evidence frames for the fixture adapter: a small settings page drawn from rectangles,
 * in a "before" and an "after" state, plus a plain panel for image sets. Deterministic, so the
 * digest of each frame is the same on every run.
 */
import { Canvas, encodePng, type Rgb } from "./png";

export const FRAME_WIDTH = 640;
export const FRAME_HEIGHT = 400;

const PAPER: Rgb = [247, 246, 243];
const INK: Rgb = [38, 40, 46];
const MUTED: Rgb = [196, 198, 204];
const LINE: Rgb = [222, 222, 218];
const CARD: Rgb = [255, 255, 255];
const ACCENT: Rgb = [46, 125, 91];
const ALERT: Rgb = [196, 86, 56];

export type FrameName = "before" | "after" | "panel";

function page(): Canvas {
  const c = new Canvas(FRAME_WIDTH, FRAME_HEIGHT, PAPER);
  c.rect(0, 0, FRAME_WIDTH, 44, INK);
  c.rect(20, 16, 72, 12, MUTED);
  for (let i = 0; i < 3; i++) c.rect(FRAME_WIDTH - 220 + i * 64, 18, 48, 8, MUTED);
  c.rect(0, 44, 150, FRAME_HEIGHT - 44, CARD);
  c.rect(150, 44, 1, FRAME_HEIGHT - 44, LINE);
  for (let i = 0; i < 5; i++) c.rect(20, 70 + i * 34, 90 - (i % 2) * 20, 10, i === 2 ? INK : MUTED);
  return c;
}

/** The session list of a settings page: before, every session reads the same. */
function before(): Canvas {
  const c = page();
  c.rect(180, 72, 160, 14, INK);
  for (let i = 0; i < 4; i++) {
    c.box(180, 108 + i * 62, 430, 50, CARD, LINE);
    c.rect(198, 124 + i * 62, 140, 10, MUTED);
    c.rect(198, 142 + i * 62, 90, 8, LINE);
  }
  return c;
}

/** After: the current session is marked and the others can be signed out. */
function after(): Canvas {
  const c = page();
  c.rect(180, 72, 160, 14, INK);
  for (let i = 0; i < 4; i++) {
    c.box(180, 108 + i * 62, 430, 50, CARD, i === 0 ? ACCENT : LINE);
    c.rect(198, 124 + i * 62, 140, 10, i === 0 ? INK : MUTED);
    c.rect(198, 142 + i * 62, 90, 8, LINE);
    if (i === 0) c.rect(512, 126 + i * 62, 80, 18, ACCENT);
    else c.box(512, 126 + i * 62, 80, 18, CARD, ALERT);
  }
  return c;
}

function panel(): Canvas {
  const c = new Canvas(FRAME_WIDTH, FRAME_HEIGHT, CARD);
  c.rect(0, 0, FRAME_WIDTH, 8, ACCENT);
  for (let i = 0; i < 6; i++) c.rect(48, 56 + i * 44, 260 + ((i * 53) % 220), 14, i % 2 ? LINE : MUTED);
  return c;
}

const DRAW: Record<FrameName, () => Canvas> = { before, after, panel };

/** The PNG bytes of a named frame. */
export function frame(name: FrameName): Uint8Array {
  return encodePng(DRAW[name]());
}

/** WebVTT captions for the fixture walkthrough video (8 s). */
export const WALKTHROUGH_VTT = [
  "WEBVTT",
  "",
  "00:00:00.000 --> 00:00:03.000",
  "The settings page lists every signed-in session.",
  "",
  "00:00:03.000 --> 00:00:06.000",
  "Changing a role signs out the old session and starts a new one.",
  "",
  "00:00:06.000 --> 00:00:08.000",
  "The list now marks the current session.",
  "",
].join("\n");
