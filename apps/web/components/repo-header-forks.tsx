"use client";

import { usePathname, useRouter } from "next/navigation";
import { useState } from "react";
import { browserClient } from "@/lib/api/browser";
import { BeeError } from "@/lib/api/transport";
import type { Org } from "@/lib/api/types";
import { repoHref } from "@/lib/routes";
import { ConfirmButton } from "./kit/confirm";
import { Modal } from "./kit/dialog";
import { useToast } from "./toast";
import { Button, ButtonLink, Field, Input, Select } from "./ui";

function message(err: unknown): string {
  return err instanceof BeeError ? err.reason + " · " + err.message : "INTERNAL · The request could not be completed.";
}

export function ForkButton({ fullName, count, signedIn }: { fullName: string; count: number; signedIn: boolean }) {
  const path = usePathname();
  const router = useRouter();
  const [open, setOpen] = useState(false);
  const [orgs, setOrgs] = useState<Org[]>([]);
  const [loadError, setLoadError] = useState("");
  const [error, setError] = useState("");
  const [pending, setPending] = useState(false);
  const [org, setOrg] = useState("");
  const [name, setName] = useState(fullName.split("/")[1] ?? "");
  const body = <><span>Fork</span><span aria-hidden="true" className="text-ink-3">·</span><span className="num text-ink-2">{count}</span></>;

  async function listDestinations() {
    setLoadError("");
    try {
      const all: Org[] = [];
      let pageToken = "";
      do {
        const result = await browserClient().org.listOrgs({ page: { pageSize: 100, pageToken } });
        all.push(...result.orgs);
        pageToken = result.page?.nextPageToken ?? "";
      } while (pageToken);
      setOrgs(all);
    } catch (err) {
      setLoadError(message(err));
    }
  }

  async function create() {
    if (pending) return;
    setPending(true);
    setError("");
    try {
      const fork = await browserClient().repo.forkRepo({ repo: { fullName }, org, name: name.trim() });
      setOpen(false);
      router.push(repoHref(fork.fullName));
      router.refresh();
    } catch (err) {
      setError(message(err));
    } finally {
      setPending(false);
    }
  }

  if (!signedIn) {
    return <ButtonLink size="sm" href={"/signin?next=" + encodeURIComponent(path)}>{body}<span className="sr-only"> (sign in first)</span></ButtonLink>;
  }

  return (
    <>
      <Button size="sm" data-testid="repo-forks" onClick={() => {
        setError("");
        setOpen(true);
        void listDestinations();
      }}>{body}</Button>
      <Modal open={open} onOpenChange={(next) => { if (!pending) setOpen(next); }} title="Fork repository" description={fullName}>
        <form className="flex flex-col gap-4" onSubmit={(event) => {
          // The explicit confirmation is the only submission control.
          event.preventDefault();
        }}>
          <Field htmlFor="fork-owner" label="Destination owner">
            <Select id="fork-owner" disabled={pending} value={org} onChange={(event) => setOrg(event.target.value)}>
              <option value="">My account</option>
              {orgs.map((o) => <option key={o.id} value={o.name}>{o.display || o.name}</option>)}
            </Select>
          </Field>
          {loadError ? <p role="status" className="text-sm text-ink-2">Could not load organizations. {loadError}</p> : null}
          <Field htmlFor="fork-name" label="Repository name">
            <Input id="fork-name" disabled={pending} value={name} onChange={(event) => setName(event.target.value)} />
          </Field>
          {error ? <p role="alert" className="text-sm text-bad">{error}</p> : null}
          <div className="flex justify-end">
            <ConfirmButton type="button" disabled={pending} consequence="approving" agentConfirm={"Creates a fork of " + fullName + " in the selected account or organization"} onClick={create}>Create fork</ConfirmButton>
          </div>
        </form>
      </Modal>
    </>
  );
}

export function SyncForkButton({ fullName, upstream }: { fullName: string; upstream: string }) {
  const router = useRouter();
  const toast = useToast();
  return (
    <ConfirmButton
      size="sm"
      variant="secondary"
      consequence="approving"
      agentConfirm={"Updates the default branch of " + fullName + " from " + upstream}
      onClick={async () => {
        try {
          await browserClient().repo.syncFork({ repo: { fullName } });
          toast.push({ tone: "neutral", title: "Fork synced" });
          router.refresh();
        } catch (err) {
          toast.push({ tone: "bad", title: "Could not sync fork", detail: message(err) });
        }
      }}
    >Sync fork</ConfirmButton>
  );
}
