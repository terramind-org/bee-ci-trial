import { HeaderSkeleton, Skel } from "@/components/skeletons";
import { Line } from "@/components/skeletons";

export default function Loading() {
  return (
    <Skel>
      <HeaderSkeleton title="Trending" meta="What is gaining attention on Beehive, this week." />
      <div className="mb-4 flex flex-wrap items-center gap-x-4 gap-y-2">
        <div className="flex gap-0.5 rounded-lg bg-sunken p-0.5">
          {["Repositories", "Developers", "Agents"].map((l, i) => (
            <span key={l} className={"inline-flex h-7 items-center rounded-md px-2.5 text-sm " + (i === 0 ? "bg-selected font-medium text-ink" : "text-ink-3")}>
              {l}
            </span>
          ))}
        </div>
        <div className="flex gap-0.5 rounded-lg bg-sunken p-0.5">
          {["Today", "This week", "This month"].map((l, i) => (
            <span key={l} className={"inline-flex h-7 items-center rounded-md px-2.5 text-sm " + (i === 1 ? "bg-selected font-medium text-ink" : "text-ink-3")}>
              {l}
            </span>
          ))}
        </div>
        <div className="flex items-center gap-2">
          <span className="text-sm text-ink-2">Language</span>
          <span className="h-8 w-40 rounded-md bg-input" />
          <span className="h-7 w-[58px] rounded-md border border-line-strong" />
        </div>
      </div>
      <div className="divide-y divide-line rounded-lg border border-line bg-surface">
        {Array.from({ length: 6 }, (_, i) => (
          <div key={i} className="flex h-[84px] items-start gap-3 px-3.5 pt-3">
            <span className="w-8" />
            <div className="flex min-w-0 flex-1 flex-col gap-1">
              <Line w={`${28 + ((i * 11) % 20)}%`} lh={22} bar={11} />
              <Line w="70%" lh={20} bar={9} />
              <Line w="40%" lh={16} bar={8} />
            </div>
          </div>
        ))}
      </div>
    </Skel>
  );
}
