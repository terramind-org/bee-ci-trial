"use client";

/**
 * A conversation comment on the change request (ChangeService.CreateChangeComment without an
 * anchor). Signed-out readers get a sign-in link instead; agents read these like any comment.
 */
import Link from "next/link";
import { useRouter } from "next/navigation";
import { useState } from "react";
import { useToast } from "@/components/toast";
import { Button, Field, Textarea } from "@/components/ui";
import { browserClient } from "@/lib/api/browser";

export function CommentForm({ repo, crId, signedIn, next }: { repo: string; crId: string; signedIn: boolean; next: string }) {
  const router = useRouter();
  const toast = useToast();
  const [body, setBody] = useState("");
  const [busy, setBusy] = useState(false);
  if (!signedIn)
    return (
      <p className="text-base text-ink-2">
        <Link href={"/signin?next=" + encodeURIComponent(next)} className="text-ink underline underline-offset-4 hover:decoration-ink">
          Sign in
        </Link>{" "}
        to join the conversation.
      </p>
    );
  async function post(e: React.FormEvent) {
    e.preventDefault();
    if (!body.trim()) return;
    setBusy(true);
    try {
      await browserClient().change.commentChange({ repo: { fullName: repo }, changeRequestId: crId, body });
      toast.push({ tone: "ok", title: "Comment posted" });
      setBody("");
      router.refresh();
    } catch (err) {
      toast.error(err, "Comment not posted");
    } finally {
      setBusy(false);
    }
  }
  return (
    <form onSubmit={post} className="flex flex-col gap-3" data-testid="comment-form">
      <Field label="Comment" htmlFor="cr-comment">
        <Textarea id="cr-comment" value={body} onChange={(e) => setBody(e.target.value)} rows={3} placeholder="Ask a question or leave a note for the author." />
      </Field>
      <div>
        <Button type="submit" variant="primary" size="sm" disabled={busy || !body.trim()}>
          {busy ? "Posting…" : "Comment"}
        </Button>
      </div>
    </form>
  );
}
