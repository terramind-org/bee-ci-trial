"use client";

/**
 * A change request's Post: the tweet-like note that publishes to followers when the change
 * merges (design: Web, "Change request page"). It mounts in the change's Brief tab under
 * "Who did it"; `post-panel-slot.tsx` builds its viewer on the server.
 *
 *   Post                                                             …
 *   ● Publishes when this change merges
 *   [ What changed, for the people who follow this repo           480/500 ]
 *   [ thumbnails, each with its description, progress and remove        ]
 *   + Add images or a video   ▤ Use evidence            Clear   Save post
 *   Preview
 *   (the post as the feed will show it)
 *
 * Who sees what (`panelMode` in lib/post-compose.ts): writers get the composer; a person who may
 * only approve or withdraw gets the post with those actions; any other reader sees a published
 * post as a card, and nothing at all before that.
 */
import Link from "next/link";
import { useRouter } from "next/navigation";
import { useState, useTransition } from "react";
import { browserClient } from "@/lib/api/browser";
import type { ChangeRequest, Post, Principal } from "@/lib/api/types";
import type { Op } from "@/lib/copyas";
import { panelMode, stateLine, withRepo, type PanelViewer, type StateTone } from "@/lib/post-compose";
import { CopyAsDialog } from "../copy-as";
import { Icon } from "../icons";
import { ConfirmButton } from "../kit/confirm";
import { Modal } from "../kit/dialog";
import { Menu, MenuButton, MenuLink } from "../menu";
import { PostCard } from "@/components/posts/post-card";
import { postHref } from "@/components/posts/post-text";
import { useToast } from "../toast";
import { Button, Textarea, buttonClass, cx } from "../ui";
import { PostComposer } from "./post-composer";
import { refusalOf, type Refusal } from "./refusal";

export interface PostPanelProps {
  /** `owner/name` of the change's repo. */
  repo: string;
  change: Pick<ChangeRequest, "id" | "number" | "title" | "state" | "post">;
  viewer: PanelViewer;
  /** The signed-in principal, who becomes the post's author on save (the live preview's byline). */
  me?: Principal | null;
}

const TONE: Record<StateTone, string> = { neutral: "text-ink-3", warn: "text-warn", ok: "text-ok", bad: "text-bad" };

export function PostPanel({ repo, change, viewer: initialViewer, me }: PostPanelProps) {
  const [post, setPost] = useState<Post | null>(change.post ? withRepo(change.post, repo) : null);
  const [viewer, setViewer] = useState(initialViewer);
  const router = useRouter();

  /** Takes the post a write answered with; its `viewer` is the server's word on what is next. */
  const update = (next: Post | null) => {
    setPost(next && withRepo(next, repo));
    const v = next?.viewer;
    setViewer((was) => (next && v ? { ...was, canEdit: v.canEdit, canApprove: v.canApprove, canWithdraw: v.canWithdraw } : { ...was, canApprove: false, canWithdraw: false }));
    router.refresh();
  };

  const mode = panelMode(post, viewer);
  if (mode === "hidden") return null;
  if (mode === "card" && post) {
    return (
      <section aria-labelledby="post-panel-title" data-testid="post-panel" data-mode="card">
        <h2 id="post-panel-title" className="mb-4 text-lg font-semibold">
          Post
        </h2>
        <PostCard post={post} signedIn={viewer.signedIn} />
      </section>
    );
  }

  const permalink = post ? postHref(post) : "";
  const line = stateLine(post, permalink);
  return (
    <section aria-labelledby="post-panel-title" data-testid="post-panel" data-mode={mode} data-state={post?.state ?? "none"}>
      <header className="mb-1 flex items-center justify-between gap-3">
        <h2 id="post-panel-title" className="text-lg font-semibold">
          Post
        </h2>
        <PanelMenu repo={repo} changeId={change.id} post={post} />
      </header>
      <p className={cx("mb-4 flex items-center gap-2 text-base", TONE[line.tone])} data-testid="post-state" role="status">
        <Icon name={line.tone === "ok" ? "check" : line.tone === "warn" ? "clock" : line.tone === "bad" ? "x" : "dot"} size={16} />
        {line.href ? (
          <Link href={line.href} className="font-medium underline decoration-line-strong underline-offset-[3px] hover:decoration-current">
            {line.text}
          </Link>
        ) : (
          <span>{line.text}</span>
        )}
        {line.approvedBy ? <span className="text-ink-3">· approved by {line.approvedBy}</span> : null}
      </p>

      {mode === "compose" ? (
        <PostComposer repo={repo} change={change} saved={post} me={me ?? undefined} onSaved={update} />
      ) : post ? (
        <PostCard post={post} signedIn={viewer.signedIn} preview mediaSource={post.state === "POST_STATE_PUBLISHED" ? "route" : "direct"} />
      ) : null}

      {post && (viewer.canApprove || viewer.canWithdraw) ? (
        <div className="mt-4 flex flex-wrap items-center gap-2">
          {viewer.canApprove ? <ApproveButton repo={repo} post={post} onDone={update} /> : null}
          {viewer.canWithdraw ? <WithdrawButton repo={repo} post={post} onDone={update} /> : null}
        </div>
      ) : null}
    </section>
  );
}

function ApproveButton({ repo, post, onDone }: { repo: string; post: Post; onDone: (p: Post) => void }) {
  const [pending, start] = useTransition();
  const toast = useToast();
  const landed = post.state === "POST_STATE_PENDING_APPROVAL";
  return (
    <ConfirmButton
      agentConfirm={landed ? "Approves this post and publishes it to everyone who follows " + repo : "Approves this post; it publishes to followers of " + repo + " when the pull request merges"}
      consequence="approving"
      // While the pull request is open its own decision is the screen's solid button; once it
      // has merged, approving the post is what is left to do.
      variant={landed ? "primary" : "secondary"}
      icon="check"
      disabled={pending}
      aria-busy={pending || undefined}
      onClick={() =>
        start(async () => {
          try {
            const next = await browserClient().post.approvePost({ repo: { fullName: repo }, postId: post.id });
            toast.push({ tone: "ok", title: landed ? "Approved and published" : "Approved", detail: landed ? undefined : "It publishes when this pull request merges." });
            onDone(next);
          } catch (err) {
            const r = refusalOf(err);
            toast.push({ tone: "bad", title: "Approve failed · " + r.reason, detail: r.message });
          }
        })
      }
    >
      Approve post
    </ConfirmButton>
  );
}

const WITHDRAW_SENTENCE = "Withdraws this post from every feed and deletes its media; this cannot be undone";

function WithdrawButton({ repo, post, onDone }: { repo: string; post: Post; onDone: (p: Post) => void }) {
  const [open, setOpen] = useState(false);
  const [reason, setReason] = useState("");
  const [error, setError] = useState<Refusal | null>(null);
  const [pending, start] = useTransition();
  const toast = useToast();
  const withdraw = () =>
    start(async () => {
      setError(null);
      try {
        const next = await browserClient().post.withdrawPost({ repo: { fullName: repo }, postId: post.id, reason: reason.trim() || undefined });
        setOpen(false);
        toast.push({ tone: "ok", title: "Post withdrawn", detail: "It is gone from every feed; its link now says it was withdrawn." });
        onDone(next);
      } catch (err) {
        setError(refusalOf(err));
      }
    });
  return (
    <>
      <ConfirmButton agentConfirm={WITHDRAW_SENTENCE} consequence="destructive" onClick={() => setOpen(true)}>
        Withdraw post
      </ConfirmButton>
      <Modal open={open} onOpenChange={setOpen} title="Withdraw this post?" description="It leaves every feed and its media is deleted. The link stays, and says the post was withdrawn. This cannot be undone." hideClose className="top-[16vh] w-[min(520px,calc(100vw-24px))]">
        <label htmlFor="withdraw-reason" className="mt-4 block text-base font-medium">
          Reason <span className="font-normal text-ink-3">(optional, shown on the link)</span>
        </label>
        <Textarea id="withdraw-reason" className="mt-1.5" rows={2} maxLength={500} value={reason} onChange={(e) => setReason(e.target.value)} />
        {error ? (
          <p role="alert" className="mt-3 text-sm text-bad">
            <span className="mono">{error.reason}</span> · {error.message}
          </p>
        ) : null}
        <div className="mt-5 flex justify-end gap-2.5">
          <Button variant="ghost" onClick={() => setOpen(false)}>
            Keep it
          </Button>
          <ConfirmButton agentConfirm={WITHDRAW_SENTENCE} consequence="destructive" onClick={withdraw} disabled={pending} aria-busy={pending || undefined}>
            Withdraw
          </ConfirmButton>
        </div>
      </Modal>
    </>
  );
}

/** The panel's "…" menu: the permalink once there is one, and *Copy as* for agents. */
function PanelMenu({ repo, changeId, post }: { repo: string; changeId: string; post: Post | null }) {
  const [op, setOp] = useState<Op | null>(null);
  const ref = { fullName: repo };
  const read: Op = { service: "PostService", method: "GetChangePost", request: { repo: ref, changeId }, label: "Read this change's post" };
  const write: Op = {
    service: "PostService",
    method: "SetChangePost",
    request: { repo: ref, changeId, text: post?.text ?? "", mediaIds: post?.media.map((m) => m.id) ?? [], expectedVersion: post?.version ?? "0" },
    label: "Write this change's post",
  };
  return (
    <>
      <Menu label="More actions for the post" labelled triggerClass={buttonClass("ghost", "sm", "size-9 px-0 text-ink-3")} trigger={<Icon name="more" size={20} weight="bold" />}>
        {post?.state === "POST_STATE_PUBLISHED" ? (
          <MenuLink href={postHref(post)} icon="link">
            Open post
          </MenuLink>
        ) : null}
        <MenuButton icon="terminal" onSelect={() => setOp(read)}>
          Copy as: read the post
        </MenuButton>
        <MenuButton icon="terminal" onSelect={() => setOp(write)}>
          Copy as: write the post
        </MenuButton>
      </Menu>
      {op ? <CopyAsDialog op={op} open={!!op} onOpenChange={(v) => (v ? null : setOp(null))} /> : null}
    </>
  );
}
