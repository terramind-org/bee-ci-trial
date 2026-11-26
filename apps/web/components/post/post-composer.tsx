"use client";

/**
 * The Post panel's composer: text with a live counter, images or a video by drop or picker (each
 * image with its description), the change's own evidence as a source, a live preview through
 * `PostCard`, and Save / Clear. Media uploads only on Save, once each image is described, so
 * nothing is sent that the server would refuse: Begin, a PUT through the same-origin route with
 * progress, Finalize (lib/post-media.ts), then `SetChangePost` with `expected_version`.
 */
import { useEffect, useId, useRef, useState } from "react";
import { browserClient } from "@/lib/api/browser";
import type { ChangeRequest, Post, PostMedia, PostMediaKind, Principal } from "@/lib/api/types";
import {
  MAX_TEXT,
  canAdd,
  counterTone,
  draftProblem,
  evidenceChoices,
  previewPost,
  textLength,
  type EvidenceChoice,
} from "@/lib/post-compose";
import { IMAGE_TYPES, MAX_ALT_CHARS, MAX_IMAGE_BYTES, MAX_VIDEO_BYTES, VIDEO_TYPES, mediaKind, readMediaInfo, uploadPostMedia } from "@/lib/post-media";
import { Icon } from "../icons";
import { PostCard } from "@/components/posts/post-card";
import { useToast } from "../toast";
import { Button, Input, Textarea, cx } from "../ui";
import { refusalOf, type Refusal } from "./refusal";
import { progressFetch } from "./upload";

type Status = "saved" | "queued" | "uploading" | "failed";

interface Item {
  key: string;
  kind: PostMediaKind;
  contentType: string;
  /** What the thumbnail and the preview show: the signed URL once uploaded, an object URL before. */
  url: string;
  width: number;
  height: number;
  alt: string;
  /** The bytes still to upload; absent once the server has them. */
  file?: File;
  /** The finalized media, once uploaded. */
  media?: PostMedia;
  status: Status;
  progress: number;
  error?: Refusal;
  /** The evidence it came from, for its label. */
  from?: string;
}

const ACCEPT = [...IMAGE_TYPES, ...VIDEO_TYPES].join(",");
const COUNTER_TONE = { ok: "text-ink-3", near: "text-warn", over: "text-bad" } as const;

function savedItems(post: Post | null): Item[] {
  return (post?.media ?? []).map((m) => ({ key: m.id, kind: m.kind, contentType: m.contentType, url: m.url, width: m.width, height: m.height, alt: m.altText, media: m, status: "saved" as const, progress: 1 }));
}

let seq = 0;

export function PostComposer({
  repo,
  change,
  saved,
  me,
  onSaved,
}: {
  repo: string;
  change: Pick<ChangeRequest, "id" | "number" | "title">;
  saved: Post | null;
  me?: Principal;
  onSaved: (p: Post | null) => void;
}) {
  const [text, setText] = useState(saved?.text ?? "");
  const [items, setItems] = useState<Item[]>(() => savedItems(saved));
  const [error, setError] = useState<Refusal | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [busy, setBusy] = useState<"save" | "clear" | null>(null);
  const [dragging, setDragging] = useState(false);
  const [picking, setPicking] = useState(false);
  const fileInput = useRef<HTMLInputElement>(null);
  const textId = useId();
  const counterId = useId();
  const toast = useToast();
  const objectUrls = useRef<string[]>([]);

  useEffect(() => {
    const urls = objectUrls.current;
    return () => urls.forEach((u) => URL.revokeObjectURL(u));
  }, []);

  const length = textLength(text);
  const tone = counterTone(length);
  const problem = draftProblem(text, items);
  const dirty = text !== (saved?.text ?? "") || items.length !== (saved?.media.length ?? 0) || items.some((i) => !i.media);

  const patch = (key: string, p: Partial<Item>) => setItems((xs) => xs.map((x) => (x.key === key ? { ...x, ...p } : x)));

  /** Adds files after the same checks the server makes on type, size and the 1–4 images or 1 video rule. */
  async function addFiles(files: File[], from?: { title: string; alt: string }) {
    setNotice(null);
    let current = items;
    for (const file of files) {
      const kind = mediaKind(file.type);
      if (!kind) {
        setNotice((file.name || "That file") + " is not a png, jpeg, gif or webp image, or an mp4 or webm video.");
        continue;
      }
      const max = kind === "POST_MEDIA_KIND_IMAGE" ? MAX_IMAGE_BYTES : MAX_VIDEO_BYTES;
      if (file.size > max) {
        setNotice((file.name || "That file") + " is over the " + (kind === "POST_MEDIA_KIND_IMAGE" ? "8 MiB image" : "64 MiB video") + " limit.");
        continue;
      }
      if (!canAdd(current, kind)) {
        setNotice(kind === "POST_MEDIA_KIND_VIDEO" ? "A video goes alone: remove the other media first." : "A post takes up to 4 images, or one video.");
        continue;
      }
      const url = URL.createObjectURL(file);
      objectUrls.current.push(url);
      const info = await readMediaInfo(file, kind).catch(() => ({ width: 0, height: 0, durationMs: 0 }));
      const item: Item = { key: "new-" + ++seq, kind, contentType: file.type, url, width: info.width, height: info.height, alt: from?.alt ?? "", file, status: "queued", progress: 0, from: from?.title };
      current = [...current, item];
      setItems(current);
    }
  }

  async function takeEvidence(choice: EvidenceChoice) {
    setPicking(false);
    setNotice(null);
    try {
      const res = await fetch(choice.url);
      if (!res.ok) throw new Error("the evidence file answered " + res.status);
      const blob = await res.blob();
      const ext = choice.contentType.split("/")[1] ?? "bin";
      await addFiles([new File([blob], "evidence-" + choice.evidenceId + "." + ext, { type: choice.contentType })], { title: choice.title, alt: choice.alt });
    } catch (err) {
      setNotice("Could not read that evidence: " + refusalOf(err).message);
    }
  }

  async function save() {
    setError(null);
    if (problem) {
      setError({ code: "invalid_argument", reason: "INVALID_ARGUMENT", message: problem });
      return;
    }
    setBusy("save");
    const client = browserClient();
    const ref = { fullName: repo };
    const ids: string[] = [];
    try {
      for (const item of items) {
        if (item.media) {
          ids.push(item.media.id);
          continue;
        }
        if (!item.file) continue;
        patch(item.key, { status: "uploading", progress: 0, error: undefined });
        try {
          const media = await uploadPostMedia(client, { repo: ref, changeId: change.id, file: item.file, altText: item.alt }, { fetch: progressFetch((f) => patch(item.key, { progress: f })) });
          patch(item.key, { media, status: "saved", progress: 1, file: undefined });
          ids.push(media.id);
        } catch (err) {
          const r = refusalOf(err);
          patch(item.key, { status: "failed", error: r });
          throw err;
        }
      }
      const post = await client.post.setChangePost({ repo: ref, changeId: change.id, text: text.normalize("NFC"), mediaIds: ids, expectedVersion: saved?.version ?? "0" });
      setText(post.text);
      setItems(savedItems(post));
      toast.push({ tone: "ok", title: saved ? "Post saved" : "Post drafted", detail: "It publishes when this change merges." });
      onSaved(post);
    } catch (err) {
      setError(refusalOf(err));
    } finally {
      setBusy(null);
    }
  }

  async function clear() {
    setError(null);
    setBusy("clear");
    try {
      await browserClient().post.clearChangePost({ repo: { fullName: repo }, changeId: change.id });
      setText("");
      setItems([]);
      toast.push({ tone: "ok", title: "Post cleared" });
      onSaved(null);
    } catch (err) {
      setError(refusalOf(err));
    } finally {
      setBusy(null);
    }
  }

  const imagesOk = canAdd(items, "POST_MEDIA_KIND_IMAGE");
  const preview = previewPost(
    change,
    repo,
    saved,
    text,
    items.map((i) => ({ id: i.key, kind: i.kind, url: i.url, alt: i.alt, width: i.width, height: i.height, contentType: i.contentType })),
    me,
  );
  const showPreview = !!text.trim() || items.length > 0;

  return (
    <div className="flex flex-col gap-4" data-testid="post-composer">
      <div
        className={cx("rounded-lg transition-colors", dragging && "bg-accent-soft outline-2 outline-offset-4 outline-accent outline-dashed")}
        onDragOver={(e) => {
          if (![...e.dataTransfer.types].includes("Files")) return;
          e.preventDefault();
          setDragging(true);
        }}
        onDragLeave={() => setDragging(false)}
        onDrop={(e) => {
          e.preventDefault();
          setDragging(false);
          void addFiles([...e.dataTransfer.files]);
        }}
      >
        <label htmlFor={textId} className="sr-only">
          Post text
        </label>
        <Textarea
          id={textId}
          rows={4}
          value={text}
          onChange={(e) => setText(e.target.value)}
          placeholder="What changed, for the people who follow this repo. Drop images or a video here."
          aria-describedby={counterId}
          aria-invalid={tone === "over" || undefined}
          className="text-md leading-7"
        />
        <p id={counterId} className={cx("mt-1 text-right text-sm", COUNTER_TONE[tone])} data-testid="post-counter" data-tone={tone}>
          <span className="num">{length}</span>/<span className="num">{MAX_TEXT}</span>
          <span className="sr-only"> characters{tone === "over" ? ", over the limit" : tone === "near" ? ", close to the limit" : ""}</span>
        </p>
      </div>

      {items.length ? (
        <ul className="grid grid-cols-1 gap-4 sm:grid-cols-2" aria-label="Attached media">
          {items.map((item, i) => (
            <Thumb
              key={item.key}
              item={item}
              index={i}
              disabled={busy !== null}
              onAlt={(alt) => patch(item.key, { alt })}
              onRemove={() => {
                setItems((xs) => xs.filter((x) => x.key !== item.key));
                setNotice(null);
              }}
            />
          ))}
        </ul>
      ) : null}

      <div className="flex flex-wrap items-center gap-2">
        <input
          ref={fileInput}
          id={textId + "-files"}
          type="file"
          accept={ACCEPT}
          multiple
          className="sr-only"
          aria-label="Add images or a video"
          tabIndex={-1}
          onChange={(e) => {
            const files = [...(e.target.files ?? [])];
            e.target.value = "";
            void addFiles(files);
          }}
        />
        <Button icon="plus" onClick={() => fileInput.current?.click()} disabled={busy !== null || (!imagesOk && items.length > 0)}>
          Add images or a video
        </Button>
        <Button icon="files" variant="ghost" onClick={() => setPicking((v) => !v)} aria-expanded={picking} disabled={busy !== null}>
          Use evidence
        </Button>
        <span className="flex-1" />
        {saved ? (
          <Button variant="ghost" onClick={clear} disabled={busy !== null} aria-busy={busy === "clear" || undefined}>
            Clear
          </Button>
        ) : null}
        <Button onClick={save} disabled={busy !== null || !dirty} aria-busy={busy === "save" || undefined}>
          {busy === "save" ? "Saving…" : saved ? "Save post" : "Save draft"}
        </Button>
      </div>

      {picking ? <EvidencePicker repo={repo} changeId={change.id} onPick={takeEvidence} onClose={() => setPicking(false)} /> : null}

      {notice ? (
        <p role="status" className="text-sm text-warn">
          {notice}
        </p>
      ) : null}
      {error ? (
        <p role="alert" className="text-sm text-bad" data-testid="post-error">
          <span className="mono">{error.reason}</span> · {error.message}
        </p>
      ) : problem && dirty ? (
        <p className="text-sm text-ink-3">{problem}</p>
      ) : null}

      {showPreview ? (
        <div className="mt-2 max-w-xl" data-testid="post-preview">
          <p className="mb-3 text-sm font-medium text-ink-3">Preview</p>
          <PostCard post={preview} signedIn mediaSource="direct" preview />
        </div>
      ) : null}
    </div>
  );
}

function Thumb({ item, index, disabled, onAlt, onRemove }: { item: Item; index: number; disabled: boolean; onAlt: (alt: string) => void; onRemove: () => void }) {
  const altId = useId();
  const image = item.kind === "POST_MEDIA_KIND_IMAGE";
  const label = (image ? "Image " : "Video ") + (index + 1);
  const missing = image && !item.alt.trim();
  return (
    <li className="flex min-w-0 flex-col gap-2" data-testid="post-attachment" data-status={item.status}>
      <div className="relative aspect-[16/10] overflow-hidden rounded-lg bg-sunken">
        {image ? (
          // A plain <img>: the source is an object URL or a signed URL, which next/image cannot optimise.
          // eslint-disable-next-line @next/next/no-img-element
          <img src={item.url} alt="" className="size-full object-cover" />
        ) : (
          <video src={item.url} preload="metadata" muted playsInline className="size-full object-cover" aria-hidden="true" />
        )}
        <button
          type="button"
          onClick={onRemove}
          disabled={disabled}
          aria-label={"Remove " + label.toLowerCase()}
          className="absolute top-2 right-2 inline-flex size-9 items-center justify-center rounded-full bg-black/60 text-white hover:bg-black/75 focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-white disabled:opacity-50"
        >
          <Icon name="x" size={16} />
        </button>
        {item.status === "uploading" ? (
          <div className="absolute inset-x-0 bottom-0 h-1 bg-black/20" aria-hidden="true">
            <div className="h-full bg-accent transition-[width] duration-150" style={{ width: Math.round(item.progress * 100) + "%" }} />
          </div>
        ) : null}
      </div>
      {item.status === "uploading" ? (
        <progress className="sr-only" max={100} value={Math.round(item.progress * 100)} aria-label={"Uploading " + label.toLowerCase()} />
      ) : null}
      <label htmlFor={altId} className="text-sm font-medium text-ink">
        {label} description{image ? "" : " (optional)"}
        {item.from ? <span className="font-normal text-ink-3"> · from {item.from}</span> : null}
      </label>
      <Input
        id={altId}
        value={item.alt}
        maxLength={MAX_ALT_CHARS}
        required={image}
        aria-invalid={missing || undefined}
        readOnly={!!item.media}
        disabled={disabled && !item.media}
        placeholder={image ? "What the image shows" : "What the video shows"}
        onChange={(e) => onAlt(e.target.value)}
      />
      {item.media ? <p className="text-sm text-ink-3">Uploaded. Remove it and add it again to change the description.</p> : null}
      {item.status === "failed" && item.error ? (
        <p role="alert" className="text-sm text-bad">
          <span className="mono">{item.error.reason}</span> · {item.error.message}
        </p>
      ) : null}
    </li>
  );
}

/** The change's image and video evidence (BriefService.ListEvidence), to re-upload as post media. */
function EvidencePicker({ repo, changeId, onPick, onClose }: { repo: string; changeId: string; onPick: (c: EvidenceChoice) => void; onClose: () => void }) {
  const [state, setState] = useState<{ choices: EvidenceChoice[] } | { error: Refusal } | null>(null);
  useEffect(() => {
    let live = true;
    browserClient()
      .brief.listEvidence({ repo: { fullName: repo }, changeIds: [changeId] })
      .then((res) => live && setState({ choices: evidenceChoices(res.evidence) }))
      .catch((err) => live && setState({ error: refusalOf(err) }));
    return () => {
      live = false;
    };
  }, [repo, changeId]);
  return (
    <div className="rounded-lg border border-line p-3" role="group" aria-label="This change's evidence" data-testid="evidence-picker">
      <div className="mb-2 flex items-center justify-between gap-2">
        <p className="text-base font-medium">Use evidence</p>
        <button type="button" onClick={onClose} aria-label="Close the evidence list" className="inline-flex size-9 items-center justify-center rounded-md text-ink-3 hover:bg-hover hover:text-ink">
          <Icon name="x" size={16} />
        </button>
      </div>
      {state === null ? (
        <p className="text-sm text-ink-3" role="status">
          Loading this change&apos;s evidence…
        </p>
      ) : "error" in state ? (
        <p className="text-sm text-ink-2" role="status">
          {state.error.code === "unimplemented" ? "This server does not list evidence yet." : "Could not list evidence: " + state.error.message}
        </p>
      ) : state.choices.length ? (
        <ul className="grid grid-cols-2 gap-3 sm:grid-cols-3">
          {state.choices.map((c) => (
            <li key={c.key}>
              <button type="button" onClick={() => onPick(c)} className="group flex w-full flex-col gap-1.5 rounded-md text-left focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent">
                <span className="block aspect-[16/10] w-full overflow-hidden rounded-md bg-sunken">
                  {c.kind === "POST_MEDIA_KIND_IMAGE" ? (
                    // eslint-disable-next-line @next/next/no-img-element
                    <img src={c.url} alt="" className="size-full object-cover group-hover:opacity-90" />
                  ) : (
                    <video src={c.url} preload="metadata" muted playsInline className="size-full object-cover" aria-hidden="true" />
                  )}
                </span>
                <span className="text-sm text-ink">
                  Use {c.title}
                  {c.stale ? <span className="text-ink-3"> · from an earlier revision</span> : null}
                </span>
              </button>
            </li>
          ))}
        </ul>
      ) : (
        <p className="text-sm text-ink-2" role="status">
          This change has no image or video evidence.
        </p>
      )}
    </div>
  );
}
