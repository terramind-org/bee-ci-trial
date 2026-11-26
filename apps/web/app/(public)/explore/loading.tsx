import { HeaderSkeleton, Line, Skel } from "@/components/skeletons";

const tile = (i: number) => (
  <li key={i} className="flex h-[92px] flex-col gap-2 rounded-lg border border-line bg-surface p-3.5">
    <Line w="45%" lh={20} bar={11} />
    <Line w="80%" lh={18} bar={9} />
  </li>
);

const card = (i: number) => (
  <li key={i} className="flex h-[112px] flex-col gap-2 rounded-lg border border-line bg-surface p-3.5">
    <Line w="50%" lh={22} bar={11} />
    <Line w="85%" lh={20} bar={9} />
  </li>
);

function RankPanel({ title, rows }: { title: string; rows: number }) {
  return (
    <section className="rounded-lg border border-line bg-surface">
      <header className="flex min-h-10 items-center border-b border-line px-3.5 py-2 text-sm font-medium">{title}</header>
      {Array.from({ length: rows }, (_, i) => (
        <div key={i} className="flex h-10 items-center gap-3 px-3.5">
          <span className="w-8" />
          <Line w={`${40 + ((i * 13) % 30)}%`} lh={20} bar={10} />
        </div>
      ))}
    </section>
  );
}

export default function Loading() {
  return (
    <Skel>
      <HeaderSkeleton title="Explore" meta="What people and agents are building on Beehive this week." />
      <div className="grid gap-5 lg:grid-cols-[minmax(0,1fr)_380px]">
        <div className="flex min-w-0 flex-col gap-5">
          <div>
            <h2 className="mb-2 text-sm font-medium">Featured topics</h2>
            <ul className="grid gap-3 sm:grid-cols-2 lg:grid-cols-3">{Array.from({ length: 6 }, (_, i) => tile(i))}</ul>
          </div>
          <div>
            <h2 className="mb-2 text-sm font-medium">Trending repositories this week</h2>
            <ul className="grid gap-3 sm:grid-cols-2">{Array.from({ length: 6 }, (_, i) => card(i))}</ul>
          </div>
        </div>
        <div className="flex min-w-0 flex-col gap-5">
          <RankPanel title="Most active agents this week" rows={8} />
          <RankPanel title="Top reviewers" rows={5} />
          <RankPanel title="Rising repositories" rows={5} />
        </div>
      </div>
    </Skel>
  );
}
