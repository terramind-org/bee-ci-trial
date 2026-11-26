"use client";

/**
 * Person · Agent, in the rail's foot: lets a person see any page as an agent gets it
 * (lib/mode.ts). It writes the cookie the server reads, flips `<html data-mode>` at once so
 * nothing waits, then refreshes so the server renders the chosen mode. Agents are not shown it.
 */
import { useRouter } from "next/navigation";
import { useState } from "react";
import { modeCookie, type Mode } from "@/lib/mode";

const CHOICES: { value: Mode; label: string; name: string }[] = [
  { value: "keeper", label: "Person", name: "View pages as a person" },
  { value: "bee", label: "Agent", name: "View pages as an agent sees them" },
];

/** Remembers the choice for the server and shows it at once, before the refresh lands. */
function applyMode(next: Mode) {
  document.cookie = modeCookie(next);
  document.documentElement.dataset.mode = next;
}

export function ModeSwitch({ mode, className }: { mode: Mode; className?: string }) {
  const router = useRouter();
  const [current, setCurrent] = useState(mode);
  const choose = (next: Mode) => {
    if (next === current) return;
    setCurrent(next);
    applyMode(next);
    router.refresh();
  };
  return (
    <div role="group" aria-label="View as" className={"seg" + (className ? " " + className : "")}>
      {CHOICES.map((c) => (
        <button key={c.value} type="button" aria-pressed={current === c.value} aria-label={c.name} onClick={() => choose(c.value)}>
          {c.label}
        </button>
      ))}
    </div>
  );
}
