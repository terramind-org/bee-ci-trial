import { HeaderSkeleton, PanelSkeleton, Skel } from "@/components/skeletons";

export default function Loading() {
  return (
    <Skel>
      <HeaderSkeleton title="Search" meta="Code across every repo you can read, with GitHub-style qualifiers (repo:, path:, lang:, symbol:)." />
      <div className="mb-5 flex gap-2">
        <span className="h-8 flex-1 rounded-md bg-input" />
        <span className="h-8 w-[90px] rounded-md border border-line-strong" />
      </div>
      <PanelSkeleton rows={5} rowHeight={64} />
    </Skel>
  );
}
