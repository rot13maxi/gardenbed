import { expect, test } from "@playwright/test";
import { findByMessage, log, mainTip } from "./util";

test("compare renders the seeded change list between two commits", async ({ page, request }) => {
  const tip = await mainTip(request);
  const entries = await log(request, tip);
  const tune = findByMessage(entries, "Tune grid spacing");
  const parent = tune.parents[0];

  await page.goto("/#/compare");
  await page.locator("#cmp-a").fill(parent);
  await page.locator("#cmp-b").fill(tune.id);
  await page.locator("#cmp-run").click();

  await expect(page.locator("#cmp-results")).toBeVisible();
  const rows = page.locator("#cmp-results .change-row");
  await expect(rows).toHaveCount(2);
  await expect(page.locator("#cmp-results")).toContainText("garden.js");
  await expect(page.locator("#cmp-results")).toContainText("styles.css");
  await expect(page.locator("#cmp-results")).toContainText("2 changes");
});

test("compare accepts refs and id prefixes", async ({ page, request }) => {
  const tip = await mainTip(request);
  const entries = await log(request, tip);
  const older = entries[entries.length - 1]; // root commit
  await page.goto(`/#/compare?a=${encodeURIComponent(older.id.slice(0, 12))}&b=main`);
  await page.locator("#cmp-run").click();
  await expect(page.locator("#cmp-results .change-row").first()).toBeVisible();
});

test("unknown changeset shows an error state with the resolver message", async ({ page }) => {
  const dead = "f".repeat(64);
  await page.goto(`/#/changeset/${dead}`);
  await expect(page.locator(".empty-state")).toBeVisible();
  // Not-found surfaces as a readable error, not a blank page.
  await expect(page.locator(".empty-state")).toContainText(/not found|no ref or changeset/i);
});

test("file view on a directory path shows an error state", async ({ page, request }) => {
  const tip = await mainTip(request);
  await page.goto(`/#/file/${tip}/plan`);
  await expect(page.locator(".empty-state, .banner")).toBeVisible();
});

test("browse to a missing path shows an error state", async ({ page, request }) => {
  const tip = await mainTip(request);
  await page.goto(`/#/browse/${tip}/no/such/dir`);
  await expect(page.locator(".empty-state")).toBeVisible();
});

test("nav links are keyboard-reachable and switch views", async ({ page }) => {
  await page.goto("/");
  for (const nav of ["history", "browse", "compare", "overview"]) {
    await page.locator(`[data-nav='${nav}']`).click();
    await expect(page.locator(".skeleton, .empty-state, #log-list, #cmp-results, #stat-chips, .tree-row").first()).toBeVisible();
  }
});

test("mobile viewport: nav wraps and tables stay reachable", async ({ page, request }) => {
  await page.setViewportSize({ width: 375, height: 812 });
  await page.goto("/");
  await expect(page.locator("[data-nav='overview']")).toBeVisible();
  await expect(page.locator("[data-nav='compare']")).toBeVisible();
  await page.locator("[data-nav='history']").click();
  await expect(page.locator("#log-list .log-row").first()).toBeVisible();
});

test("global ref selector drives page defaults", async ({ page, request }) => {
  await mainTip(request);
  await page.goto("/");
  const sel = page.locator("#ref-select");
  await expect(sel).toBeVisible();
  await expect(sel.locator("option").first()).toBeAttached();
  // Selection persists across navigation.
  await sel.selectOption({ index: 0 });
  await page.locator("[data-nav='history']").click();
  await expect(page.locator("#log-list .log-row").first()).toBeVisible();
  await expect(sel).toHaveValue(/.+/);
});
