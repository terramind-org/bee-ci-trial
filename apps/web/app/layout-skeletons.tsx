/**
 * Suspense fallbacks for the group layouts, shown while a layout loads the shell's data: the
 * rail (the slim top bar below `md`), and on repo pages the repo header, at their rendered sizes.
 * Who is signed in is not known yet, so the fallback draws the signed-in frame most visits get.
 */
import { HeaderSkeleton, Line, ListSkeleton, Skel } from "@/components/skeletons";

function RailSkeleton() {
  return (
    <div className="hidden border-r border-line px-[18px] pt-[26px] md:block">
      <Line w={110} lh={40} bar={14} className="mb-[18px]" />
      {Array.from({ length: 3 }, (_, i) => (
        <Line key={i} w={130 - i * 14} lh={38} bar={12} className="px-2.5" />
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
    <div className="min-w-0 overflow-hidden px-5 pt-8 sm:px-8 md:px-[5vw] md:pt-[7vh]">
      <div className="max-w-[1120px]">
        <HeaderSkeleton />
        <ListSkeleton rows={8} rowHeight={59} />
      </div>
    </div>
  );
}

/** The repo header: the `owner / name` line with its buttons, then the tabs. */
function RepoHeaderSkeleton() {
  return (
    <div className="min-w-0 overflow-hidden px-5 pt-8 sm:px-8 md:px-[5vw] md:pt-[7vh]">
      <div className="max-w-[1120px]">
        <Line w={320} lh={34} bar={12} className="mb-3.5" />
        <div className="flex gap-6 overflow-hidden border-b border-line">
          {Array.from({ length: 8 }, (_, i) => (
            <Line key={i} w={72} lh={40} bar={11} />
          ))}
        </div>
        <ListSkeleton rows={8} rowHeight={50} />
      </div>
    </div>
  );
}

function Frame({ children }: { children: React.ReactNode }) {
  return (
    <div className="min-h-dvh md:grid md:grid-cols-[228px_minmax(0,1fr)]">
      <BarSkeleton />
      <RailSkeleton />
      {children}
    </div>
  );
}

export function GlobalLoading() {
  return (
    <Skel frame>
      <Frame>
        <PageBodySkeleton />
      </Frame>
    </Skel>
  );
}

export function RepoLoading() {
  return (
    <Skel frame>
      <Frame>
        <RepoHeaderSkeleton />
      </Frame>
    </Skel>
  );
}
