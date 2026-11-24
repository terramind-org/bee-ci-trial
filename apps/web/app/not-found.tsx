import Link from "next/link";

export default function NotFound() {
  return (
    <main id="main" className="mx-auto flex min-h-dvh max-w-md flex-col items-center justify-center gap-3 px-6 text-center">
      <p className="mono text-sm text-ink-3">NOT_FOUND</p>
      <h1 className="text-xl font-semibold">Nothing here</h1>
      <p className="text-ink-2">It may have been renamed, or your token cannot see it. Existence is never leaked, so the two look the same.</p>
      <Link href="/" className="link">
        Back to Beehive
      </Link>
    </main>
  );
}
