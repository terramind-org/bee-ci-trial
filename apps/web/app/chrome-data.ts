/**
 * What every layout's shell and keyboard chrome need, loaded once per request: who is signed
 * in, the edition, the org and repo the palette and `g` chords resolve against, and the repos
 * the caller can switch to. Signed out, nothing is listed and no fallback repo is guessed.
 */
import type { AppShellProps, ChromeProps } from "@/components/app-shell";
import { adapterKind, currentScope, edition, getRequestScope, serverClient, sessionState } from "@/lib/api/server";
import { isRepoFullName } from "@/lib/routes";

export type ShellBase = Pick<AppShellProps, "me" | "edition" | "adapter" | "org">;

export async function chromeData(): Promise<{ shell: ShellBase; chrome: ChromeProps }> {
  const [{ me }, ed] = await Promise.all([sessionState(), edition()]);
  const inUrl = getRequestScope();
  const scope = inUrl || me ? await currentScope() : { org: "", repo: "" };
  let repos: string[] = [];
  if (me && scope.org) {
    try {
      repos = (await (await serverClient()).repo.listRepos({ org: scope.org })).repos.map((r) => r.fullName);
    } catch {
      // The switcher is optional: the page itself shows any error.
      repos = [];
    }
  }
  const repo = isRepoFullName(scope.repo) ? scope.repo : null;
  return {
    shell: { me, edition: ed, org: scope.org, adapter: adapterKind() },
    chrome: { repo, repos, org: scope.org, signedIn: !!me },
  };
}
