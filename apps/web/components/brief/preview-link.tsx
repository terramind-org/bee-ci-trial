/**
 * A link to a running preview of the change (`PREVIEW_LINK` evidence, or the brief's try-it URL).
 * Only https URLs are links; anything else is shown as text and never followed.
 */
import { ArrowSquareOutIcon, GlobeIcon } from "@phosphor-icons/react/dist/ssr";

export function safePreviewUrl(url: string): URL | null {
  try {
    const u = new URL(url);
    return u.protocol === "https:" && !u.username && !u.password ? u : null;
  } catch {
    return null;
  }
}

export function PreviewLink({ url, title = "Try it", caption }: { url: string; title?: string; caption?: string }) {
  const u = safePreviewUrl(url);
  const shown = u ? u.host + (u.pathname === "/" ? "" : u.pathname) : url;
  return (
    <div className="flex items-center gap-3" data-preview-link>
      <span className="grid size-10 shrink-0 place-items-center rounded-full bg-sunken text-ink-2">
        <GlobeIcon size={20} aria-hidden="true" />
      </span>
      <div className="flex min-w-0 flex-1 flex-col">
        {u ? (
          <a href={u.toString()} target="_blank" rel="noreferrer noopener" className="inline-flex items-center gap-1 text-base font-medium text-ink hover:underline">
            {title}
            <ArrowSquareOutIcon size={14} aria-hidden="true" />
            <span className="sr-only">(opens in a new tab)</span>
          </a>
        ) : (
          <span className="text-base font-medium text-ink">{title}</span>
        )}
        <span className="mono truncate text-sm text-ink-3" title={url}>
          {shown}
        </span>
        {caption ? <span className="text-sm text-ink-2">{caption}</span> : null}
      </div>
    </div>
  );
}
