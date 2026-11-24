"use client";

import { useRouter } from "next/navigation";
import { useState } from "react";
import { browserClient } from "@/lib/api/browser";
import { CopyAs } from "./copy-as";
import { Markdown } from "./markdown";
import { useToast } from "./toast";
import { Button, Field, Input, Textarea } from "./ui";

export function NewRelease({ repo, suggestedTag }: { repo: string; suggestedTag: string }) {
  const router = useRouter();
  const toast = useToast();
  const [tag, setTag] = useState(suggestedTag);
  const [notes, setNotes] = useState("");
  const [pre, setPre] = useState(false);
  const [busy, setBusy] = useState(false);
  const [preview, setPreview] = useState(false);
  const r = { fullName: repo };
  const req = { repo: r, tag, name: tag, notes, prerelease: pre };
  return (
    <form
      className="flex flex-col gap-3"
      aria-label="Draft a release"
      onSubmit={async (e) => {
        e.preventDefault();
        setBusy(true);
        try {
          await browserClient().release.createRelease(req);
          toast.push({ tone: "ok", title: "Published " + tag });
          setNotes("");
          router.refresh();
        } catch (err) {
          toast.error(err, "Not published");
        } finally {
          setBusy(false);
        }
      }}
    >
      <div className="grid gap-3 sm:grid-cols-[200px_1fr] [&>*]:min-w-0">
        <Field label="Tag" htmlFor="rel-tag" hint="Can't be changed once published.">
          <Input id="rel-tag" value={tag} onChange={(e) => setTag(e.target.value)} className="mono" />
        </Field>
        <div className="flex flex-col gap-1.5">
          <div className="flex flex-wrap items-center justify-between gap-1">
            <label htmlFor="rel-notes" className="text-sm font-medium">
              Notes
            </label>
            <div className="flex flex-wrap items-center gap-1">
              <Button
                size="sm"
                variant="ghost"
                icon="refresh"
                onClick={async () => {
                  try {
                    const res = await browserClient().release.generateNotes({ repo: r, tag });
                    setNotes(res.notes);
                  } catch (err) {
                    toast.error(err);
                  }
                }}
              >
                Generate from landed changes
              </Button>
              <CopyAs compact op={{ service: "ReleaseService", method: "GenerateNotes", request: { repo: r, tag }, label: "Generate release notes" }} />
              <Button size="sm" variant="ghost" onClick={() => setPreview((p) => !p)} aria-pressed={preview}>
                {preview ? "Edit" : "Preview"}
              </Button>
            </div>
          </div>
          {preview ? <div className="min-h-32 rounded-md border border-line p-3"><Markdown source={notes} /></div> : <Textarea id="rel-notes" value={notes} onChange={(e) => setNotes(e.target.value)} rows={7} className="mono text-sm" />}
        </div>
      </div>
      <label className="flex items-center gap-1.5 text-sm">
        <input type="checkbox" checked={pre} onChange={(e) => setPre(e.target.checked)} className="accent-[var(--accent)]" />
        Pre-release (mutable until promoted)
      </label>
      <div className="flex items-center gap-1.5">
        <Button type="submit" variant="primary" icon="releases" disabled={busy || !tag}>
          Publish {tag}
        </Button>
        <CopyAs op={{ service: "ReleaseService", method: "CreateRelease", request: req, label: "Publish this release" }} />
      </div>
    </form>
  );
}
