import { HeaderSkeleton, PanelSkeleton, Skel } from "@/components/skeletons";

export default function Loading() {
  return (
    <Skel>
      <HeaderSkeleton title="Import from GitHub" meta="Dry run first. Git history arrives with identical commit ids; issues and pull requests keep their numbers." />
      <div className="grid grid-cols-1 gap-5 xl:grid-cols-[minmax(0,1fr)_320px]">
        <PanelSkeleton rows={4} rowHeight={64} />
        <PanelSkeleton title="Previous imports" rows={4} rowHeight={30} lines={1} />
      </div>
    </Skel>
  );
}
