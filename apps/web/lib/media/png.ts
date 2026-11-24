/**
 * A deterministic PNG encoder: the same pixels always give the same bytes, so the fixture's
 * generated evidence has a stable digest. Truecolour, 8 bits, no interlacing, filter 0.
 */
import { deflateSync } from "node:zlib";

const CRC = (() => {
  const t = new Uint32Array(256);
  for (let n = 0; n < 256; n++) {
    let c = n;
    for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    t[n] = c >>> 0;
  }
  return t;
})();

function crc32(data: Uint8Array): number {
  let c = 0xffffffff;
  for (const b of data) c = (CRC[(c ^ b) & 0xff] as number) ^ (c >>> 8);
  return (c ^ 0xffffffff) >>> 0;
}

function chunk(type: string, data: Uint8Array): Uint8Array {
  const out = new Uint8Array(12 + data.length);
  const view = new DataView(out.buffer);
  view.setUint32(0, data.length);
  for (let i = 0; i < 4; i++) out[4 + i] = type.charCodeAt(i);
  out.set(data, 8);
  view.setUint32(8 + data.length, crc32(out.subarray(4, 8 + data.length)));
  return out;
}

export type Rgb = readonly [number, number, number];

/** A raster the frame generators draw on. */
export class Canvas {
  readonly data: Uint8Array;
  constructor(
    readonly width: number,
    readonly height: number,
    background: Rgb,
  ) {
    this.data = new Uint8Array(width * height * 3);
    this.rect(0, 0, width, height, background);
  }

  /** Fills a rectangle, clipped to the canvas. */
  rect(x: number, y: number, w: number, h: number, rgb: Rgb): void {
    const x0 = Math.max(0, Math.floor(x));
    const y0 = Math.max(0, Math.floor(y));
    const x1 = Math.min(this.width, Math.floor(x + w));
    const y1 = Math.min(this.height, Math.floor(y + h));
    for (let j = y0; j < y1; j++) {
      for (let i = x0; i < x1; i++) {
        const o = (j * this.width + i) * 3;
        this.data[o] = rgb[0];
        this.data[o + 1] = rgb[1];
        this.data[o + 2] = rgb[2];
      }
    }
  }

  /** A rectangle with a 1 px outline. */
  box(x: number, y: number, w: number, h: number, fill: Rgb, line: Rgb): void {
    this.rect(x, y, w, h, line);
    this.rect(x + 1, y + 1, w - 2, h - 2, fill);
  }
}

/** Encodes the canvas as a PNG. */
export function encodePng(c: Canvas): Uint8Array {
  const stride = c.width * 3;
  const raw = new Uint8Array((stride + 1) * c.height);
  for (let y = 0; y < c.height; y++) {
    raw[y * (stride + 1)] = 0;
    raw.set(c.data.subarray(y * stride, (y + 1) * stride), y * (stride + 1) + 1);
  }
  const ihdr = new Uint8Array(13);
  const v = new DataView(ihdr.buffer);
  v.setUint32(0, c.width);
  v.setUint32(4, c.height);
  ihdr[8] = 8; // bit depth
  ihdr[9] = 2; // colour type: RGB
  const parts = [new Uint8Array([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]), chunk("IHDR", ihdr), chunk("IDAT", deflateSync(raw, { level: 9 })), chunk("IEND", new Uint8Array())];
  const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
  let o = 0;
  for (const p of parts) {
    out.set(p, o);
    o += p.length;
  }
  return out;
}
