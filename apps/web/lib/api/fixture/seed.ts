/**
 * Deterministic helpers for fixture data. Nothing here touches the wall clock or Math.random,
 * so the same inputs always produce the same ids and content.
 *
 * Object ids use the SPEC §3.1 layout (algorithm byte, kind byte, 32-byte digest), but the
 * digest is a fixture hash, not BLAKE3: fixture ids are stable labels, not content addresses.
 */
import { bytesToBase64, hex } from "../ids";
import type { ChangeId, ObjectId } from "../types";

export function fnv1a(s: string): number {
  let h = 0x811c9dc5;
  for (let i = 0; i < s.length; i++) {
    h ^= s.charCodeAt(i);
    h = Math.imul(h, 0x01000193) >>> 0;
  }
  return h >>> 0;
}

/** mulberry32: small, fast, good enough for demo data. */
export function prng(seed: number): () => number {
  let a = seed >>> 0;
  return () => {
    a = (a + 0x6d2b79f5) >>> 0;
    let t = a;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

export function digest(key: string, length = 32): Uint8Array {
  const rnd = prng(fnv1a(key));
  const out = new Uint8Array(length);
  for (let i = 0; i < length; i++) out[i] = Math.floor(rnd() * 256);
  return out;
}

export const KIND = { blob: 1, tree: 2, changeset: 3, meta: 4 } as const;

export function objectId(kind: keyof typeof KIND, key: string): ObjectId {
  const raw = new Uint8Array(34);
  raw[0] = 1;
  raw[1] = KIND[kind];
  raw.set(digest(`${kind}:${key}`), 2);
  return { raw: bytesToBase64(raw) };
}

export function changeId(key: string): ChangeId {
  return { raw: bytesToBase64(digest(`change:${key}`, 16)) };
}

export function digestHex(key: string, length = 32): string {
  return hex(digest(key, length));
}

const CROCKFORD = "0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/** A ULID-shaped id whose time part is `ms` and whose random part is derived from `key`. */
export function ulid(ms: number, key: string): string {
  let t = "";
  let n = Math.max(0, Math.floor(ms));
  for (let i = 0; i < 10; i++) {
    t = CROCKFORD[n % 32] + t;
    n = Math.floor(n / 32);
  }
  const rnd = prng(fnv1a(`ulid:${key}`));
  let r = "";
  for (let i = 0; i < 16; i++) r += CROCKFORD[Math.floor(rnd() * 32)];
  return t + r;
}
