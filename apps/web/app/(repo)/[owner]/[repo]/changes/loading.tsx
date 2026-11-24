import { Line, Skel, TabsSkeleton } from "@/components/skeletons";
import { PageHeader } from "@/components/ui";

function RowPlaceholder({ i }: { i: number }) {
  return (
    <li className="flex items-start gap-4 py-4">
      <span className="h-14 w-[88px] shrink-0 animate-pulse rounded-md bg-sunken max-sm:h-12 max-sm:w-16" />
      <div className="flex min-w-0 flex-1 flex-col gap-2">
        <Line w={i % 2 ? "58%" : "72%"} lh={24} bar={14} />
        <Line w="84%" lh={24} bar={10} />
        <Line w="46%" lh={24} bar={10} />
      </div>
    </li>
  );
}

export default function Loading() {
  return (
    <Skel>
      <PageHeader title="Changes" lede="What needs a decision, what can land, and what is waiting on agents." />
      <TabsSkeleton labels={["Open", "Landed", "Closed", "All"]} />
      <div className="mb-6 flex gap-2">
        <span className="h-9 flex-1 rounded-md bg-input" />
        <span className="h-9 w-[92px] rounded-md border border-line-strong" />
      </div>
      <ul className="divide-y divide-line">
        {[0, 1, 2, 3].map((i) => (
          <RowPlaceholder key={i} i={i} />
        ))}
      </ul>
    </Skel>
  );
}
