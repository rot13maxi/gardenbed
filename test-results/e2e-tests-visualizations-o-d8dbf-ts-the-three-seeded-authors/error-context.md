# Instructions

- Following Playwright test failed.
- Explain why, be concise, respect Playwright best practices.
- Provide a snippet of code with the fix, if possible.

# Test info

- Name: e2e/tests/visualizations.spec.ts >> overview authors chart lists the three seeded authors
- Location: e2e/tests/visualizations.spec.ts:57:5

# Error details

```
Error: page.goto: Protocol error (Page.navigate): Cannot navigate to invalid URL
Call log:
  - navigating to "/", waiting until "load"

```

# Test source

```ts
  1  | import { expect, test } from "@playwright/test";
  2  | import { findByMessage, log, mainTip } from "./util";
  3  | 
  4  | test("history renders a commit-graph column aligned with rows", async ({ page, request }) => {
  5  |   const tip = await mainTip(request);
  6  |   const entries = await log(request, tip);
  7  |   await page.goto("/#/log");
  8  |   const rows = page.locator("#log-list .log-row");
  9  |   await rows.first().waitFor();
  10 | 
  11 |   const graph = page.locator(".commit-graph");
  12 |   await expect(graph).toBeVisible();
  13 |   expect(await page.locator(".commit-graph .graph-node").count()).toBe(await rows.count());
  14 |   expect(await page.locator(".commit-graph .graph-edge").count()).toBeGreaterThanOrEqual(
  15 |     (await rows.count()) - 1,
  16 |   );
  17 |   expect(entries.length).toBeGreaterThanOrEqual(10);
  18 | });
  19 | 
  20 | test("overview shows the activity heatmap with seeded days", async ({ page, request }) => {
  21 |   const tip = await mainTip(request);
  22 |   const entries = await log(request, tip);
  23 |   // Seeded commits: one per day at 86_400s steps starting 1751586400.
  24 |   await page.goto("/");
  25 |   const cells = page.locator("#activity-heatmap .heat-cell");
  26 |   await expect(cells.first()).toBeVisible();
  27 | 
  28 |   const counts = await cells.evaluateAll((els) =>
  29 |     els.map((el) => Number(el.getAttribute("data-count") ?? "0")),
  30 |   );
  31 |   const dates = await cells.evaluateAll((els) =>
  32 |     els.map((el) => el.getAttribute("data-date")),
  33 |   );
  34 |   for (const d of dates) {
  35 |     expect(d).toMatch(/^\d{4}-\d{2}-\d{2}$/);
  36 |   }
  37 | 
  38 |   // The window anchors to the newest commit day; independently recompute
  39 |   // the expected count per day from the API and compare cell-by-cell.
  40 |   // Seeded identities use tz_min=0, so civil day == UTC day.
  41 |   const byDay = new Map<string, number>();
  42 |   for (const e of entries) {
  43 |     const day = new Date(e.committer.time_secs * 1000).toISOString().slice(0, 10);
  44 |     byDay.set(day, (byDay.get(day) ?? 0) + 1);
  45 |   }
  46 |   let mismatches = 0;
  47 |   for (let i = 0; i < dates.length; i++) {
  48 |     const want = byDay.get(dates[i]!) ?? 0;
  49 |     if (counts[i] !== want) mismatches++;
  50 |   }
  51 |   expect(mismatches).toBe(0);
  52 |   // Seeded history: ten distinct days with one changeset each are inside
  53 |   // the window.
  54 |   expect([...byDay.values()].reduce((a, b) => a + b, 0)).toBeGreaterThanOrEqual(10);
  55 | });
  56 | 
  57 | test("overview authors chart lists the three seeded authors", async ({ page }) => {
> 58 |   await page.goto("/");
     |              ^ Error: page.goto: Protocol error (Page.navigate): Cannot navigate to invalid URL
  59 |   const chart = page.locator("#authors-chart");
  60 |   await expect(chart).toBeVisible();
  61 |   for (const author of ["Rosa Bloom", "Ken Furrow", "Ada Trellis"]) {
  62 |     await expect(chart.locator(".author-row", { hasText: author })).toBeVisible();
  63 |   }
  64 |   // Rosa authored 6 of 12 seeded changesets → widest bar.
  65 |   const rosa = chart.locator(".author-row", { hasText: "Rosa Bloom" }).locator(".author-bar");
  66 |   const width = await rosa.evaluate((el) => el.getBoundingClientRect().width);
  67 |   expect(width).toBeGreaterThan(0);
  68 | });
  69 | 
  70 | test("changeset page shows diffstat chips and bar", async ({ page, request }) => {
  71 |   const tip = await mainTip(request);
  72 |   const tune = findByMessage(await log(request, tip), "Tune grid spacing");
  73 |   await page.goto(`/#/changeset/${tune.id}`);
  74 | 
  75 |   await expect(page.locator("#cs-header .diffstat")).toBeVisible();
  76 |   await expect(page.locator(".diff-chip[data-op='modify']")).toContainText("2");
  77 | });
  78 | 
  79 | test("changeset diffstat reflects the rename commit", async ({ page, request }) => {
  80 |   const tip = await mainTip(request);
  81 |   const rename = findByMessage(await log(request, tip), "Rename spring plan");
  82 |   await page.goto(`/#/changeset/${rename.id}`);
  83 |   await expect(page.locator(".diff-chip[data-op='rename']")).toContainText("1");
  84 | });
  85 | 
```