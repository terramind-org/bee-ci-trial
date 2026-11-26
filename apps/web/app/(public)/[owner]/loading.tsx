import { Line, PanelSkeleton, Skel, TabsSkeleton } from "@/components/skeletons";

/** The profile of a person or agent: identity column, then tabs, README, repo cards and the graph. */
export default function Loading() {
  return (
    <Skel>
      <div className="grid grid-cols-1 gap-6 lg:grid-cols-[280px_minmax(0,1fr)]">
        <div className="flex flex-col gap-4">
          <span className="block size-[72px] rounded-full bg-hover" />
          <Line w={180} lh={28} bar={16} />
          <Line w={110} lh={24} bar={10} />
          <Line w="90%" lh={20} bar={9} />
          <Line w={96} lh={32} bar={32} />
          <Line w={160} lh={20} bar={9} />
        </div>
        <div className="min-w-0">
          <TabsSkeleton labels={["Overview", "Repositories", "Stars", "Followers", "Following"]} counts={false} />
          <div className="flex flex-col gap-5">
            <PanelSkeleton title="README" rows={3} rowHeight={40} lines={1} />
            <div className="grid gap-3 md:grid-cols-2">
              {Array.from({ length: 4 }, (_, i) => (
                <div key={i} className="flex flex-col gap-2 rounded-lg border border-line bg-surface p-3.5">
                  <Line w={`${48 + (i % 3) * 10}%`} lh={24} bar={12} />
                  <Line w="86%" lh={20} bar={9} />
                  <Line w="40%" lh={20} bar={9} />
                </div>
              ))}
            </div>
            <PanelSkeleton title="Contributions" rows={3} rowHeight={40} lines={1} />
          </div>
        </div>
      </div>
    </Skel>
  );
}
