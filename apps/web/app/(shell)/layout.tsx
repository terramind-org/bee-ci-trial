import { headers } from "next/headers";
import { redirect } from "next/navigation";
import { Suspense, type ReactNode } from "react";
import { adapterKind, sessionState } from "@/lib/api/server";
import { PATH_HEADER, safeNext } from "@/lib/routes";
import { Frame } from "../frame";
import { GlobalLoading } from "../layout-skeletons";

/** Pages that need a session (import, org settings, admin): signed out goes to sign-in and back. */
export default async function ShellLayout({ children }: { children: ReactNode }) {
  const { me, error } = await sessionState();
  const rejected = error && (error.code === "unauthenticated" || error.reason.includes("TOKEN"));
  if (adapterKind() === "http" && !me && (!error || rejected)) {
    const next = safeNext((await headers()).get(PATH_HEADER));
    redirect("/signin" + (next ? "?next=" + encodeURIComponent(next) : ""));
  }
  // An unreachable server still renders the shell; each page shows its own typed error state.
  // The sign-in redirect above is decided before anything streams, so it stays a real redirect.
  return (
    <Suspense fallback={<GlobalLoading />}>
      <Frame>{children}</Frame>
    </Suspense>
  );
}
