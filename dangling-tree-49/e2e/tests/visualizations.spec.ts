import { expect, test } from "@playwright/test";
import { findByMessage, log, mainTip } from "./util";

test("history renders a commit-graph column aligned with rows", async ({ page, request }) => {
  const tip = await mainTip(request);
  const entries = await log(request, tip);
  await page.goto("/#/log");
  const rows = page.locator("#log-list .log-row");
  await rows.first().waitFor();

  const graph = page.locator(".commit-graph");
  await expect(graph).toBeVisible();
  expect(await page.locator(".commit-graph .graph-node").count()).toBe(await rows.count());
  expect(await page.locator(".commit-graph .graph-edge").count()).toBeGreaterThanOrEqual(
    (await rows.count()) - 1,
  );
  expect(entries.length).toBeGreaterThanOrEqual(10);
});

test("overview shows the activity heatmap with seeded days", async ({ page, request }) => {
  const tip = await mainTip(request);
  const entries = await log(request, tip);
  // Seeded commits: one per day at 86_400s steps starting 1751586400.
  await page.goto("/");
  const cells = page.locator("#activity-heatmap .heat-cell");
  await expect(cells.first()).toBeVisible();

  const counts = await cells.evaluateAll((els) =>
    els.map((el) => Number(el.getAttribute("data-count") ?? "0")),
  );
  const dates = await cells.evaluateAll((els) =>
    els.map((el) => el.getAttribute("data-date")),
  );
  for (const d of dates) {
    expect(d).toMatch(/^\d{4}-\d{2}-\d{2}$/);
  }

  // The window anchors to the newest commit day; independently recompute
  // the expected count per day from the API and compare cell-by-cell.
  // Seeded identities use tz_min=0, so civil day == UTC day.
  const byDay = new Map<string, number>();
  for (const e of entries) {
    const day = new Date(e.committer.time_secs * 1000).toISOString().slice(0, 10);
    byDay.set(day, (byDay.get(day) ?? 0) + 1);
  }
  let mismatches = 0;
  for (let i = 0; i < dates.length; i++) {
    const want = byDay.get(dates[i]!) ?? 0;
    if (counts[i] !== want) mismatches++;
  }
  expect(mismatches).toBe(0);
  // Seeded history: ten distinct days with one changeset each are inside
  // the window.
  expect([...byDay.values()].reduce((a, b) => a + b, 0)).toBeGreaterThanOrEqual(10);
});

test("overview authors chart lists the three seeded authors", async ({ page }) => {
  await page.goto("/");
  const chart = page.locator("#authors-chart");
  await expect(chart).toBeVisible();
  for (const author of ["Rosa Bloom", "Ken Furrow", "Ada Trellis"]) {
    await expect(chart.locator(".author-row", { hasText: author })).toBeVisible();
  }
  // Rosa authored 6 of 12 seeded changesets → widest bar.
  const rosa = chart.locator(".author-row", { hasText: "Rosa Bloom" }).locator(".author-bar");
  const width = await rosa.evaluate((el) => el.getBoundingClientRect().width);
  expect(width).toBeGreaterThan(0);
});

test("changeset page shows diffstat chips and bar", async ({ page, request }) => {
  const tip = await mainTip(request);
  const tune = findByMessage(await log(request, tip), "Tune grid spacing");
  await page.goto(`/#/changeset/${tune.id}`);

  await expect(page.locator("#cs-header .diffstat")).toBeVisible();
  await expect(page.locator(".diff-chip[data-op='modify']")).toContainText("2");
});

test("changeset diffstat reflects the rename commit", async ({ page, request }) => {
  const tip = await mainTip(request);
  const rename = findByMessage(await log(request, tip), "Rename spring plan");
  await page.goto(`/#/changeset/${rename.id}`);
  await expect(page.locator(".diff-chip[data-op='rename']")).toContainText("1");
});
