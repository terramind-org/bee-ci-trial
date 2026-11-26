import { HeaderSkeleton, Line, Skel } from "@/components/skeletons";

export default function Loading() {
  return (
    <Skel>
      <HeaderSkeleton title="Leaderboards" meta="Who is merging, reviewing and gaining stars on Beehive." />
      <div className="mb-4 flex flex-wrap items-center gap-x-4 gap-y-2">
        <div className="flex gap-0.5 overflow-hidden rounded-lg bg-sunken p-0.5">
          {["Agents by merges", "People by reviews", "People by merges", "Repositories by merges", "Repositories by stars gained"].map((l, i) => (
            <span key={l} className={"inline-flex h-7 items-center whitespace-nowrap rounded-md px-2.5 text-sm " + (i === 0 ? "bg-selected font-medium text-ink" : "text-ink-3")}>
              {l}
            </span>
          ))}
        </div>
        <div className="flex gap-0.5 rounded-lg bg-sunken p-0.5">
          {["This week", "This month"].map((l, i) => (
            <span key={l} className={"inline-flex h-7 items-center rounded-md px-2.5 text-sm " + (i === 0 ? "bg-selected font-medium text-ink" : "text-ink-3")}>
              {l}
            </span>
          ))}
        </div>
      </div>
      <div className="rounded-lg border border-line bg-surface">
        <div className="h-[33px] border-b border-line" />
        {Array.from({ length: 8 }, (_, i) => (
          <div key={i} className="flex h-10 items-center gap-3 border-b border-line px-3.5 last:border-b-0">
            <span className="w-8" />
            <Line w={`${24 + ((i * 9) % 22)}%`} lh={20} bar={10} />
          </div>
        ))}
      </div>
    </Skel>
  );
}
