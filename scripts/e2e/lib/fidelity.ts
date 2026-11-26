// The fidelity-report rules of the exit test (PLAN phase 1, step 1): a drop is acceptable only
// when it carries a reason, and the counts have to add up, so nothing is lost without a record.

export interface Drop {
  ref?: string;
  reason?: string;
}

export interface Item {
  category: string;
  imported?: string | number;
  archived?: string | number;
  dropped?: string | number;
  drops?: Drop[];
}

export interface FidelityReport {
  importId?: string;
  items?: Item[];
  manualSteps?: string[];
}

export interface FidelitySummary {
  categories: number;
  imported: number;
  archived: number;
  dropped: number;
  /** Drops with a reason, by category and reason. */
  explained: { category: string; reason: string; count: number }[];
  /** Problems: a drop without a reason, or a count that disagrees with the listed drops. */
  unexplained: string[];
}

const n = (v: string | number | undefined): number => Number(v ?? 0);

export function checkFidelity(report: FidelityReport): FidelitySummary {
  const items = report.items ?? [];
  const sum: FidelitySummary = { categories: items.length, imported: 0, archived: 0, dropped: 0, explained: [], unexplained: [] };
  for (const item of items) {
    sum.imported += n(item.imported);
    sum.archived += n(item.archived);
    sum.dropped += n(item.dropped);
    const drops = item.drops ?? [];
    if (drops.length !== n(item.dropped)) {
      sum.unexplained.push(`${item.category}: ${n(item.dropped)} dropped but ${drops.length} listed`);
    }
    const byReason = new Map<string, number>();
    for (const d of drops) {
      const reason = (d.reason ?? "").trim();
      if (!reason) sum.unexplained.push(`${item.category}: drop of ${d.ref ?? "(unnamed)"} has no reason`);
      else byReason.set(reason, (byReason.get(reason) ?? 0) + 1);
    }
    for (const [reason, count] of [...byReason].sort(([a], [b]) => a.localeCompare(b))) {
      sum.explained.push({ category: item.category, reason, count });
    }
  }
  return sum;
}
