"use client";

import { useEffect, useState } from "react";
import { Icon } from "./icons";
import { buttonClass } from "./ui";

export const THEME_KEY = "bee-theme";

/** Runs before paint (inlined in <head>) so there is no flash of the wrong theme. Dark is the
 *  default, as in Nucleus; light only when the user picked it. */
export const THEME_SCRIPT =
  "(function(){try{var t=localStorage.getItem('" +
  THEME_KEY +
  "');document.documentElement.dataset.theme=t==='light'?'light':'dark'}catch(e){document.documentElement.dataset.theme='dark'}})()";

export function currentTheme(): "light" | "dark" {
  return document.documentElement.dataset.theme === "light" ? "light" : "dark";
}

export function setTheme(t: "light" | "dark") {
  document.documentElement.dataset.theme = t;
  try {
    localStorage.setItem(THEME_KEY, t);
  } catch {
    /* private mode */
  }
  window.dispatchEvent(new Event("bee-theme"));
}

export function toggleTheme() {
  setTheme(currentTheme() === "dark" ? "light" : "dark");
}

export function ThemeToggle() {
  const [theme, setLocal] = useState<"light" | "dark" | null>(null);
  useEffect(() => {
    const sync = () => setLocal(currentTheme());
    sync();
    window.addEventListener("bee-theme", sync);
    return () => window.removeEventListener("bee-theme", sync);
  }, []);
  return (
    <button type="button" className={buttonClass("ghost", "sm", "size-8 px-0")} onClick={toggleTheme} aria-label={theme === "dark" ? "Switch to light theme" : "Switch to dark theme"} title="Toggle theme">
      <Icon name={theme === "dark" ? "sun" : "moon"} />
    </button>
  );
}
