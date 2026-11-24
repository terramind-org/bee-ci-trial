"use client";

import { useRouter } from "next/navigation";
import { useEffect, useRef, useState } from "react";
import { Icon } from "@/components/icons";
import { Input, Kbd, Select } from "@/components/ui";
import { listHref, type ListParams, type ListTarget } from "./query";

/**
 * The filter bar of /repositories. Every control writes the URL (the page is server-rendered
 * from it), so a filtered list can be shared and the back button undoes a filter. Text waits for
 * a short pause; selects apply at once. `/` focuses the text box (the shell's shortcut finds
 * `data-filter`).
 */
export function RepoFilters({ params, signedIn, languages, target }: { params: ListParams; signedIn: boolean; languages: string[]; target?: ListTarget }) {
  const router = useRouter();
  const [q, setQ] = useState(params.q);
  const [topic, setTopic] = useState(params.topic);
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const go = (patch: Partial<ListParams>) => router.replace(listHref({ ...patch, cursor: "", page: 1 }, params, signedIn, target), { scroll: false });

  // Back/forward changes the URL under us: follow it (adjusting state while rendering, not in an effect).
  const [seen, setSeen] = useState(params.q + "\0" + params.topic);
  if (seen !== params.q + "\0" + params.topic) {
    setSeen(params.q + "\0" + params.topic);
    setQ(params.q);
    setTopic(params.topic);
  }
  useEffect(() => () => void (timer.current && clearTimeout(timer.current)), []);

  const later = (patch: Partial<ListParams>) => {
    if (timer.current) clearTimeout(timer.current);
    timer.current = setTimeout(() => go(patch), 250);
  };
  const langs = params.language && !languages.includes(params.language) ? [...languages, params.language].sort() : languages;
  const isPublicTab = !target && params.tab === "public";
  // Starred repos carry neither a visibility nor topics, so those two filters cannot apply there.
  const starred = !target && params.tab === "starred";

  return (
    <form
      role="search"
      aria-label="Filter repositories"
      className="flex flex-wrap items-center gap-2"
      onSubmit={(e) => {
        e.preventDefault();
        if (timer.current) clearTimeout(timer.current);
        go({ q, topic });
      }}
    >
      <div className="relative min-w-[200px] flex-1 basis-64">
        <Icon name="search" className="pointer-events-none absolute left-2.5 top-1/2 -translate-y-1/2 text-ink-3" />
        <Input
          data-filter
          type="search"
          name="q"
          aria-label="Find a repository"
          placeholder="Find a repository"
          autoComplete="off"
          className="pl-8 pr-9"
          value={q}
          onChange={(e) => {
            setQ(e.target.value);
            later({ q: e.target.value.trim().replace(/\s+/g, " ") });
          }}
        />
        <span className="pointer-events-none absolute right-2 top-1/2 -translate-y-1/2" aria-hidden="true">
          <Kbd>/</Kbd>
        </span>
      </div>
      <Select
        aria-label="Visibility"
        name="visibility"
        value={isPublicTab ? "public" : params.visibility}
        disabled={isPublicTab || starred}
        title={isPublicTab ? "The Public tab only lists public repositories" : starred ? "Starred repositories cannot be filtered by visibility" : undefined}
        className="w-auto!"
        onChange={(e) => go({ visibility: e.target.value as ListParams["visibility"] })}
      >
        {isPublicTab ? null : <option value="">Any visibility</option>}
        <option value="public">Public</option>
        {isPublicTab ? null : (
          <>
            <option value="internal">Internal</option>
            <option value="private">Private</option>
          </>
        )}
      </Select>
      <Select aria-label="Language" name="language" value={params.language} className="w-auto!" onChange={(e) => go({ language: e.target.value })}>
        <option value="">Any language</option>
        {langs.map((l) => (
          <option key={l} value={l}>
            {l}
          </option>
        ))}
      </Select>
      <Input
        name="topic"
        aria-label="Topic"
        placeholder="Topic"
        disabled={starred}
        title={starred ? "Starred repositories cannot be filtered by topic" : undefined}
        autoComplete="off"
        className="w-32!"
        value={topic}
        onChange={(e) => {
          setTopic(e.target.value);
          later({ topic: e.target.value.trim().toLowerCase().replace(/[\s:"]+/g, "") });
        }}
      />
      <Select aria-label="Sort" name="sort" value={params.sort} className="w-auto!" onChange={(e) => go({ sort: e.target.value as ListParams["sort"] })}>
        <option value="updated">Recently updated</option>
        <option value="stars">Most stars</option>
        <option value="name">Name</option>
      </Select>
    </form>
  );
}
