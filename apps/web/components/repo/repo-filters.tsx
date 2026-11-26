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
  const go = (patch: Partial<ListParams>) => {
    if (timer.current) clearTimeout(timer.current);
    router.replace(listHref({
      q: q.trim().replace(/\s+/g, " "),
      topic: topic.trim().toLowerCase().replace(/[\s:"]+/g, ""),
      ...patch, cursor: "", page: 1,
    }, params, signedIn, target), { scroll: false });
  };

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
      className="flex flex-wrap items-center gap-2 text-sm"
      onSubmit={(e) => {
        e.preventDefault();
        go({});
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
          className="h-[34px] pl-8 pr-9 text-sm"
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
      {isPublicTab || starred ? null : (
        <Select aria-label="Visibility" name="visibility" value={params.visibility} className="h-[34px] w-auto text-sm" onChange={(e) => go({ visibility: e.target.value as ListParams["visibility"] })}>
          <option value="">Any visibility</option>
          <option value="public">Public</option>
          <option value="internal">Internal</option>
          <option value="private">Private</option>
        </Select>
      )}
      <Select aria-label="Language" name="language" value={params.language} className="h-[34px] w-auto text-sm" onChange={(e) => go({ language: e.target.value })}>
        <option value="">Any language</option>
        {langs.map((l) => (
          <option key={l} value={l}>
            {l}
          </option>
        ))}
      </Select>
      {starred ? null : <Input
        name="topic"
        aria-label="Topic"
        placeholder="Topic"
        autoComplete="off"
        className="h-[34px] w-32 text-sm"
        value={topic}
        onChange={(e) => {
          setTopic(e.target.value);
          later({ topic: e.target.value.trim().toLowerCase().replace(/[\s:"]+/g, "") });
        }}
      />}
      <Select aria-label="Sort" name="sort" value={params.sort} className="h-[34px] w-auto text-sm" onChange={(e) => go({ sort: e.target.value as ListParams["sort"] })}>
        <option value="updated">Recently updated</option>
        <option value="stars">Most stars</option>
        <option value="name">Name</option>
      </Select>
    </form>
  );
}
