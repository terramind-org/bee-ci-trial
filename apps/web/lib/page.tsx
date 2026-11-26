/**
 * Server-side page helpers. `guard` renders a typed error state for BeeErrors (so the user
 * sees the SPEC §12.2 reason) and maps not-found reasons to the 404 page. Everything else is
 * rethrown: Next's own control-flow errors (redirect, notFound, dynamic bailout) must pass.
 */
import { notFound } from "next/navigation";
import type { ReactNode } from "react";
import { ErrorState, Unavailable } from "@/components/error-state";
import { sessionToken } from "./api/server";
import { BeeError } from "./api/transport";

export async function guard(render: () => Promise<ReactNode>): Promise<ReactNode> {
  try {
    return await render();
  } catch (err) {
    if (err instanceof BeeError) return failure(err);
    throw err;
  }
}

/**
 * `guard` for one part of a page. The page header and tabs render outside it, so a part the
 * server cannot serve (UNIMPLEMENTED) leaves a plain card where it would be and nothing else
 * changes. `what` names the part in words a person uses ("Apps"), never an RPC. Every other
 * error is handled exactly as `guard` handles it.
 */
export async function guardSection(what: string, render: () => Promise<ReactNode>): Promise<ReactNode> {
  try {
    return await render();
  } catch (err) {
    if (err instanceof BeeError) {
      if (err.code === "unimplemented") return <Unavailable what={what} message="This server does not offer it yet. The rest of this page works as usual." />;
      return failure(err);
    }
    throw err;
  }
}

async function failure(err: BeeError): Promise<ReactNode> {
  if (err.code === "not_found") notFound();
  // No session token at all: UNAUTHENTICATED asks a visitor to sign in, not to sign in again.
  const signedOut = err.code === "unauthenticated" && !(await sessionToken());
  return <ErrorState reason={err.reason} message={err.message} retryable={err.retryable} signedOut={signedOut} />;
}

export type SearchParams = Promise<Record<string, string | string[] | undefined>>;

export function param(sp: Record<string, string | string[] | undefined>, key: string): string {
  const v = sp[key];
  return Array.isArray(v) ? (v[0] ?? "") : (v ?? "");
}
