import { spawnSync } from "node:child_process";
import { readFileSync, statSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";

const script = fileURLToPath(new URL("../../scripts/evidence-media.mjs", import.meta.url));
const media = (name: string) => fileURLToPath(new URL("./media/" + name, import.meta.url));

describe("the seed's evidence media", () => {
  it("shows what scripts/evidence-media.mjs draws, pixel for pixel", () => {
    const run = spawnSync("node", [script, "--check"], { encoding: "utf8" });
    expect(run.stderr).toBe("");
    expect(run.status).toBe(0);
  });

  it("has PNG frames of the sizes the seed declares", () => {
    for (const name of ["ui-before.png", "ui-after.png", "ui-first-try.png", "ui-poster.png"]) {
      const png = readFileSync(media(name));
      expect([...png.subarray(0, 8)]).toEqual([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]);
      expect([png.readUInt32BE(16), png.readUInt32BE(20)]).toEqual([960, 600]);
    }
    expect(readFileSync(media("ui-before.png")).equals(readFileSync(media("ui-after.png")))).toBe(false);
  });

  it("commits a webm of at most 400 KB and captions that cover it", () => {
    const webm = readFileSync(media("ui-walkthrough.webm"));
    expect(webm.readUInt32BE(0)).toBe(0x1a45dfa3);
    expect(statSync(media("ui-walkthrough.webm")).size).toBeLessThanOrEqual(400 * 1024);
    const vtt = readFileSync(media("ui-walkthrough.vtt"), "utf8");
    expect(vtt.startsWith("WEBVTT\n")).toBe(true);
    expect(vtt).toContain("00:00:06.000 --> 00:00:08.000");
  });
});
