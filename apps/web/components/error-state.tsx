"use client";

import { usePathname, useRouter } from "next/navigation";
import type { ReactNode } from "react";
import { Icon, type IconName } from "./icons";
import { Button, ButtonLink, EmptyState } from "./ui";

const HINTS: Record<string, string> = {
  UNAVAILABLE: "The bee server did not answer. Your work is safe; retry in a moment.",
  UNAUTHENTICATED: "Your session has ended. Sign in again.",
  INVALID_TOKEN: "This token is not valid for this server. Sign in again.",
  TOKEN_REVOKED: "This token was revoked. Sign in again or ask its owner for a new one.",
  PERMISSION_DENIED: "Your token does not allow this. Ask an owner to widen its scope.",
  POLICY_DENIED: "Policy does not allow this yet. The missing items are listed where the action lives.",
};

/**
 * What the screen shows when the server answers UNIMPLEMENTED: the feature is not served by this
 * server yet. Nothing is faked; the reason and the RPC are shown as they came back.
 */
export function Unavailable({ what, message, reason = "UNIMPLEMENTED" }: { what: string; message: string; reason?: string }) {
  return (
    <div role="status" data-testid="unavailable" className="mx-auto my-10 flex max-w-lg flex-col items-center gap-3 rounded-lg border border-dashed border-line-strong bg-transparent px-6 py-10 text-center">
      <span className="grid size-10 place-items-center rounded-full bg-sunken text-ink-2">
        <Icon name="lock" />
      </span>
      <p className="text-md font-semibold">{what} is not available on this server</p>
      <p className="text-sm text-ink-2 [overflow-wrap:anywhere]">{message}</p>
      <p className="mono text-xs text-ink-3">reason: {reason}</p>
    </div>
  );
}

/** Where "Sign in" goes from `path`: the sign-in page, coming back here afterwards. */
export function signInHref(path: string): string {
  return "/signin" + (path && path !== "/" ? "?next=" + encodeURIComponent(path) : "");
}

/** A part of a public repo that only signed-in members see, shown to a signed-out visitor. */
export function SignInToSee({ title, next, icon = "lock", children }: { title: string; next: string; icon?: IconName; children?: ReactNode }) {
  return (
    <EmptyState
      title={title}
      icon={icon}
      action={
        <ButtonLink href={signInHref(next)} variant="primary">
          Sign in
        </ButtonLink>
      }
    >
      {children}
    </EmptyState>
  );
}

/**
 * A page-sized error. `signedOut` says the request carried no session: UNAUTHENTICATED then means
 * "this needs an account", not "your session has ended".
 */
export function ErrorState({ reason, message, retryable = false, signedOut = false }: { reason: string; message: string; retryable?: boolean; signedOut?: boolean }) {
  const router = useRouter();
  const path = usePathname();
  // The server's message names the RPC it lacks; a person gets the plain version.
  if (reason === "UNIMPLEMENTED") return <Unavailable what="This screen" message="This server does not offer it yet." />;
  if (signedOut && reason === "UNAUTHENTICATED") {
    return (
      <div className="mx-auto my-10 max-w-lg rounded-lg border border-line bg-surface">
        <SignInToSee title="Sign in to see this" next={path ?? "/"}>
          This part of Beehive needs an account. Public code, history and issues stay open without one.
        </SignInToSee>
      </div>
    );
  }
  return (
    <div role="alert" className="mx-auto my-10 flex max-w-lg flex-col items-center gap-3 rounded-lg border border-line bg-surface px-6 py-10 text-center">
      <span className="grid size-10 place-items-center rounded-full bg-bad-soft text-bad">
        <Icon name="alert" />
      </span>
      <p className="text-md font-semibold">{HINTS[reason] ? HINTS[reason] : "Something went wrong"}</p>
      <p className="text-sm text-ink-2 [overflow-wrap:anywhere]">{message}</p>
      <p className="mono text-xs text-ink-3">reason: {reason}</p>
      <div className="mt-1 flex gap-2">
        <Button variant={retryable ? "primary" : "secondary"} icon="refresh" onClick={() => router.refresh()}>
          Try again
        </Button>
        {reason.includes("TOKEN") || reason === "UNAUTHENTICATED" ? (
          <Button onClick={() => router.push("/signin")}>Sign in</Button>
        ) : null}
      </div>
    </div>
  );
}
