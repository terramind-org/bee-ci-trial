// BLAKE3-256 (hash mode only), written from the BLAKE3 specification. Setup fixtures need it to
// compute blob ids (SPEC §3.1: algorithm 0x01, kind 0x01, BLAKE3 of the raw bytes) before
// uploading content. Speed is irrelevant here; fixtures are a few kilobytes.

const IV = new Uint32Array([
  0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
]);
const PERM = [2, 6, 3, 10, 7, 0, 4, 13, 1, 11, 12, 5, 9, 14, 15, 8];
const CHUNK_START = 1;
const CHUNK_END = 2;
const PARENT = 4;
const ROOT = 8;
const BLOCK = 64;
const CHUNK = 1024;

const rotr = (x: number, n: number) => ((x >>> n) | (x << (32 - n))) >>> 0;

function g(s: Uint32Array, a: number, b: number, c: number, d: number, x: number, y: number): void {
  s[a] = (s[a]! + s[b]! + x) >>> 0;
  s[d] = rotr(s[d]! ^ s[a]!, 16);
  s[c] = (s[c]! + s[d]!) >>> 0;
  s[b] = rotr(s[b]! ^ s[c]!, 12);
  s[a] = (s[a]! + s[b]! + y) >>> 0;
  s[d] = rotr(s[d]! ^ s[a]!, 8);
  s[c] = (s[c]! + s[d]!) >>> 0;
  s[b] = rotr(s[b]! ^ s[c]!, 7);
}

function compress(cv: Uint32Array, block: Uint32Array, counter: number, len: number, flags: number): Uint32Array {
  const s = new Uint32Array(16);
  s.set(cv, 0);
  s.set(IV.subarray(0, 4), 8);
  s[12] = counter >>> 0;
  s[13] = Math.floor(counter / 2 ** 32) >>> 0;
  s[14] = len;
  s[15] = flags;
  let m = Uint32Array.from(block);
  for (let r = 0; r < 7; r++) {
    g(s, 0, 4, 8, 12, m[0]!, m[1]!);
    g(s, 1, 5, 9, 13, m[2]!, m[3]!);
    g(s, 2, 6, 10, 14, m[4]!, m[5]!);
    g(s, 3, 7, 11, 15, m[6]!, m[7]!);
    g(s, 0, 5, 10, 15, m[8]!, m[9]!);
    g(s, 1, 6, 11, 12, m[10]!, m[11]!);
    g(s, 2, 7, 8, 13, m[12]!, m[13]!);
    g(s, 3, 4, 9, 14, m[14]!, m[15]!);
    if (r < 6) m = Uint32Array.from(PERM, (p) => m[p]!);
  }
  for (let i = 0; i < 8; i++) {
    s[i] = (s[i]! ^ s[i + 8]!) >>> 0;
    s[i + 8] = (s[i + 8]! ^ cv[i]!) >>> 0;
  }
  return s;
}

function words(bytes: Uint8Array): Uint32Array {
  const padded = new Uint8Array(BLOCK);
  padded.set(bytes);
  const dv = new DataView(padded.buffer);
  const w = new Uint32Array(16);
  for (let i = 0; i < 16; i++) w[i] = dv.getUint32(i * 4, true);
  return w;
}

/** The pending compression whose flags are decided late (ROOT only on the very last one). */
interface Output {
  cv: Uint32Array;
  block: Uint32Array;
  counter: number;
  len: number;
  flags: number;
}

function chunkOutput(chunk: Uint8Array, index: number): Output {
  let cv = IV.slice();
  const blocks = Math.max(1, Math.ceil(chunk.length / BLOCK));
  for (let b = 0; b < blocks - 1; b++) {
    const flags = b === 0 ? CHUNK_START : 0;
    cv = compress(cv, words(chunk.subarray(b * BLOCK, (b + 1) * BLOCK)), index, BLOCK, flags).slice(0, 8);
  }
  const last = chunk.subarray((blocks - 1) * BLOCK);
  return {
    cv,
    block: words(last),
    counter: index,
    len: last.length,
    flags: (blocks === 1 ? CHUNK_START : 0) | CHUNK_END,
  };
}

const chaining = (o: Output) => compress(o.cv, o.block, o.counter, o.len, o.flags).slice(0, 8);

function parentOutput(left: Uint32Array, right: Uint32Array): Output {
  const block = new Uint32Array(16);
  block.set(left, 0);
  block.set(right, 8);
  return { cv: IV.slice(), block, counter: 0, len: BLOCK, flags: PARENT };
}

export function blake3(data: Uint8Array): Uint8Array {
  const stack: Uint32Array[] = [];
  const chunks = Math.max(1, Math.ceil(data.length / CHUNK));
  for (let i = 0; i < chunks - 1; i++) {
    let cv = chaining(chunkOutput(data.subarray(i * CHUNK, (i + 1) * CHUNK), i));
    let total = i + 1;
    while ((total & 1) === 0) {
      cv = chaining(parentOutput(stack.pop()!, cv));
      total >>= 1;
    }
    stack.push(cv);
  }
  let out = chunkOutput(data.subarray((chunks - 1) * CHUNK), chunks - 1);
  for (let i = stack.length - 1; i >= 0; i--) out = parentOutput(stack[i]!, chaining(out));
  const root = compress(out.cv, out.block, 0, out.len, out.flags | ROOT);
  const bytes = new Uint8Array(32);
  const dv = new DataView(bytes.buffer);
  for (let i = 0; i < 8; i++) dv.setUint32(i * 4, root[i]!, true);
  return bytes;
}

export function toHex(b: Uint8Array): string {
  return Array.from(b, (x) => x.toString(16).padStart(2, "0")).join("");
}

/** A blob's 34-byte object id (SPEC §3.1). */
export function blobId(content: Uint8Array): Uint8Array {
  const id = new Uint8Array(34);
  id[0] = 0x01;
  id[1] = 0x01;
  id.set(blake3(content), 2);
  return id;
}
