"use client";

import { useEffect, useState } from "react";

export const THEME_KEY = "bee-theme";

export type Theme = "light" | "dark";
/** What the person chose: one theme, or to follow the system (nothing stored). */
export type ThemeChoice = Theme | "system";

/**
 * The theme the page should show: the stored choice when there is one, else the system's. Pure,
 * so the before-paint script and the tests share one rule.
 */
export function resolveTheme(stored: string | null | undefined, systemDark: boolean): Theme {
  if (stored === "light" || stored === "dark") return stored;
  return systemDark ? "dark" : "light";
}

/**
 * Runs before paint (inlined at the top of <body>) so the first frame is already right. The
 * colours follow the system on their own (`light-dark()` in app/globals.css); this pins the
 * person's stored choice and writes the resolved theme to `data-theme` for scripts and `dark:`.
 */
export const THEME_SCRIPT =
  "(function(){var d=document.documentElement,m=window.matchMedia('(prefers-color-scheme: dark)'),s=null;" +
  "try{s=localStorage.getItem('" +
  THEME_KEY +
  "')}catch(e){}" +
  "d.dataset.theme=s==='light'||s==='dark'?s:m.matches?'dark':'light'})()";

function systemDark(): boolean {
  return typeof window !== "undefined" && window.matchMedia("(prefers-color-scheme: dark)").matches;
}

function stored(): string | null {
  try {
    return localStorage.getItem(THEME_KEY);
  } catch {
    return null; // private mode
  }
}

export function currentTheme(): Theme {
  return document.documentElement.dataset.theme === "dark" ? "dark" : "light";
}

export function currentChoice(): ThemeChoice {
  const s = stored();
  return s === "light" || s === "dark" ? s : "system";
}

/** Pins a theme, or with "system" forgets the choice and follows the system again. */
export function setTheme(choice: ThemeChoice) {
  try {
    if (choice === "system") localStorage.removeItem(THEME_KEY);
    else localStorage.setItem(THEME_KEY, choice);
  } catch {
    /* private mode: the choice lasts for this page */
  }
  document.documentElement.dataset.theme = choice === "system" ? resolveTheme(null, systemDark()) : choice;
  window.dispatchEvent(new Event("bee-theme"));
}

export function toggleTheme() {
  setTheme(currentTheme() === "dark" ? "light" : "dark");
}

/** Keeps `data-theme` in step with the system while nothing is stored. Mounted once by the switch. */
function useFollowSystem() {
  useEffect(() => {
    const media = window.matchMedia("(prefers-color-scheme: dark)");
    const onChange = () => {
      if (currentChoice() !== "system") return;
      document.documentElement.dataset.theme = resolveTheme(null, media.matches);
      window.dispatchEvent(new Event("bee-theme"));
    };
    media.addEventListener("change", onChange);
    return () => media.removeEventListener("change", onChange);
  }, []);
}

const CHOICES: { value: ThemeChoice; label: string; name: string }[] = [
  { value: "system", label: "System", name: "Follow the system theme" },
  { value: "light", label: "Light", name: "Switch to light theme" },
  { value: "dark", label: "Dark", name: "Switch to dark theme" },
];

/** System · Light · Dark, as one pill. System follows the device and is the default. */
export function ThemeSwitch({ className }: { className?: string }) {
  const [choice, setChoice] = useState<ThemeChoice | null>(null);
  useFollowSystem();
  useEffect(() => {
    const sync = () => setChoice(currentChoice());
    sync();
    window.addEventListener("bee-theme", sync);
    return () => window.removeEventListener("bee-theme", sync);
  }, []);
  return (
    <div role="group" aria-label="Theme" className={"seg" + (className ? " " + className : "")}>
      {CHOICES.map((c) => (
        <button key={c.value} type="button" aria-pressed={choice === c.value} aria-label={c.name} onClick={() => setTheme(c.value)}>
          {c.label}
        </button>
      ))}
    </div>
  );
}

/** The switch under its old name, for callers written before it had three positions. */
export const ThemeToggle = ThemeSwitch;
