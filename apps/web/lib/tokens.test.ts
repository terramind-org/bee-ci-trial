import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";
import { MONO_STACK, SANS_STACK } from "../app/fonts";

/**
 * The design tokens, checked where they are declared (app/globals.css). This reads the colour
 * pairs out of the stylesheet and computes WCAG contrast for every token used as text against
 * every ground text is set on. It does not render a page: a component that puts text on some
 * other ground is caught by the axe runs in e2e, not here.
 */
const read = (rel: string) => readFileSync(fileURLToPath(new URL(rel, import.meta.url)), "utf8");
const css = read("../app/globals.css");

type Rgba = [number, number, number, number];
type Theme = 0 | 1;

function parseColor(text: string): Rgba | null {
  const t = text.trim();
  const hex = /^#([0-9a-f]{6})$/i.exec(t);
  if (hex) {
    const n = parseInt(hex[1] as string, 16);
    return [(n >> 16) & 255, (n >> 8) & 255, n & 255, 1];
  }
  const rgb = /^rgb\((\d+) (\d+) (\d+)(?: \/ ([\d.]+))?\)$/.exec(t);
  if (rgb) return [Number(rgb[1]), Number(rgb[2]), Number(rgb[3]), rgb[4] === undefined ? 1 : Number(rgb[4])];
  if (t === "transparent") return [0, 0, 0, 0];
  return null;
}

/** `--name: light-dark(a, b);` and `--name: var(--other);` in `:root`. */
const decls = new Map<string, string>();
for (const m of css.matchAll(/^\s*--([\w-]+):\s*([^;]+);/gm)) if (!decls.has(m[1] as string)) decls.set(m[1] as string, (m[2] as string).trim());

function token(name: string, theme: Theme): Rgba {
  const value = decls.get(name);
  if (!value) throw new Error("no token --" + name);
  const alias = /^var\(--([\w-]+)\)$/.exec(value);
  if (alias) return token(alias[1] as string, theme);
  const pair = /^light-dark\((.+)\)$/.exec(value);
  if (!pair) throw new Error("--" + name + " is not a light-dark() pair: " + value);
  // Split on the comma between the two colours (neither colour form used here contains one).
  const parts = (pair[1] as string).split(",").map((p) => p.trim());
  if (parts.length !== 2) throw new Error("--" + name + ": expected two colours, got " + value);
  const colour = parseColor(parts[theme] as string);
  if (!colour) throw new Error("--" + name + ": cannot read " + parts[theme]);
  return colour;
}

const over = (top: Rgba, under: Rgba): Rgba => [0, 1, 2].map((i) => top[i]! * top[3] + under[i]! * (1 - top[3])).concat(1) as Rgba;
const lin = (c: number) => (c / 255 <= 0.04045 ? c / 255 / 12.92 : ((c / 255 + 0.055) / 1.055) ** 2.4);
const luminance = (c: Rgba) => 0.2126 * lin(c[0]) + 0.7152 * lin(c[1]) + 0.0722 * lin(c[2]);
function contrast(a: Rgba, b: Rgba): number {
  const [hi, lo] = [luminance(a), luminance(b)].sort((x, y) => y - x) as [number, number];
  return (hi + 0.05) / (lo + 0.05);
}

const THEMES: [string, Theme][] = [
  ["light", 0],
  ["dark", 1],
];
/** Every token that sets text. */
const TEXT = ["ink", "ink-2", "ink-3", "ok", "warn", "bad", "accent-ink", "sh-class", "sh-property", "sh-entity", "sh-string", "sh-keyword", "sh-comment", "sh-sign"];
/** Every opaque ground text sits on. */
const GROUNDS = ["bg", "surface", "sunken", "hover", "selected", "popover"];

describe("design tokens", () => {
  it("declares UI SDK's cascade layer first, as its styling guide requires", () => {
    expect(css.startsWith("@layer ui-sdk;\n")).toBe(true);
    expect(css.indexOf("@layer ui-sdk;")).toBeLessThan(css.indexOf('@import "tailwindcss"'));
  });

  it("uses the prototype's paper, ink and mark", () => {
    expect(decls.get("bg")).toBe("light-dark(#f4f2ec, #0f0f0e)");
    expect(decls.get("ink")).toBe("light-dark(#16171a, #f4f3ed)");
    expect(decls.get("mark")).toBe("light-dark(#f0cb45, #f4d463)");
    expect(decls.get("mark-ring")).toBe("light-dark(rgb(22 23 26 / 0.55), transparent)");
  });

  it("makes the accent ink, never a hue of its own", () => {
    for (const name of ["accent", "accent-ink", "focus"]) expect(decls.get(name)).toBe("var(--ink)");
    expect(decls.get("on-accent")).toBe("var(--bg)");
  });

  for (const [themeName, theme] of THEMES) {
    it("keeps every text colour at 4.5:1 or better on every ground, " + themeName, () => {
      const failures: string[] = [];
      for (const ground of GROUNDS) {
        const g = token(ground, theme);
        for (const name of TEXT) {
          const ratio = contrast(token(name, theme), g);
          if (ratio < 4.5) failures.push(name + " on " + ground + ": " + ratio.toFixed(2));
        }
      }
      expect(failures).toEqual([]);
    });

    it("keeps status words readable on their own tint, and button text on solid ink, " + themeName, () => {
      const bg = token("bg", theme);
      for (const status of ["ok", "warn", "bad"]) expect(contrast(token(status, theme), over(token(status + "-soft", theme), bg))).toBeGreaterThanOrEqual(4.5);
      expect(contrast(token("on-accent", theme), token("accent", theme))).toBeGreaterThanOrEqual(4.5);
      expect(contrast(token("on-accent", theme), token("accent-hover", theme))).toBeGreaterThanOrEqual(4.5);
      // Text inside a diff line sits on the add or delete tint.
      for (const tint of ["add-bg", "del-bg"]) for (const name of ["ink", "ink-2", "ink-3"]) expect(contrast(token(name, theme), over(token(tint, theme), bg))).toBeGreaterThanOrEqual(4.5);
    });
  }

  it("keeps the yellow mark out of text on the light theme: it could not be read there", () => {
    // The reason for the rule, as a number: the mark on paper is far below any text threshold.
    expect(contrast(token("mark", 0), token("bg", 0))).toBeLessThan(3);
    expect(css).not.toMatch(/(^|[^-])color:\s*var\(--mark\)/);
  });

  it("uses the system's faces, the same stacks app/fonts.ts exports", () => {
    expect(decls.get("sans")).toBe(SANS_STACK);
    expect(decls.get("mono")).toBe(MONO_STACK);
    expect(css).not.toContain("@font-face");
  });

  it("gives the agent's view mono text and no motion", () => {
    expect(css).toMatch(/\[data-mode="bee"\] main \{[^}]*font-family: var\(--mono\)/);
    expect(css).toMatch(/\[data-mode="bee"\] \*,[^{]*\{[^}]*animation: none !important;[^}]*transition: none !important/);
  });

  it("does not name another product in the stylesheet, the fonts or the token reference", () => {
    for (const file of ["../app/globals.css", "../app/fonts.ts", "../../../docs/design/tokens.md"]) expect(read(file)).not.toMatch(/nucleus|terramind|trellis|geist/i);
  });
});
