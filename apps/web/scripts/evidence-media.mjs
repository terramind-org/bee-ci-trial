#!/usr/bin/env node
/**
 * Writes the evidence media the real-server seed attaches to the `ui` change request:
 *
 *   node scripts/evidence-media.mjs            writes e2e/seed/media/*
 *   node scripts/evidence-media.mjs --check    fails when the committed files differ
 *
 * - ui-before.png, ui-after.png, ui-first-try.png and ui-poster.png: a settings page drawn from
 *   rectangles, by a hand-written PNG encoder (signature, IHDR, one IDAT, IEND; filter 0). The
 *   compressed IDAT bytes depend on the zlib that node was built with (node 22's differs from
 *   node 25's), so `--check` compares the header and the inflated pixel rows, not the file.
 * - ui-walkthrough.vtt: the captions, plain text.
 * - ui-walkthrough.webm: eight seconds of the same page changing state, VP9 through ffmpeg. Only
 *   this step needs ffmpeg, and its bytes depend on the ffmpeg build, so `--check` only checks
 *   that the committed file is a webm of at most 400 KB. The file is committed; the seed never
 *   runs ffmpeg.
 */
import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, statSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { deflateSync, inflateSync } from "node:zlib";

const OUT = join(dirname(fileURLToPath(import.meta.url)), "..", "e2e", "seed", "media");
export const WIDTH = 960;
export const HEIGHT = 600;
const VIDEO = { width: 480, height: 300, fps: 8, seconds: 8 };
export const MAX_WEBM_BYTES = 400 * 1024;

// ------------------------------------------------------------ PNG

const CRC = (() => {
  const t = new Uint32Array(256);
  for (let n = 0; n < 256; n++) {
    let c = n;
    for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    t[n] = c >>> 0;
  }
  return t;
})();

function crc32(data) {
  let c = 0xffffffff;
  for (const b of data) c = CRC[(c ^ b) & 0xff] ^ (c >>> 8);
  return (c ^ 0xffffffff) >>> 0;
}

function chunk(type, data) {
  const out = Buffer.alloc(12 + data.length);
  out.writeUInt32BE(data.length, 0);
  out.write(type, 4, "latin1");
  Buffer.from(data).copy(out, 8);
  out.writeUInt32BE(crc32(out.subarray(4, 8 + data.length)), 8 + data.length);
  return out;
}

/** The IHDR and the inflated IDAT of a PNG: what it shows, whichever zlib compressed it. */
export function pngContent(png) {
  const parts = { ihdr: Buffer.alloc(0), idat: [] };
  for (let at = 8; at + 8 <= png.length; ) {
    const len = png.readUInt32BE(at);
    const type = png.toString("latin1", at + 4, at + 8);
    const data = png.subarray(at + 8, at + 8 + len);
    if (type === "IHDR") parts.ihdr = Buffer.from(data);
    else if (type === "IDAT") parts.idat.push(data);
    at += 12 + len;
  }
  return Buffer.concat([parts.ihdr, inflateSync(Buffer.concat(parts.idat))]);
}

/** Truecolour 8-bit PNG of an RGB buffer (width * height * 3 bytes). */
export function encodePng(width, height, rgb) {
  const rows = Buffer.alloc(height * (1 + width * 3));
  for (let y = 0; y < height; y++) {
    const at = y * (1 + width * 3);
    rows[at] = 0;
    Buffer.from(rgb.buffer, rgb.byteOffset + y * width * 3, width * 3).copy(rows, at + 1);
  }
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(width, 0);
  ihdr.writeUInt32BE(height, 4);
  ihdr[8] = 8; // bit depth
  ihdr[9] = 2; // truecolour
  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    chunk("IHDR", ihdr),
    chunk("IDAT", deflateSync(rows, { level: 9 })),
    chunk("IEND", Buffer.alloc(0)),
  ]);
}

// ------------------------------------------------------------ drawing

const PAPER = [247, 246, 243];
const INK = [38, 40, 46];
const MUTED = [196, 198, 204];
const LINE = [222, 222, 218];
const CARD = [255, 255, 255];
const ACCENT = [46, 125, 91];
const ALERT = [196, 86, 56];
const WARM = [244, 226, 196];

class Canvas {
  constructor(width, height, fill) {
    this.width = width;
    this.height = height;
    this.rgb = new Uint8Array(width * height * 3);
    this.rect(0, 0, width, height, fill);
  }
  rect(x, y, w, h, color) {
    const x0 = Math.max(0, Math.round(x));
    const y0 = Math.max(0, Math.round(y));
    const x1 = Math.min(this.width, Math.round(x + w));
    const y1 = Math.min(this.height, Math.round(y + h));
    for (let j = y0; j < y1; j++) {
      for (let i = x0; i < x1; i++) {
        const at = (j * this.width + i) * 3;
        this.rgb[at] = color[0];
        this.rgb[at + 1] = color[1];
        this.rgb[at + 2] = color[2];
      }
    }
  }
  /** A filled rectangle with a one-unit border. */
  box(x, y, w, h, fill, border, unit = 1) {
    this.rect(x, y, w, h, border);
    this.rect(x + unit, y + unit, w - 2 * unit, h - 2 * unit, fill);
  }
}

/** Row states of the sessions list: how far the list has moved from "before" to "after". */
const ROWS = 4;

/**
 * The settings page, drawn in a 640 by 400 design space and scaled to the canvas. `marked` is how
 * many rows already read as "after" (0 is the old page, 4 the new one); `cursor` is a row index
 * highlighted as the one being worked on, or -1.
 */
function settingsPage(width, height, { marked, cursor = -1, warm = false }) {
  const s = width / 640;
  const c = new Canvas(width, height, PAPER);
  const R = (x, y, w, h, col) => c.rect(x * s, y * s, w * s, h * s, col);
  const B = (x, y, w, h, fill, border) => c.box(x * s, y * s, w * s, h * s, fill, border, Math.max(1, Math.round(s)));
  R(0, 0, 640, 44, INK);
  R(20, 16, 72, 12, MUTED);
  for (let i = 0; i < 3; i++) R(640 - 220 + i * 64, 18, 48, 8, MUTED);
  R(0, 44, 150, 356, CARD);
  R(150, 44, 1, 356, LINE);
  for (let i = 0; i < 5; i++) R(20, 70 + i * 34, 90 - (i % 2) * 20, 10, i === 2 ? INK : MUTED);
  R(180, 72, 160, 14, INK);
  if (warm) R(360, 72, 250, 14, WARM);
  for (let i = 0; i < ROWS; i++) {
    const y = 108 + i * 62;
    const after = i < marked;
    B(180, y, 430, 50, i === cursor ? WARM : CARD, after && i === 0 ? ACCENT : LINE);
    R(198, y + 16, 140, 10, after && i === 0 ? INK : MUTED);
    R(198, y + 34, 90, 8, LINE);
    if (after) {
      if (i === 0) R(512, y + 18, 80, 18, ACCENT);
      else B(512, y + 18, 80, 18, CARD, ALERT);
    }
  }
  return c;
}

/** The first attempt: every row is marked, but with a banner the design later dropped. */
function firstTry(width, height) {
  const c = settingsPage(width, height, { marked: ROWS, warm: true });
  const s = width / 640;
  c.rect(180 * s, 356 * s, 430 * s, 20 * s, WARM);
  return c;
}

/** The poster of the video: the page before, with a play triangle's block in the middle. */
function poster(width, height) {
  const c = settingsPage(width, height, { marked: 0 });
  const s = width / 640;
  c.rect(0, 0, width, height, [38, 40, 46]);
  c.rect(60 * s, 60 * s, 520 * s, 280 * s, PAPER);
  c.rect(290 * s, 170 * s, 60 * s, 60 * s, ACCENT);
  return c;
}

const FRAMES = {
  "ui-before.png": () => settingsPage(WIDTH, HEIGHT, { marked: 0 }),
  "ui-after.png": () => settingsPage(WIDTH, HEIGHT, { marked: ROWS }),
  "ui-first-try.png": () => firstTry(WIDTH, HEIGHT),
  "ui-poster.png": () => poster(WIDTH, HEIGHT),
};

export const CAPTIONS = [
  "WEBVTT",
  "",
  "00:00:00.000 --> 00:00:03.000",
  "The settings page lists every signed-in session the same way.",
  "",
  "00:00:03.000 --> 00:00:06.000",
  "Each row is marked in turn: this device is outlined, the others get a Sign out button.",
  "",
  "00:00:06.000 --> 00:00:08.000",
  "The finished list shows which session is yours.",
  "",
].join("\n");

/** Video frame `n`: rows switch to their "after" look one after another, a cursor leads. */
function videoFrame(n) {
  const frames = VIDEO.fps * VIDEO.seconds;
  const t = n / (frames - 1);
  // The first quarter holds the old page, the last quarter holds the new one.
  const progress = Math.min(1, Math.max(0, (t - 0.25) / 0.5));
  const marked = Math.floor(progress * ROWS + 1e-9);
  const cursor = progress >= 1 || progress <= 0 ? -1 : Math.min(ROWS - 1, marked);
  return settingsPage(VIDEO.width, VIDEO.height, { marked: Math.min(ROWS, marked), cursor }).rgb;
}

function encodeWebm() {
  const frames = VIDEO.fps * VIDEO.seconds;
  const raw = Buffer.concat(Array.from({ length: frames }, (_, n) => Buffer.from(videoFrame(n))));
  const args = [
    "-v", "error", "-y",
    "-f", "rawvideo", "-pix_fmt", "rgb24", "-s", `${VIDEO.width}x${VIDEO.height}`, "-r", String(VIDEO.fps), "-i", "-",
    "-c:v", "libvpx-vp9", "-b:v", "0", "-crf", "42", "-deadline", "good", "-cpu-used", "2", "-threads", "1",
    "-pix_fmt", "yuv420p", "-an", "-map_metadata", "-1", "-fflags", "+bitexact", "-flags:v", "+bitexact",
    "-f", "webm", "pipe:1",
  ];
  const res = spawnSync("ffmpeg", args, { input: raw, maxBuffer: 64 * 1024 * 1024 });
  if (res.error) throw new Error("ffmpeg is needed to write the webm: " + res.error.message);
  if (res.status !== 0) throw new Error("ffmpeg failed: " + String(res.stderr));
  if (res.stdout.length > MAX_WEBM_BYTES) throw new Error(`the webm is ${res.stdout.length} bytes, over the ${MAX_WEBM_BYTES} limit`);
  return res.stdout;
}

/** True for the EBML magic and a "webm" doctype in the first bytes. */
function isWebm(bytes) {
  return bytes.length > 32 && bytes.readUInt32BE(0) === 0x1a45dfa3 && bytes.subarray(0, 64).includes(Buffer.from("webm"));
}

function main() {
  const check = process.argv.includes("--check");
  const problems = [];
  const files = { ...Object.fromEntries(Object.entries(FRAMES).map(([name, draw]) => [name, () => { const c = draw(); return encodePng(c.width, c.height, c.rgb); }])), "ui-walkthrough.vtt": () => Buffer.from(CAPTIONS) };
  if (!check) mkdirSync(OUT, { recursive: true });
  for (const [name, make] of Object.entries(files)) {
    const bytes = make();
    const path = join(OUT, name);
    if (check) {
      const same = (a, b) => (name.endsWith(".png") ? pngContent(a).equals(pngContent(b)) : a.equals(b));
      if (!existsSync(path) || !same(readFileSync(path), bytes)) problems.push(name + " differs from what the script writes");
    } else {
      writeFileSync(path, bytes);
      console.log("wrote", name, bytes.length, "bytes");
    }
  }
  const webm = join(OUT, "ui-walkthrough.webm");
  if (check) {
    if (!existsSync(webm)) problems.push("ui-walkthrough.webm is missing");
    else if (!isWebm(readFileSync(webm))) problems.push("ui-walkthrough.webm is not a webm file");
    else if (statSync(webm).size > MAX_WEBM_BYTES) problems.push("ui-walkthrough.webm is over 400 KB");
  } else {
    const bytes = encodeWebm();
    writeFileSync(webm, bytes);
    console.log("wrote ui-walkthrough.webm", bytes.length, "bytes");
  }
  if (problems.length) {
    console.error(problems.join("\n"));
    process.exit(1);
  }
}

if (process.argv[1] === fileURLToPath(import.meta.url)) main();
