import { TabLinks } from "@/components/ui";

export type ExploreView = "overview" | "trending" | "leaderboards";

/**
 * Explore, Trending and Leaderboards are three views of one place. Each keeps its own URL (so
 * every old link still works) and they share these tabs, which keeps the rail at five items.
 */
export function ExploreTabs({ current }: { current: ExploreView }) {
  const items: { view: ExploreView; href: string; label: string }[] = [
    { view: "overview", href: "/explore", label: "Overview" },
    { view: "trending", href: "/trending", label: "Trending" },
    { view: "leaderboards", href: "/leaderboards", label: "Leaderboards" },
  ];
  return <TabLinks label="Explore sections" items={items.map((i) => ({ href: i.href, label: i.label, active: i.view === current }))} />;
}
