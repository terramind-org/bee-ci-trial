"use client";

import { useRouter, useSearchParams } from "next/navigation";
import { useEffect, useState } from "react";
import { browserClient } from "@/lib/api/browser";
import type { Edition } from "@/lib/api/server";
import type { LoginResponse } from "@/lib/api/types";
import { BeeError } from "@/lib/api/transport";
import { safeNext } from "@/lib/routes";
import { CopyAsPanel } from "./copy-as-panel";
import { Icon } from "./icons";
import { Button, Field, Input, buttonClass } from "./ui";

/** A GitHub access token (classic, OAuth, app or fine-grained), as opposed to a Beehive token. */
const GITHUB_TOKEN = /^(gh[pousr]_|github_pat_)/;

async function establish(token: string): Promise<void> {
  const res = await fetch("/api/session", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ token }) });
  if (!res.ok) throw BeeError.fromJSON(await res.json().catch(() => ({})), res.status);
}

export function SignIn({ edition }: { edition: Edition }) {
  const router = useRouter();
  // Back to the page that sent the visitor here; without one, home, or onboarding for a new
  // account on the cloud edition (the server creates the account on first sign-in).
  const explicit = safeNext(useSearchParams().get("next"));
  const cloud = edition === "cloud";
  const [token, setToken] = useState("");
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  const [device, setDevice] = useState<LoginResponse | null>(null);
  const [status, setStatus] = useState("");

  async function destination(): Promise<string> {
    if (explicit) return explicit;
    if (!cloud) return "/";
    try {
      const api = browserClient();
      const [orgs, repos] = await Promise.all([api.org.listOrgs({}), api.repo.listMyRepos({})]);
      if (!orgs.orgs.length && !repos.repos.length) return "/new";
    } catch {
      // Not knowing is not a reason to hold the visitor here.
    }
    return "/";
  }

  async function withToken(t: string) {
    setBusy(true);
    setError("");
    try {
      const pasted = t.trim();
      // A pasted GitHub token is exchanged for a Beehive one; the server creates the account on first login.
      await establish(GITHUB_TOKEN.test(pasted) ? (await browserClient().auth.login({ githubToken: pasted })).token : t);
      router.push(await destination());
      router.refresh();
    } catch (e) {
      setError(e instanceof BeeError ? e.reason + ": " + e.message : String(e));
    } finally {
      setBusy(false);
    }
  }

  useEffect(() => {
    if (!device) return;
    let alive = true;
    const poll = async () => {
      while (alive) {
        await new Promise((r) => setTimeout(r, Math.max(1, device.intervalSeconds) * 1000));
        if (!alive) return;
        try {
          const res = await browserClient().auth.login({ githubDeviceCode: device.deviceCode });
          if (res.token) {
            setStatus("Approved. Signing you in…");
            await withToken(res.token);
            return;
          }
        } catch (e) {
          if (e instanceof BeeError && e.reason === "AUTHORIZATION_PENDING") {
            setStatus("Waiting for you to approve on GitHub…");
            continue;
          }
          setError(e instanceof BeeError ? e.reason + ": " + e.message : String(e));
          setDevice(null);
          return;
        }
      }
    };
    void poll();
    return () => {
      alive = false;
    };
    // withToken is stable enough for this flow; re-running on its identity would restart polling.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [device]);

  return (
    <div className="flex flex-col gap-6">
      <section aria-labelledby="gh-title">
        <h2 id="gh-title" className="text-xl font-semibold tracking-tight">
          {cloud ? "Create your account with GitHub" : "Sign in with GitHub"}
        </h2>
        <p className="mt-1 text-base text-ink-2">
          {cloud ? "New here? Signing in creates your account. " : ""}Device flow: no password passes through Beehive. Your identity is your stable GitHub id, never your login.
        </p>
        {device ? (
          <div className="mt-3 flex flex-col gap-2" role="status" aria-live="polite">
            <p className="text-base">
              Open{" "}
              <a href={device.verificationUri} target="_blank" rel="noreferrer noopener" className="link">
                {device.verificationUri}
              </a>{" "}
              and enter
            </p>
            <p className="mono text-2xl font-semibold tracking-widest" data-testid="user-code">
              {device.userCode}
            </p>
            <p className="text-base text-ink-2">{status || "Waiting…"}</p>
            <Button size="sm" variant="ghost" onClick={() => setDevice(null)}>
              Cancel
            </Button>
          </div>
        ) : (
          <Button
            className="mt-3"
            variant="primary"
            onClick={async () => {
              setError("");
              try {
                setDevice(await browserClient().auth.login({ githubStart: true }));
              } catch (e) {
                if (e instanceof BeeError && e.reason === "UNIMPLEMENTED") setError("This server has no GitHub app configured for sign-in (UNIMPLEMENTED). Paste a token instead.");
                else setError(e instanceof BeeError ? e.reason + ": " + e.message : String(e));
              }
            }}
          >
            Continue with GitHub
          </Button>
        )}
      </section>

      <section aria-labelledby="tok-title" className="rule pt-6">
        <h2 id="tok-title" className="text-xl font-semibold tracking-tight">
          Paste a token
        </h2>
        <p className="mt-1 text-base text-ink-2">
          Any Beehive token works, including one minted for an agent. {cloud ? "A GitHub token works too and creates your account. " : ""}The UI then shows exactly what that token can see.
        </p>
        <form
          className="mt-3 flex flex-col gap-3"
          onSubmit={(e) => {
            e.preventDefault();
            void withToken(token);
          }}
        >
          <Field label="Token" htmlFor="token" error={error || undefined}>
            <Input id="token" type="password" value={token} onChange={(e) => setToken(e.target.value)} autoComplete="off" className="mono" placeholder="swt_…" />
          </Field>
          <Button type="submit" disabled={!token.trim() || busy}>
            {busy ? "Checking…" : "Sign in with token"}
          </Button>
        </form>
      </section>

      <details className="rule pt-4">
        <summary className={buttonClass("ghost", "md", "-ml-2.5 justify-start")}>
          <Icon name="terminal" /> Signing in an agent instead?
        </summary>
        <div className="pt-4">
          <CopyAsPanel op={{ service: "AuthService", method: "WhoAmI", request: {}, label: "Check a token" }} />
        </div>
      </details>
    </div>
  );
}
