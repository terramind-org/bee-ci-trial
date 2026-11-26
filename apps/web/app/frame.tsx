import { Suspense, type ReactNode } from "react";
import { AppShell, type AppShellProps } from "@/components/app-shell";
import { chromeData } from "./chrome-data";
import { InboxCount } from "./inbox-count";

/**
 * The app shell as every layout mounts it: the shell data for this request, the Inbox count
 * streaming into the rail, and the page. Repo pages pass `repo` (and their own org).
 */
export async function Frame({ children, repo, org }: { children: ReactNode; repo?: AppShellProps["repo"]; org?: string }) {
  const { shell, chrome } = await chromeData();
  const me = shell.me;
  return (
    <AppShell
      {...shell}
      org={org ?? shell.org}
      chrome={org ? { ...chrome, org } : chrome}
      repo={repo}
      inboxBadge={
        me ? (
          <Suspense fallback={null}>
            <InboxCount />
          </Suspense>
        ) : undefined
      }
    >
      {children}
    </AppShell>
  );
}
