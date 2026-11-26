/** Achievements as a plain list: name, tier and how many times, with the reason beneath. */
import { Time } from "@/components/time";
import { Panel } from "@/components/ui";
import type { Achievement } from "@/lib/api/types";

export function Achievements({ achievements }: { achievements: Achievement[] }) {
  if (!achievements.length) return null;
  return (
    <Panel title="Achievements" id="achievements-heading">
      <ul className="rule divide-y divide-line">
        {achievements.map((a) => (
          <li key={a.slug} className="flex min-w-0 flex-col gap-0.5 py-3.5">
            <div className="flex items-baseline gap-3">
              <span className="min-w-0 truncate text-md font-medium">{a.name}</span>
              {a.tier ? <span className="shrink-0 text-sm text-ink-3">{a.tier}</span> : null}
              {a.count > 1 ? <span className="num shrink-0 text-sm text-ink-3">×{a.count}</span> : null}
            </div>
            <p className="max-w-2xl text-base text-ink-2">{a.description}</p>
            {a.unlockedAt ? (
              <p className="text-sm text-ink-3">
                Unlocked <Time iso={a.unlockedAt} />
              </p>
            ) : null}
          </li>
        ))}
      </ul>
    </Panel>
  );
}
