"use client";

import { useRouter } from "next/navigation";
import { useId, useRef, useState, type FormEvent } from "react";
import { Button, Field, Input, Select, Textarea } from "@/components/kit";
import { browserClient } from "@/lib/api/browser";
import type { PostAttachment, PostMediaKind } from "@/lib/api/types";
import { canAdd, MAX_TEXT, textLength } from "@/lib/post-compose";
import { checkUpload, IMAGE_TYPES, MAX_ALT_CHARS, mediaKind, uploadPostMedia, VIDEO_TYPES } from "@/lib/post-media";
import { refusalOf } from "../post/refusal";
import { progressFetch } from "../post/upload";
import { attachmentLabel } from "./attachments";
import { postHref } from "./post-text";

const BODY_LIMIT = 20_000;
const TITLE_LIMIT = 200;
const ARTICLE_LIMIT = 100_000;
const ATTACHMENT_LIMIT = 32;
const ACCEPT = [...IMAGE_TYPES, ...VIDEO_TYPES].join(",");
const KIND_LABELS = {
  POST_ATTACHMENT_KIND_CHANGE_REQUEST: "Pull requests",
  POST_ATTACHMENT_KIND_RELEASE: "Releases",
  POST_ATTACHMENT_KIND_ISSUE: "Issues",
  POST_ATTACHMENT_KIND_AGENT_RUN: "Agent runs",
} as const;

type Media = { key: number; file: File; kind: PostMediaKind; alt: string };

export function ComposePost({ fullName, choices }: { fullName: string; choices: PostAttachment[] }) {
  const router = useRouter();
  const id = useId();
  const [text, setText] = useState("");
  const [selected, setSelected] = useState<PostAttachment[]>([]);
  const [choice, setChoice] = useState("");
  const [media, setMedia] = useState<Media[]>([]);
  const [busy, setBusy] = useState("");
  const [error, setError] = useState("");
  const filePicker = useRef<HTMLInputElement>(null);
  const sequence = useRef(0);
  const inFlight = useRef(false);
  // A refused CreatePost can be retried using its finalized media while descriptions stay the same.
  const uploads = useRef(new Map<number, { alt: string; id: string }>());

  const eligible = (candidate: PostAttachment) => !selected.some((a) =>
    a.kind === candidate.kind && (a.id === candidate.id || a.kind !== "POST_ATTACHMENT_KIND_CHANGE_REQUEST"),
  );

  function addFiles(files: File[]) {
    const next = [...media];
    try {
      for (const file of files) {
        const kind = mediaKind(file.type);
        // During selection, check format and size. The real description is required on submit.
        checkUpload(file, file.name);
        if (!kind || !canAdd(next, kind)) {
          setError("INVALID_ARGUMENT · Choose up to four images or one video.");
          return;
        }
        next.push({ key: ++sequence.current, file, kind, alt: "" });
      }
      setMedia(next);
      setError("");
    } catch (err) {
      const reason = refusalOf(err);
      setError(reason.reason + " · " + reason.message);
    }
  }

  async function publish(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (inFlight.current) return;
    const form = new FormData(event.currentTarget);
    const summary = text.normalize("NFC").trim();
    const bodyMarkdown = String(form.get("body")).trim();
    const title = String(form.get("articleTitle")).normalize("NFC").trim();
    const body = String(form.get("articleBody")).trim();
    if (!summary && !bodyMarkdown && !body && !media.length) {
      setError("INVALID_ARGUMENT · Add text, Markdown, an article, or media.");
      return;
    }
    if (!!title !== !!body) {
      setError("INVALID_ARGUMENT · An article needs both a title and a body.");
      return;
    }
    if (textLength(summary) > MAX_TEXT || textLength(bodyMarkdown) > BODY_LIMIT || textLength(title) > TITLE_LIMIT || textLength(body) > ARTICLE_LIMIT) {
      setError("INVALID_ARGUMENT · Shorten the text to fit the field limits.");
      return;
    }

    inFlight.current = true;
    setBusy("Publishing…");
    setError("");
    const repo = { fullName };
    const api = browserClient();
    try {
      for (const item of media) checkUpload(item.file, item.alt);
      const mediaIds: string[] = [];
      for (const [index, item] of media.entries()) {
        const cached = uploads.current.get(item.key);
        if (cached?.alt === item.alt) {
          mediaIds.push(cached.id);
          continue;
        }
        const progress = "Uploading " + (index + 1) + " of " + media.length;
        setBusy(progress + "…");
        const uploaded = await uploadPostMedia(api, { repo, file: item.file, altText: item.alt }, {
          fetch: progressFetch((fraction) => setBusy(progress + " · " + Math.round(fraction * 100) + "%")),
        });
        uploads.current.set(item.key, { id: uploaded.id, alt: item.alt });
        mediaIds.push(uploaded.id);
      }
      setBusy("Publishing…");
      const post = await api.post.createPost({
        repo, text: summary, bodyMarkdown, article: title && body ? { title, bodyMarkdown: body } : undefined,
        attachments: selected.map(({ kind, id, number }) => ({ kind, id, number })), mediaIds,
      });
      router.push(postHref(post));
      return;
    } catch (err) {
      const reason = refusalOf(err);
      // The media may have expired while the form was open. A retry can upload it again.
      if (/media.*(not_found|expired|bad_media)|bad_media/i.test(reason.reason + " " + reason.message)) uploads.current.clear();
      setError(reason.reason + " · " + reason.message);
    }
    inFlight.current = false;
    setBusy("");
  }

  return (
    <form onSubmit={publish} className="space-y-8">
      <fieldset disabled={!!busy} className="min-w-0 space-y-5">
        <Field label="Post text" htmlFor={id + "-text"} hint={textLength(text) + " / " + MAX_TEXT + " characters"}>
          <Textarea id={id + "-text"} value={text} onChange={(e) => setText(e.target.value)} aria-invalid={textLength(text) > MAX_TEXT || undefined} />
        </Field>
        <Field label="Body Markdown" htmlFor={id + "-body"} hint="Optional. Up to 20,000 characters.">
          <Textarea id={id + "-body"} name="body" rows={5} maxLength={BODY_LIMIT * 2} />
        </Field>
        <section aria-labelledby={id + "-article"} className="space-y-4 border-t border-line pt-5">
          <h2 id={id + "-article"} className="text-lg">Article <span className="text-sm text-ink-2">Optional</span></h2>
          <Field label="Article title" htmlFor={id + "-article-title"}>
            <Input id={id + "-article-title"} name="articleTitle" maxLength={TITLE_LIMIT * 2} />
          </Field>
          <Field label="Article Markdown" htmlFor={id + "-article-body"}>
            <Textarea id={id + "-article-body"} name="articleBody" rows={6} maxLength={ARTICLE_LIMIT * 2} />
          </Field>
        </section>
        <section aria-labelledby={id + "-attachments"} className="space-y-3 border-t border-line pt-5">
          <h2 id={id + "-attachments"} className="text-lg">Attachments</h2>
          {selected.length ? <ul className="divide-y divide-line">
            {selected.map((a) => <li key={a.kind + ":" + a.id} className="flex items-center justify-between gap-3 py-2">
              <span>{attachmentLabel(a)}</span>
              <Button size="sm" variant="ghost" aria-label={"Remove " + attachmentLabel(a)} onClick={() => setSelected(selected.filter((value) => value !== a))}>Remove</Button>
            </li>)}
          </ul> : null}
          {choices.length ? (
            <div className="flex flex-wrap items-end gap-2">
              <div className="min-w-0 flex-1">
                <Field label="Attach from this repository" htmlFor={id + "-choice"}>
                  <Select id={id + "-choice"} value={choice} onChange={(e) => setChoice(e.target.value)}>
                    <option value="">Choose an attachment</option>
                    {Object.entries(KIND_LABELS).map(([kind, label]) => {
                      const options = choices.flatMap((a, index) => a.kind === kind && eligible(a) ? [<option key={index} value={index}>{attachmentLabel(a)}</option>] : []);
                      return options.length ? <optgroup key={kind} label={label}>{options}</optgroup> : null;
                    })}
                  </Select>
                </Field>
              </div>
              <Button disabled={!choice || selected.length >= ATTACHMENT_LIMIT} onClick={() => {
                const candidate = choices[Number(choice)];
                if (choice && candidate && eligible(candidate)) setSelected([...selected, candidate]);
                setChoice("");
              }}>Attach</Button>
            </div>
          ) : <p className="text-ink-2">There are no items to attach in this repository.</p>}
        </section>
        <section aria-labelledby={id + "-media"} className="space-y-3 border-t border-line pt-5">
          <h2 id={id + "-media"} className="text-lg">Media</h2>
          <p className="text-sm text-ink-2">Up to four images (8 MiB each) or one video (64 MiB).</p>
          <Input type="file" ref={filePicker} accept={ACCEPT} multiple className="sr-only" tabIndex={-1} aria-hidden="true" onChange={(e) => {
            addFiles(Array.from(e.target.files ?? []));
            e.target.value = "";
          }} />
          <Button onClick={() => filePicker.current?.click()}>Add images or a video</Button>
          {media.length ? <ul className="space-y-4">
            {media.map((item, index) => <li className="min-w-0" key={item.key}>
              <div className="flex items-center justify-between gap-3">
                <span className="truncate text-sm">{item.file.name}</span>
                <Button size="sm" variant="ghost" onClick={() => {
                  uploads.current.delete(item.key);
                  setMedia(media.filter((value) => value !== item));
                }} aria-label={"Remove " + item.file.name}>Remove</Button>
              </div>
              <Field htmlFor={id + "-alt-" + item.key} label={(item.kind === "POST_MEDIA_KIND_IMAGE" ? "Image " : "Video ") + (index + 1) + " description"}>
                <Input id={id + "-alt-" + item.key} required={item.kind === "POST_MEDIA_KIND_IMAGE"}
                  value={item.alt} maxLength={MAX_ALT_CHARS} onChange={(e) => setMedia(media.map((value) => value === item ? { ...value, alt: e.target.value } : value))} />
              </Field>
            </li>)}
          </ul> : null}
        </section>
      </fieldset>
      {error ? <p className="text-sm text-bad" role="alert">{error}</p> : null}
      <Button type="submit" variant="primary" disabled={!!busy || textLength(text) > MAX_TEXT}>Publish post</Button>
      {busy ? <p role="status" className="text-sm text-ink-2">{busy}</p> : null}
    </form>
  );
}
