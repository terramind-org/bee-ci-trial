"use client";

/**
 * The repositories this browser opened last, for the rail's "Recent" group. Kept in
 * localStorage, so it is true to what the person did here and costs the server nothing; a new
 * browser starts with none. Opening a repository page moves it to the front.
 */
import { useEffect, useSyncExternalStore } from "react";
import { withRecent } from "@/lib/nav";
import { isRepoFullName } from "@/lib/routes";

const KEY = "bee-recent-repos";
const EVENT = "bee-recent-repos";
const NONE: string[] = [];

let cache: { raw: string | null; list: string[] } = { raw: null, list: NONE };

function read(): string[] {
  let raw: string | null = null;
  try {
    raw = localStorage.getItem(KEY);
  } catch {
    return NONE; // private mode
  }
  if (raw === cache.raw) return cache.list;
  let list = NONE;
  try {
    const parsed: unknown = JSON.parse(raw ?? "[]");
    if (Array.isArray(parsed)) list = parsed.filter((r): r is string => typeof r === "string" && isRepoFullName(r)).slice(0, 5);
  } catch {
    list = NONE;
  }
  cache = { raw, list };
  return list;
}

function subscribe(onChange: () => void): () => void {
  window.addEventListener(EVENT, onChange);
  window.addEventListener("storage", onChange);
  return () => {
    window.removeEventListener(EVENT, onChange);
    window.removeEventListener("storage", onChange);
  };
}

/** Records `fullName` (when on a repository page) and returns the list, newest first. */
export function useRecentRepos(fullName: string | undefined): string[] {
  useEffect(() => {
    if (!fullName) return;
    try {
      localStorage.setItem(KEY, JSON.stringify(withRecent(read(), fullName)));
    } catch {
      return;
    }
    window.dispatchEvent(new Event(EVENT));
  }, [fullName]);
  return useSyncExternalStore(subscribe, read, () => NONE);
}
