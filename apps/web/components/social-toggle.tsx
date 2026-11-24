"use client";

import Link from "next/link";
import { usePathname } from "next/navigation";
import { useState, useTransition } from "react";
import type { ServiceName } from "@/lib/api/methods";
import type { Op } from "@/lib/copyas";
import { CopyAs } from "./copy-as";
import { Icon, type IconName } from "./icons";
import { Swap } from "./motion";
import type { SocialResult } from "./social-actions";
import { useToast } from "./toast";
import { buttonClass, cx } from "./ui";

/**
 * The shared body of the star and follow buttons: an on/off toggle with an optional count,
 * applied optimistically and rolled back with the server's reason when the call fails.
 */
export interface SocialToggleProps {
  on: boolean;
  count?: number;
  icon: IconName;
  labels: { on: string; off: string };
  /** What the toggle acts on (`acme/api`, `mara`), read after the visible label by screen readers. */
  subject: string;
  /** Signed out: the control becomes a link to sign-in that comes back here. */
  signedIn: boolean;
  /** Set when the server cannot do this (e.g. UNIMPLEMENTED); the control is disabled and says why. */
  unavailable?: string;
  act: (on: boolean) => Promise<SocialResult>;
  /** The UserService call each direction makes, for Copy as. */
  op: (on: boolean) => { method: string; request: unknown; label: string };
  /** Show the inline Copy-as button. Headers turn it off: their "…" menu is the page's one Copy as. */
  copyAs?: boolean;
}

/**
 * UserService joins the typed method table with the community data methods (foundation-data);
 * until then Copy as names the RPC directly, which is what it renders anyway.
 */
const USER_SERVICE = "UserService" as ServiceName;

export function SocialToggle({ on: initial, count: initialCount, icon, labels, subject, signedIn, unavailable, act, op, copyAs = true }: SocialToggleProps) {
  const [on, setOn] = useState(initial);
  const [count, setCount] = useState(initialCount);
  const [pending, start] = useTransition();
  const toast = useToast();
  const path = usePathname();
  const next = op(!on);
  const copy: Op = { service: USER_SERVICE, method: next.method, request: next.request, label: next.label };
  const body = (
    <>
      <Icon name={icon} size={16} weight={on ? "fill" : "regular"} className={on ? "text-ink" : undefined} />
      <span>{on ? labels.on : labels.off}</span>
      <span className="sr-only">{" " + subject}</span>
      {count !== undefined ? (
        <Swap value={count} className="num border-l border-line pl-1.5 text-xs text-ink-3">
          {count}
        </Swap>
      ) : null}
    </>
  );
  if (!signedIn) {
    return (
      <Link href={"/signin?next=" + encodeURIComponent(path ?? "/")} className={buttonClass("secondary", "sm")}>
        {body}
        <span className="sr-only"> (sign in first)</span>
      </Link>
    );
  }
  return (
    <span className="inline-flex items-center gap-1">
      <button
        type="button"
        className={buttonClass("secondary", "sm", cx(on && "bg-selected"))}
        aria-pressed={on}
        title={unavailable}
        disabled={pending || !!unavailable}
        onClick={() => {
          const want = !on;
          setOn(want);
          if (count !== undefined) setCount(Math.max(0, count + (want ? 1 : -1)));
          start(async () => {
            const res = await act(want);
            if (!res.ok) {
              setOn(!want);
              setCount(count);
              toast.push({ tone: "bad", title: (want ? labels.off : labels.on) + " failed · " + res.reason, detail: res.message });
            }
          });
        }}
      >
        {body}
      </button>
      {unavailable || !copyAs ? null : <CopyAs compact op={copy} />}
    </span>
  );
}
