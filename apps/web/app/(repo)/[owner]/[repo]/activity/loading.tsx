import { Line, PanelSkeleton, HeaderSkeleton, Skel, StatsSkeleton } from "@/components/skeletons";

export default function Loading() {
  return (
    <Skel>
      <HeaderSkeleton title="Activity" meta="Everything agents and people do, as it happens." />
      <StatsSkeleton labels={["Agents working", "Open changes", "Live leases", "Agent spend, 7 days"]} />
      <div className="grid grid-cols-1 gap-5 xl:grid-cols-[minmax(0,1fr)_340px]">
        <section className="rounded-lg border border-line bg-surface">
          <header className="flex items-center gap-2 border-b border-line px-3.5 py-2">
            <h2 className="mr-1 text-base font-semibold">Stream</h2>
            <span className="flex h-7 items-center">
              <Line w={40} lh={20} bar={9} />
            </span>
          </header>
          <div className="flex gap-1 border-b border-line px-2.5 py-1.5">
            {["Everything", "Merging", "Review", "Leases", "Checks and runs", "Issues and releases"].map((l, i) => (
              <span key={l} className={"inline-flex h-7 items-center rounded-md px-2.5 text-sm " + (i === 0 ? "bg-selected font-medium text-ink" : "text-ink-3")}>
                {l}
              </span>
            ))}
          </div>
          <ul className="divide-y divide-line">
            {Array.from({ length: 12 }, (_, i) => (
              <li key={i} className="flex h-[62px] items-start gap-3 px-3.5 py-2.5">
                <span className="mt-0.5 size-[22px] shrink-0 rounded-[5px] bg-hover" />
                <div className="flex min-w-0 flex-1 flex-col gap-2 pt-1">
                  <span className="block h-2.5 rounded-[3px] bg-hover" style={{ width: [64, 52, 70, 46, 58, 66][i % 6] + "%" }} />
                  <span className="block h-2 w-24 rounded-[3px] bg-hover/70" />
                </div>
              </li>
            ))}
          </ul>
        </section>
        <div className="flex flex-col gap-5">
          <PanelSkeleton title="Working now" rows={4} rowHeight={81} actions />
          <PanelSkeleton title="Merge queue · main" rows={2} rowHeight={52} actions />
        </div>
      </div>
    </Skel>
  );
}
