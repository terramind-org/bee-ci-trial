"use client";

import { useRouter } from "next/navigation";
import { useState } from "react";
import { browserClient } from "@/lib/api/browser";
import { pathBytes } from "@/lib/api/ids";
import type { MintTokenRequest } from "@/lib/api/types";
import { CopyAs } from "./copy-as";
import { ConfirmButton, askPerson } from "./kit/confirm";
import { useToast } from "./toast";
import { Button, Field, Input, Select, buttonClass } from "./ui";

const ACTIONS = ["read", "commit", "land", "review", "lease"];
const EXPIRY: Record<string, number> = { "1 hour": 3600, "24 hours": 86_400, "7 days": 604_800 };

export function RevokeToken({ tokenId, label }: { tokenId: string; label: string }) {
  const router = useRouter();
  const toast = useToast();
  const [busy, setBusy] = useState(false);
  return (
    <span className="inline-flex items-center gap-1">
      <ConfirmButton
        agentConfirm={"Revokes " + label + " and every token derived from it; this cannot be undone"}
        consequence="destructive"
        size="sm"
        disabled={busy}
        onClick={async () => {
          if (!(await askPerson({ title: "Revoke " + label + "?", description: "Every token derived from it stops working too. This cannot be undone.", confirmLabel: "Revoke", destructive: true }))) return;
          setBusy(true);
          try {
            await browserClient().auth.revoke({ tokenId });
            toast.push({ tone: "ok", title: "Revoked " + tokenId, detail: "Derived tokens were revoked with it." });
            router.refresh();
          } catch (e) {
            toast.error(e, "Could not revoke");
          } finally {
            setBusy(false);
          }
        }}
      >
        Revoke
      </ConfirmButton>
      <CopyAs compact op={{ service: "AuthService", method: "Revoke", request: { tokenId }, label: "Revoke this token" }} />
    </span>
  );
}

/** Mint a token narrower than the agent's own. The secret is shown once and never stored here. */
export function MintTokenForm({ principalId, defaultRepo, now }: { principalId: string; defaultRepo: string; now: number }) {
  const router = useRouter();
  const toast = useToast();
  const [session, setSession] = useState("sub-agent");
  const [repos, setRepos] = useState(defaultRepo);
  const [write, setWrite] = useState("docs/");
  const [actions, setActions] = useState<string[]>(["read", "commit"]);
  const [maxCs, setMaxCs] = useState("5");
  const [maxLanded, setMaxLanded] = useState("0");
  const [expiry, setExpiry] = useState("24 hours");
  const [secret, setSecret] = useState<{ token: string; tokenId: string } | null>(null);
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);

  const req: MintTokenRequest = {
    principalId,
    scope: {
      repos: repos.split(",").map((s) => s.trim()).filter(Boolean),
      readPaths: [],
      writePaths: write.split(",").map((s) => s.trim().replace(/\/$/, "")).filter(Boolean).map(pathBytes),
      refs: ["ws/*"],
      actions,
      expiresAt: new Date(now + (EXPIRY[expiry] ?? 86_400) * 1000).toISOString(),
      budget: { maxChangesets: String(Number(maxCs) || 0), maxLanded: String(Number(maxLanded) || 0), maxUploadBytes: "0" },
      session,
    },
  };

  async function mint() {
    setBusy(true);
    setError("");
    try {
      const res = await browserClient().auth.mintToken(req);
      setSecret(res);
      router.refresh();
    } catch (e) {
      setError((e as Error).message);
      toast.error(e, "Token not minted");
    } finally {
      setBusy(false);
    }
  }

  if (secret)
    return (
      <div className="flex flex-col gap-2" role="status">
        <p className="text-sm font-medium">Token {secret.tokenId} minted. Copy it now; it is not shown again.</p>
        <code className="mono block rounded-md border border-line bg-sunken p-2.5 text-sm break-all" data-testid="minted-token">
          {secret.token}
        </code>
        <div className="flex gap-1.5">
          <button type="button" className={buttonClass("secondary", "sm")} onClick={() => void navigator.clipboard?.writeText(secret.token)}>
            Copy token
          </button>
          <Button size="sm" variant="ghost" onClick={() => setSecret(null)}>
            Done
          </Button>
        </div>
      </div>
    );

  return (
    <form
      className="grid gap-3 sm:grid-cols-2"
      onSubmit={(e) => {
        e.preventDefault();
        void mint();
      }}
      aria-label="Mint a narrowed token"
    >
      <Field label="Session label" htmlFor="mint-session" hint="Copied into provenance of everything this token writes.">
        <Input id="mint-session" value={session} onChange={(e) => setSession(e.target.value)} />
      </Field>
      <Field label="Repositories" htmlFor="mint-repos" hint="Comma separated; org/* for every repo.">
        <Input id="mint-repos" value={repos} onChange={(e) => setRepos(e.target.value)} className="mono" />
      </Field>
      <Field label="Write paths" htmlFor="mint-write" hint="Prefixes. Must sit inside the parent token's.">
        <Input id="mint-write" value={write} onChange={(e) => setWrite(e.target.value)} className="mono" />
      </Field>
      <Field label="Expires in" htmlFor="mint-expiry">
        <Select id="mint-expiry" value={expiry} onChange={(e) => setExpiry(e.target.value)}>
          {Object.keys(EXPIRY).map((k) => (
            <option key={k}>{k}</option>
          ))}
        </Select>
      </Field>
      <fieldset className="sm:col-span-2">
        <legend className="mb-1.5 text-sm font-medium">Actions</legend>
        <div className="flex flex-wrap gap-3">
          {ACTIONS.map((a) => (
            <label key={a} className="flex items-center gap-1.5 text-sm">
              <input type="checkbox" checked={actions.includes(a)} onChange={(e) => setActions((xs) => (e.target.checked ? [...xs, a] : xs.filter((x) => x !== a)))} className="accent-[var(--accent)]" />
              <span className="mono">{a}</span>
            </label>
          ))}
        </div>
      </fieldset>
      <Field label="Budget: commits" htmlFor="mint-cs">
        <Input id="mint-cs" type="number" min={0} value={maxCs} onChange={(e) => setMaxCs(e.target.value)} />
      </Field>
      <Field label="Budget: merged commits" htmlFor="mint-landed">
        <Input id="mint-landed" type="number" min={0} value={maxLanded} onChange={(e) => setMaxLanded(e.target.value)} />
      </Field>
      {error ? (
        <p className="text-sm text-bad sm:col-span-2" role="alert">
          {error}
        </p>
      ) : null}
      <div className="flex items-center gap-1.5 sm:col-span-2">
        <Button type="submit" variant="primary" icon="key" disabled={busy || actions.length === 0}>
          {busy ? "Minting…" : "Mint token"}
        </Button>
        <CopyAs op={{ service: "AuthService", method: "MintToken", request: req, label: "Mint this token" }} />
      </div>
    </form>
  );
}
