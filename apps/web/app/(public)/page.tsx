import type { Metadata } from "next";
import { Inbox } from "@/components/inbox/inbox";
import { SignedOut } from "@/components/home/signed-out";
import { ErrorState } from "@/components/error-state";
import { edition, sessionState } from "@/lib/api/server";

export async function generateMetadata(): Promise<Metadata> {
  const { me } = await sessionState();
  return { title: me ? "Inbox" : "Home" };
}

/**
 * `/`: the Inbox for a signed-in caller, the instance landing for everyone else. A server that
 * cannot be reached says so rather than passing for "signed out".
 */
export default async function HomePage() {
  const [{ me, error }, ed] = await Promise.all([sessionState(), edition()]);
  if (me) return <Inbox />;
  if (error && error.code === "unavailable") return <ErrorState reason={error.reason} message={error.message} retryable />;
  return <SignedOut edition={ed} />;
}
