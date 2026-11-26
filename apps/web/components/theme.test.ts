import { describe, expect, it } from "vitest";
import { THEME_KEY, THEME_SCRIPT, resolveTheme } from "./theme";

/** Runs the before-paint script against a stand-in for the browser and returns `data-theme`. */
function paint(stored: string | null | "throws", systemDark: boolean): string | undefined {
  const dataset: Record<string, string> = {};
  const env = {
    document: { documentElement: { dataset } },
    window: { matchMedia: (q: string) => ({ matches: q.includes("dark") && systemDark }) },
    localStorage: {
      getItem: (key: string) => {
        if (stored === "throws") throw new Error("private mode");
        return key === THEME_KEY ? stored : null;
      },
    },
  };
  new Function("document", "window", "localStorage", THEME_SCRIPT)(env.document, env.window, env.localStorage);
  return dataset.theme;
}

describe("theme", () => {
  it("follows the system when nothing is stored", () => {
    expect(resolveTheme(null, true)).toBe("dark");
    expect(resolveTheme(null, false)).toBe("light");
    expect(resolveTheme(undefined, false)).toBe("light");
  });

  it("lets the stored choice win over the system", () => {
    expect(resolveTheme("light", true)).toBe("light");
    expect(resolveTheme("dark", false)).toBe("dark");
  });

  it("ignores a stored value it does not know", () => {
    expect(resolveTheme("system", true)).toBe("dark");
    expect(resolveTheme("blue", false)).toBe("light");
  });

  it("sets the same theme before paint as the rule says, for every case", () => {
    for (const stored of [null, "light", "dark", "blue"] as const) {
      for (const systemDark of [true, false]) expect(paint(stored, systemDark)).toBe(resolveTheme(stored, systemDark));
    }
  });

  it("still follows the system when storage is blocked", () => {
    expect(paint("throws", true)).toBe("dark");
    expect(paint("throws", false)).toBe("light");
  });
});
