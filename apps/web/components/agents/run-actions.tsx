"use client";

import { useRouter } from "next/navigation";
import { useId, useState, type FormEvent } from "react";
import { askPerson, Button, ConfirmButton, Field, Textarea } from "@/components/kit";
import { useToast } from "@/components/toast";
import { browserClient } from "@/lib/api/browser";
import { repoHref } from "@/lib/routes";

export function AgentRunActions({ repo, runId, finished }: { repo: string; runId: string; finished: boolean }) {
  const router = useRouter();
  const toast = useToast();
  const [busy, setBusy] = useState(false);

  async function stop() {
    setBusy(true);
    try {
      if (!await askPerson({
        title: "Stop this run?",
        description: "Stopping ends the run. You can start a new run from the same task with Retry.",
        confirmLabel: "Stop run",
        cancelLabel: "Keep run",
        destructive: true,
      })) return;
      await browserClient().agent.cancelAgentRun({ id: runId });
      router.refresh();
    } catch (error) {
      toast.error(error, "Could not stop the run");
    } finally {
      setBusy(false);
    }
  }

  async function retry() {
    setBusy(true);
    try {
      const retry = await browserClient().agent.retryAgentRun({ id: runId });
      if (retry.id) router.push(repoHref(repo, "agents/runs/" + encodeURIComponent(retry.id)));
    } catch (error) {
      toast.error(error, "Could not retry the run");
    } finally {
      setBusy(false);
    }
  }

  return finished ? (
    <Button size="sm" disabled={busy} onClick={retry}>Retry</Button>
  ) : (
    <ConfirmButton size="sm" disabled={busy} onClick={stop} consequence="destructive" agentConfirm="Stops this run permanently.">
      Stop
    </ConfirmButton>
  );
}

export function RunMessageForm({ runId }: { runId: string }) {
  const id = useId();
  const [body, setBody] = useState("");
  const [busy, setBusy] = useState(false);
  const router = useRouter();
  const toast = useToast();

  async function send(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (busy || !body.trim()) return;
    setBusy(true);
    try {
      await browserClient().agent.sendAgentRunMessage({ runId, body: body.trim() });
      setBody("");
      toast.push({ tone: "ok", title: "Message sent" });
      router.refresh();
    } catch (error) {
      toast.error(error, "Could not send the message");
    } finally {
      setBusy(false);
    }
  }

  return (
    <form onSubmit={send} className="flex max-w-xl flex-col gap-3">
      <Field htmlFor={id} label="Message to this agent">
        <Textarea id={id} value={body} onChange={(event) => setBody(event.currentTarget.value)} required disabled={busy} />
      </Field>
      <Button className="self-start" size="sm" variant="primary" type="submit" disabled={busy || !body.trim()}>Send message</Button>
    </form>
  );
}
