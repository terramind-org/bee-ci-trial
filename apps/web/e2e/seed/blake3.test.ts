import { describe, expect, it } from "vitest";
import { blake3 } from "./blake3";

const hex = (b: Uint8Array) => Array.from(b, (x) => x.toString(16).padStart(2, "0")).join("");
const bytes = (n: number) => Uint8Array.from({ length: n }, (_, i) => i % 251);

describe("blake3", () => {
  it("matches the published digests of short inputs", () => {
    expect(hex(blake3(new Uint8Array(0)))).toBe("af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262");
    expect(hex(blake3(new TextEncoder().encode("abc")))).toBe("6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85");
  });

  it("hashes multi-chunk inputs the way bee-server does", () => {
    // Digests the dev server accepted as blob ids for these inputs (it verifies every upload).
    expect(hex(blake3(bytes(1024)))).toBe(MULTI[1024]);
    expect(hex(blake3(bytes(3073)))).toBe(MULTI[3073]);
    expect(hex(blake3(bytes(8193)))).toBe(MULTI[8193]);
  });
});

const MULTI: Record<number, string> = {
  1024: "42214739f095a406f3fc83deb889744ac00df831c10daa55189b5d121c855af7",
  3073: "7124b49501012f81cc7f11ca069ec9226cecb8a2c850cfe644e327d22d3e1cd3",
  8193: "bab6c09cb8ce8cf459261398d2e7aef35700bf488116ceb94a36d0f5f1b7bc3b",
};
