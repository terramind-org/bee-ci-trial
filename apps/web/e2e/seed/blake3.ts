/**
 * BLAKE3-256 (unkeyed hash mode), written from the BLAKE3 specification. The seed needs it to
 * name blobs before uploading them: a bee blob id is BLAKE3 over the raw bytes (SPEC §3.1).
 * Small inputs only; no SIMD, no streaming API.
 */

const IV = new Uint32Array([0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19]);
const PERMUTATION = [2, 6, 3, 10, 7, 0, 4, 13, 1, 11, 12, 5, 9, 14, 15, 8];
const CHUNK_START = 1;
const CHUNK_END = 2;
const PARENT = 4;
const ROOT = 8;
const BLOCK = 64;
const CHUNK = 1024;

const rotr = (x: number, n: number) => ((x >>> n) | (x << (32 - n))) >>> 0;

function g(s: Uint32Array, a: number, b: number, c: number, d: number, x: number, y: number): void {
  s[a] = ((s[a] as number) + (s[b] as number) + x) >>> 0;
  s[d] = rotr((s[d] as number) ^ (s[a] as number), 16);
  s[c] = ((s[c] as number) + (s[d] as number)) >>> 0;
  s[b] = rotr((s[b] as number) ^ (s[c] as number), 12);
  s[a] = ((s[a] as number) + (s[b] as number) + y) >>> 0;
  s[d] = rotr((s[d] as number) ^ (s[a] as number), 8);
  s[c] = ((s[c] as number) + (s[d] as number)) >>> 0;
  s[b] = rotr((s[b] as number) ^ (s[c] as number), 7);
}

/** The compression function: 16 output words (the first 8 are the chaining value). */
function compress(cv: Uint32Array, block: Uint32Array, counter: number, blockLen: number, flags: number): Uint32Array {
  const s = new Uint32Array(16);
  s.set(cv, 0);
  s.set(IV.subarray(0, 4), 8);
  s[12] = counter >>> 0;
  s[13] = Math.floor(counter / 0x1_0000_0000) >>> 0;
  s[14] = blockLen;
  s[15] = flags;
  let m = Uint32Array.from(block);
  for (let round = 0; round < 7; round++) {
    const w = (i: number) => m[i] as number;
    g(s, 0, 4, 8, 12, w(0), w(1));
    g(s, 1, 5, 9, 13, w(2), w(3));
    g(s, 2, 6, 10, 14, w(4), w(5));
    g(s, 3, 7, 11, 15, w(6), w(7));
    g(s, 0, 5, 10, 15, w(8), w(9));
    g(s, 1, 6, 11, 12, w(10), w(11));
    g(s, 2, 7, 8, 13, w(12), w(13));
    g(s, 3, 4, 9, 14, w(14), w(15));
    const next = new Uint32Array(16);
    for (let i = 0; i < 16; i++) next[i] = m[PERMUTATION[i] as number] as number;
    m = next;
  }
  for (let i = 0; i < 8; i++) {
    s[i] = ((s[i] as number) ^ (s[i + 8] as number)) >>> 0;
    s[i + 8] = ((s[i + 8] as number) ^ (cv[i] as number)) >>> 0;
  }
  return s;
}

function words(bytes: Uint8Array): Uint32Array {
  const padded = new Uint8Array(BLOCK);
  padded.set(bytes);
  const out = new Uint32Array(16);
  const view = new DataView(padded.buffer);
  for (let i = 0; i < 16; i++) out[i] = view.getUint32(i * 4, true);
  return out;
}

interface Output {
  cv: Uint32Array;
  block: Uint32Array;
  counter: number;
  blockLen: number;
  flags: number;
}

const chainingValue = (o: Output) => compress(o.cv, o.block, o.counter, o.blockLen, o.flags).subarray(0, 8);

/** The output of one chunk (up to 1024 bytes; the empty input is one empty chunk). */
function chunkOutput(chunk: Uint8Array, counter: number): Output {
  let cv: Uint32Array = IV;
  const blocks = Math.max(1, Math.ceil(chunk.length / BLOCK));
  for (let b = 0; b < blocks; b++) {
    const bytes = chunk.subarray(b * BLOCK, (b + 1) * BLOCK);
    const flags = (b === 0 ? CHUNK_START : 0) | (b === blocks - 1 ? CHUNK_END : 0);
    if (b === blocks - 1) return { cv, block: words(bytes), counter, blockLen: bytes.length, flags };
    cv = compress(cv, words(bytes), counter, BLOCK, flags).subarray(0, 8);
  }
  throw new Error("unreachable");
}

function parentOutput(left: Uint32Array, right: Uint32Array): Output {
  const block = new Uint32Array(16);
  block.set(left, 0);
  block.set(right, 8);
  return { cv: IV, block, counter: 0, blockLen: BLOCK, flags: PARENT };
}

/** BLAKE3-256 of `input`. */
export function blake3(input: Uint8Array): Uint8Array {
  const chunks = Math.max(1, Math.ceil(input.length / CHUNK));
  const stack: Uint32Array[] = [];
  let out: Output | undefined;
  for (let i = 0; i < chunks; i++) {
    const o = chunkOutput(input.subarray(i * CHUNK, (i + 1) * CHUNK), i);
    if (i === chunks - 1) {
      out = o;
      break;
    }
    // Merge completed subtrees: one merge per trailing zero bit of the chunk count so far.
    let cv = chainingValue(o);
    let total = i + 1;
    while ((total & 1) === 0) {
      cv = chainingValue(parentOutput(stack.pop() as Uint32Array, cv));
      total >>= 1;
    }
    stack.push(cv);
  }
  let o = out as Output;
  while (stack.length) o = parentOutput(stack.pop() as Uint32Array, chainingValue(o));
  const root = compress(o.cv, o.block, 0, o.blockLen, o.flags | ROOT);
  const digest = new Uint8Array(32);
  const view = new DataView(digest.buffer);
  for (let i = 0; i < 8; i++) view.setUint32(i * 4, root[i] as number, true);
  return digest;
}
