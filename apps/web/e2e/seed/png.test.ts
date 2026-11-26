import { inflateSync } from "node:zlib";
import { describe, expect, it } from "vitest";
import { crc32, flatPng } from "./png";

describe("flatPng", () => {
  it("has the PNG signature, a matching IHDR and a decodable single-colour image", () => {
    const png = flatPng(4, 3, [10, 20, 30]);
    expect([...png.subarray(0, 8)]).toEqual([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]);
    const view = new DataView(png.buffer, png.byteOffset);
    expect([view.getUint32(16), view.getUint32(20)]).toEqual([4, 3]);
    // The IDAT payload inflates to three rows of a filter byte and four identical pixels.
    const idatLen = view.getUint32(33);
    const raw = inflateSync(png.subarray(41, 41 + idatLen));
    expect(raw.length).toBe(3 * (1 + 4 * 3));
    expect([...raw.subarray(0, 4)]).toEqual([0, 10, 20, 30]);
    expect(new TextDecoder().decode(png.subarray(png.length - 8, png.length - 4))).toBe("IEND");
  });

  it("is deterministic and carries valid chunk checksums", () => {
    expect(flatPng(8, 8, [1, 2, 3])).toEqual(flatPng(8, 8, [1, 2, 3]));
    expect(crc32(new TextEncoder().encode("IEND"))).toBe(0xae426082);
    const png = flatPng(2, 2, [255, 0, 0]);
    const view = new DataView(png.buffer, png.byteOffset);
    expect(view.getUint32(29)).toBe(crc32(png.subarray(12, 29)));
  });
});
