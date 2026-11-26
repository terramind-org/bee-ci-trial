/** Wire helpers: base64 bytes, paths, and the SPEC §3 text forms of ids. */
import type { ChangeId, ObjectId } from "./types";

const B64 = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const B64_INDEX = new Map([...B64].map((c, i) => [c, i]));

export function bytesToBase64(bytes: Uint8Array): string {
  let out = "";
  for (let i = 0; i < bytes.length; i += 3) {
    const a = bytes[i] ?? 0;
    const b = bytes[i + 1];
    const c = bytes[i + 2];
    const n = (a << 16) | ((b ?? 0) << 8) | (c ?? 0);
    out += B64[(n >> 18) & 63];
    out += B64[(n >> 12) & 63];
    out += b === undefined ? "=" : B64[(n >> 6) & 63];
    out += c === undefined ? "=" : B64[n & 63];
  }
  return out;
}

/** Accepts standard and URL-safe alphabets, with or without padding (proto3 JSON allows both). */
export function base64ToBytes(s: string): Uint8Array {
  const clean = s.replace(/[\s=]/g, "").replace(/-/g, "+").replace(/_/g, "/");
  const out = new Uint8Array(Math.floor((clean.length * 3) / 4));
  let o = 0;
  for (let i = 0; i < clean.length; i += 4) {
    const v = [0, 1, 2, 3].map((k) => B64_INDEX.get(clean[i + k] ?? "A") ?? 0);
    const n = ((v[0] ?? 0) << 18) | ((v[1] ?? 0) << 12) | ((v[2] ?? 0) << 6) | (v[3] ?? 0);
    if (o < out.length) out[o++] = (n >> 16) & 255;
    if (o < out.length && i + 2 < clean.length) out[o++] = (n >> 8) & 255;
    if (o < out.length && i + 3 < clean.length) out[o++] = n & 255;
  }
  return out.subarray(0, o);
}

const enc = new TextEncoder();
const dec = new TextDecoder();

export function textToBase64(s: string): string {
  return bytesToBase64(enc.encode(s));
}

export function base64ToText(s: string): string {
  return dec.decode(base64ToBytes(s));
}

/** Paths are `bytes` on the wire. */
export const pathBytes = textToBase64;
export const pathText = base64ToText;

export function hex(bytes: Uint8Array): string {
  let s = "";
  for (const b of bytes) s += b.toString(16).padStart(2, "0");
  return s;
}

export function fromHex(h: string): Uint8Array {
  const out = new Uint8Array(h.length / 2);
  for (let i = 0; i < out.length; i++) out[i] = parseInt(h.slice(i * 2, i * 2 + 2), 16);
  return out;
}

const KIND_LETTER: Record<number, string> = { 1: "b", 2: "t", 3: "c", 4: "m" };
const LETTER_KIND: Record<string, number> = { b: 1, t: 2, c: 3, m: 4 };

/** `<kind-letter><algorithm-digit>_<64 hex>` (SPEC §3.1). `short` keeps 10 hex digits. */
export function objectIdText(id: ObjectId | undefined, short = false): string {
  if (!id || !id.raw) return "";
  const raw = base64ToBytes(id.raw);
  if (raw.length !== 34) return `?_${hex(raw)}`;
  const alg = raw[0] ?? 0;
  const letter = KIND_LETTER[raw[1] ?? 0] ?? "x";
  const digest = hex(raw.subarray(2));
  return `${letter}${alg}_${short ? digest.slice(0, 10) : digest}`;
}

/** Parses a full text-form object id. Returns null when malformed. */
export function parseObjectId(text: string): ObjectId | null {
  const m = /^([btcm])(\d)_([0-9a-f]{64})$/.exec(text);
  if (!m || !m[1] || !m[2] || !m[3]) return null;
  const raw = new Uint8Array(34);
  raw[0] = Number(m[2]);
  raw[1] = LETTER_KIND[m[1]] ?? 0;
  raw.set(fromHex(m[3]), 2);
  return { raw: bytesToBase64(raw) };
}

/** `k_<32 hex>` (SPEC §3.3). */
export function changeIdText(id: ChangeId | undefined, short = false): string {
  if (!id || !id.raw) return "";
  const h = hex(base64ToBytes(id.raw));
  return `k_${short ? h.slice(0, 8) : h}`;
}
