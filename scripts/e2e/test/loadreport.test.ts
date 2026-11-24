import { expect, test } from "bun:test";
import { parseTargets, summarize } from "../lib/loadreport";

const report = `- Date: 2026-10-20
- Store: SQLite

| Operation | Target | p50 | p99 | Measured | Result | Notes |
|---|---|---|---|---|---|---|
| CreateChangeset, 1 file changed, server time | p50 ≤ 10 ms, p99 ≤ 50 ms | 20.00 ms | 41.49 ms | 200 samples | **miss** (by 100 %) | repo of 11400 files |
| Land, uncontended, 1-change stack | p50 ≤ 30 ms | 6.91 ms | 30.18 ms | 200 samples | hit (77 % margin) |  |
| Availability | reads 99.99% | – | – | observed | not judged: a monthly SLO | every request |

| Extra measurement | Value |
|---|---|
| Mixed reads | 332 reads/s |
`;

test("the target table is parsed and the extras table is left alone", () => {
  const rows = parseTargets(report);
  expect(rows.map((r) => r.status)).toEqual(["miss", "hit", "not judged"]);
  expect(rows[0]!.by).toBe("100 %");
  expect(rows[0]!.operation).toBe("CreateChangeset, 1 file changed, server time");
});

test("summaries count misses", () => {
  const s = summarize(parseTargets(report));
  expect(s.hits).toBe(1);
  expect(s.misses.length).toBe(1);
  expect(s.notJudged).toBe(1);
});

test("a report with no table has no rows", () => {
  expect(parseTargets("nothing here")).toEqual([]);
});
