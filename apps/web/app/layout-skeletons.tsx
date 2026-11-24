/**
 * Suspense fallbacks for the group layouts, shown while a layout loads the shell's data: the
 * rail (the slim top bar below `md`), and on repo pages the repo header, at their rendered sizes.
 * Who is signed in is not known yet, so the fallback draws the signed-in frame most visits get.
 */
import { HeaderSkeleton, Line, ListSkeleton, Skel } from "@/components/skeletons";

function RailSkeleton() {
  return (
    <div className="hidden border-r border-line bg-sunken/50 px-3 pt-4 md:block">
      <Line w={110} lh={40} bar={14} />
      <Line w="100%" lh={36} bar={36} className="mt-4 mb-5" />
      {Array.from({ length: 5 }, (_, i) => (
        <Line key={i} w={140 - i * 12} lh={40} bar={12} className="px-2.5" />
      ))}
    </div>
  );
}

function BarSkeleton() {
  return (
    <div className="flex h-14 items-center gap-2 border-b border-line px-3 md:hidden">
      <Line w={40} lh={40} bar={22} />
      <Line w={90} lh={40} bar={14} />
    </div>
  );
}

/** A page body: title, meta and a list. */
export function PageBodySkeleton() {
  return (
    <div className="min-w-0 overflow-hidden px-5 pt-8 sm:px-8 lg:px-12 lg:pt-12">
      <div className="mx-auto max-w-[1120px]">
        <HeaderSkeleton />
        <ListSkeleton rows={8} rowHeight={59} />
      </div>
    </div>
  );
}

/** The repo header: crumb, the large name, the lede and the tabs. */
function RepoHeaderSkeleton() {
  return (
    <div className="min-w-0 overflow-hidden px-5 pt-8 sm:px-8 lg:px-12 lg:pt-12">
      <div className="mx-auto max-w-[1120px]">
        <Line w={120} lh={24} bar={10} />
        <Line w={260} lh={52} bar={32} className="mt-1" />
        <Line w={420} lh={28} bar={12} className="mt-2" />
        <div className="mt-8 flex gap-6 overflow-hidden border-b border-line">
          {Array.from({ length: 6 }, (_, i) => (
            <Line key={i} w={80} lh={44} bar={11} />
          ))}
        </div>
        <ListSkeleton rows={8} rowHeight={50} />
      </div>
    </div>
  );
}

function Frame({ children }: { children: React.ReactNode }) {
  return (
    <div className="min-h-dvh md:grid md:grid-cols-[248px_minmax(0,1fr)]">
      <BarSkeleton />
      <RailSkeleton />
      {children}
    </div>
  );
}

export function GlobalLoading() {
  return (
    <Skel>
      <Frame>
        <PageBodySkeleton />
      </Frame>
    </Skel>
  );
}

export function RepoLoading() {
  return (
    <Skel>
      <Frame>
        <RepoHeaderSkeleton />
      </Frame>
    </Skel>
  );
}
