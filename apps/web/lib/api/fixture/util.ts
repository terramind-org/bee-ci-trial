/** Small helpers shared by the fixture handlers. */
import { BeeError } from "../transport";

export const invalid = (message: string, reason = "INVALID_ARGUMENT") => new BeeError("invalid_argument", { reason, message });

/** Offset paging: the token is the index of the next item. */
export function page<X>(items: X[], req: { pageSize?: number; pageToken?: string } | undefined, def = 50): { items: X[]; next: string } {
  const size = Math.max(1, Math.min(200, req?.pageSize || def));
  const start = Number(req?.pageToken || 0) || 0;
  return { items: items.slice(start, start + size), next: start + size < items.length ? String(start + size) : "" };
}

/** Whether two paths overlap (one contains the other); "" overlaps everything. */
export function overlaps(a: string, b: string): boolean {
  const x = a.replace(/\/$/, "");
  const y = b.replace(/\/$/, "");
  return x === y || x.startsWith(y + "/") || y.startsWith(x + "/") || x === "" || y === "";
}

/** Yields every `ms` until the signal aborts. */
export async function* waitLoop(signal: AbortSignal | undefined, ms: number): AsyncGenerator<void> {
  while (!signal?.aborted) {
    await new Promise<void>((resolve) => {
      if (signal?.aborted) return resolve();
      const t = setTimeout(resolve, ms);
      signal?.addEventListener(
        "abort",
        () => {
          clearTimeout(t);
          resolve();
        },
        { once: true },
      );
    });
    if (signal?.aborted) return;
    yield;
  }
}
