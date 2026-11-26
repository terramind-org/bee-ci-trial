/**
 * The composer's transport for `uploadPostMedia` (lib/post-media.ts): a `fetch` for its one PUT
 * that goes to the same-origin route instead of the server's signed URL, and reports progress.
 * `fetch` cannot report upload progress, so the PUT is an XMLHttpRequest wrapped as a Response.
 */
import { uploadPath } from "@/lib/post-compose";

/** Headers the browser sets itself and refuses from script. */
const FORBIDDEN = new Set(["content-length", "host", "connection", "accept-encoding", "cookie", "origin", "referer"]);

export function progressFetch(onProgress: (fraction: number) => void): typeof fetch {
  return ((input: RequestInfo | URL, init?: RequestInit) =>
    new Promise<Response>((resolve, reject) => {
      const raw = typeof input === "string" ? input : input instanceof URL ? input.href : input.url;
      const target = uploadPath(raw);
      if (!target) {
        reject(new Error("the server answered an upload address this page does not send to"));
        return;
      }
      const xhr = new XMLHttpRequest();
      xhr.open(init?.method ?? "PUT", target);
      for (const [k, v] of Object.entries((init?.headers ?? {}) as Record<string, string>)) {
        if (!FORBIDDEN.has(k.toLowerCase())) xhr.setRequestHeader(k, v);
      }
      xhr.upload.onprogress = (e) => {
        if (e.lengthComputable && e.total > 0) onProgress(e.loaded / e.total);
      };
      xhr.onload = () => {
        onProgress(1);
        const empty = xhr.status === 204 || xhr.status === 304;
        resolve(new Response(empty ? null : xhr.responseText, { status: xhr.status, headers: { "content-type": xhr.getResponseHeader("content-type") ?? "text/plain" } }));
      };
      xhr.onerror = () => reject(new Error("the network dropped the upload"));
      xhr.onabort = () => reject(new Error("the upload was cancelled"));
      xhr.send((init?.body ?? null) as XMLHttpRequestBodyInit | null);
    })) as typeof fetch;
}
