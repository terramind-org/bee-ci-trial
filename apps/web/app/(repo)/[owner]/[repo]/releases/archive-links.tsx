"use client";

import { useState, useTransition } from "react";
import { CopyAs } from "@/components/copy-as";
import { Icon } from "@/components/icons";
import { Button } from "@/components/ui";
import type { SourceArchiveFormat } from "@/lib/api/types";
import { archiveRefusalText, type ArchiveOutcome } from "@/lib/infra";
import { requestSourceArchive } from "./archive-actions";

const FORMATS: { format: SourceArchiveFormat; label: string }[] = [
  { format: "SOURCE_ARCHIVE_FORMAT_TAR_GZ", label: "tar.gz" },
  { format: "SOURCE_ARCHIVE_FORMAT_ZIP", label: "zip" },
];

/** What replaces the link when the server refused the archive: its reason, then its words. */
export function ArchiveRefusal({ reason, message }: { reason: string; message: string }) {
  return (
    <p role="alert" data-testid="archive-refusal" className="flex items-start gap-1.5 text-sm text-bad">
      <Icon name="x" size={14} className="mt-0.5 shrink-0" />
      <span>
        {archiveRefusalText(reason, message)} <span className="mono text-xs text-ink-3">{reason}</span>
      </span>
    </p>
  );
}

/** Source archives of a release's revision. Each click asks the server; a refusal prints inline. */
export function ArchiveLinks({ repo, at, tag }: { repo: string; at: string; tag: string }) {
  const [pending, start] = useTransition();
  const [outcome, setOutcome] = useState<ArchiveOutcome | null>(null);
  const ask = (format: SourceArchiveFormat) =>
    start(async () => {
      const res = await requestSourceArchive(repo, at, format);
      setOutcome(res);
      if (res.ok) window.location.assign(res.url);
    });
  return (
    <div className="flex flex-col gap-1.5" data-testid="archive-links">
      <div className="flex flex-wrap items-center gap-1.5">
        <span className="text-sm text-ink-2">Source code</span>
        {FORMATS.map((f) => (
          <Button key={f.format} size="sm" variant="secondary" icon="file" disabled={pending} onClick={() => ask(f.format)} title={"Download " + tag + " as " + f.label}>
            {f.label}
          </Button>
        ))}
        <CopyAs compact op={{ service: "ReleaseService", method: "GetSourceArchiveUrl", request: { repo: { fullName: repo }, at: { expr: at }, format: "SOURCE_ARCHIVE_FORMAT_TAR_GZ" }, label: "Get a signed URL for the source archive" }} />
      </div>
      {outcome && !outcome.ok ? <ArchiveRefusal reason={outcome.reason} message={outcome.message} /> : null}
      {outcome?.ok && outcome.filtered ? <p className="text-xs text-ink-3">Paths you cannot read were left out of this archive.</p> : null}
    </div>
  );
}
