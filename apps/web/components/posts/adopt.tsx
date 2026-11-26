"use client";

import { useId, useState, type FormEvent } from "react";
import { Button, ButtonLink, Field, Input, Modal, Textarea } from "@/components/kit";
import { browserClient } from "@/lib/api/browser";
import type { Post } from "@/lib/api/types";
import { isRepoFullName, repoHref } from "@/lib/routes";
import { refusalOf } from "../post/refusal";
import { postHref } from "./post-text";

export function Adopt({ post, signedIn, primary }: { post: Post; signedIn: boolean; primary: boolean }) {
  const id = useId();
  const [open, setOpen] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const variant = primary ? "primary" : "secondary";
  const label = "Adopt in my repository";
  if (!signedIn) return <ButtonLink size="sm" variant={variant} href={"/signin?next=" + encodeURIComponent(postHref(post))}>{label}</ButtonLink>;

  async function adopt(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    const data = new FormData(event.currentTarget);
    const fullName = String(data.get("repo")).trim();
    const title = String(data.get("title")).trim();
    if (!isRepoFullName(fullName) || !title) {
      setError("INVALID_ARGUMENT · Enter a repository in owner/name form and an issue title.");
      return;
    }
    setError("");
    setBusy(true);
    try {
      const issue = await browserClient().issue.createIssue({
        repo: { fullName }, title, body: String(data.get("body")).trim(), inspiredByPostId: post.id,
      });
      setOpen(false);
      window.location.assign(repoHref(fullName, "work/" + issue.number));
    } catch (err) {
      const reason = refusalOf(err);
      setError(reason.reason + " · " + reason.message);
    } finally {
      setBusy(false);
    }
  }

  return (
    <>
      <Button size="sm" variant={variant} onClick={() => { setError(""); setOpen(true); }}>{label}</Button>
      <Modal open={open} onOpenChange={(next) => { if (!busy) setOpen(next); }} title={label} description="Open an issue in your repository that links back to this post.">
        <form onSubmit={adopt} className="flex flex-col gap-4">
          <Field label="Repository" htmlFor={id + "-repo"} hint="owner/name">
            <Input id={id + "-repo"} name="repo" required disabled={busy} />
          </Field>
          <Field label="Issue title" htmlFor={id + "-title"}>
            <Input id={id + "-title"} name="title" required disabled={busy} />
          </Field>
          <Field label="What would you adopt?" htmlFor={id + "-body"}>
            <Textarea id={id + "-body"} name="body" disabled={busy} />
          </Field>
          {error ? <p role="alert" className="text-sm text-bad">{error}</p> : null}
          <Button type="submit" className="self-start" disabled={busy}>{busy ? "Opening issue…" : "Open issue"}</Button>
        </form>
      </Modal>
    </>
  );
}
