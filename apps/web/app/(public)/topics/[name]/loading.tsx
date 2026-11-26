import { HeaderSkeleton, Line, Skel } from "@/components/skeletons";

export default function Loading() {
  return (
    <Skel>
      <HeaderSkeleton eyebrow metaWidth={260} />
      <div className="mb-5 flex max-w-2xl flex-col gap-2">
        <Line w="90%" lh={22} bar={10} />
        <Line w="70%" lh={22} bar={10} />
      </div>
      <div className="mb-4 flex w-fit gap-0.5 rounded-lg bg-sunken p-0.5">
        {["Most stars", "Recently updated", "Most forks"].map((l, i) => (
          <span key={l} className={"inline-flex h-7 items-center rounded-md px-2.5 text-sm " + (i === 0 ? "bg-selected font-medium text-ink" : "text-ink-3")}>
            {l}
          </span>
        ))}
      </div>
      <ul className="grid gap-3 sm:grid-cols-2 xl:grid-cols-3">
        {Array.from({ length: 6 }, (_, i) => (
          <li key={i} className="flex h-[112px] flex-col gap-2 rounded-lg border border-line bg-surface p-3.5">
            <Line w="50%" lh={22} bar={11} />
            <Line w="85%" lh={20} bar={9} />
          </li>
        ))}
      </ul>
    </Skel>
  );
}
