# Instructions

- Following Playwright test failed.
- Explain why, be concise, respect Playwright best practices.
- Provide a snippet of code with the fix, if possible.

# Test info

- Name: e2e/tests/compare-errors.spec.ts >> unknown changeset shows an error state with the resolver message
- Location: e2e/tests/compare-errors.spec.ts:32:5

# Error details

```
Error: page.goto: Protocol error (Page.navigate): Cannot navigate to invalid URL
Call log:
  - navigating to "/#/changeset/ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff", waiting until "load"

```

# Test source

```ts
  1  | import { expect, test } from "@playwright/test";
  2  | import { findByMessage, log, mainTip } from "./util";
  3  | 
  4  | test("compare renders the seeded change list between two commits", async ({ page, request }) => {
  5  |   const tip = await mainTip(request);
  6  |   const entries = await log(request, tip);
  7  |   const tune = findByMessage(entries, "Tune grid spacing");
  8  |   const parent = tune.parents[0];
  9  | 
  10 |   await page.goto("/#/compare");
  11 |   await page.locator("#cmp-a").fill(parent);
  12 |   await page.locator("#cmp-b").fill(tune.id);
  13 |   await page.locator("#cmp-run").click();
  14 | 
  15 |   await expect(page.locator("#cmp-results")).toBeVisible();
  16 |   const rows = page.locator("#cmp-results .change-row");
  17 |   await expect(rows).toHaveCount(2);
  18 |   await expect(page.locator("#cmp-results")).toContainText("garden.js");
  19 |   await expect(page.locator("#cmp-results")).toContainText("styles.css");
  20 |   await expect(page.locator("#cmp-results")).toContainText("2 changes");
  21 | });
  22 | 
  23 | test("compare accepts refs and id prefixes", async ({ page, request }) => {
  24 |   const tip = await mainTip(request);
  25 |   const entries = await log(request, tip);
  26 |   const older = entries[entries.length - 1]; // root commit
  27 |   await page.goto(`/#/compare?a=${encodeURIComponent(older.id.slice(0, 12))}&b=main`);
  28 |   await page.locator("#cmp-run").click();
  29 |   await expect(page.locator("#cmp-results .change-row").first()).toBeVisible();
  30 | });
  31 | 
  32 | test("unknown changeset shows an error state with the resolver message", async ({ page }) => {
  33 |   const dead = "f".repeat(64);
> 34 |   await page.goto(`/#/changeset/${dead}`);
     |              ^ Error: page.goto: Protocol error (Page.navigate): Cannot navigate to invalid URL
  35 |   await expect(page.locator(".empty-state")).toBeVisible();
  36 |   // Not-found surfaces as a readable error, not a blank page.
  37 |   await expect(page.locator(".empty-state")).toContainText(/not found|no ref or changeset/i);
  38 | });
  39 | 
  40 | test("file view on a directory path shows an error state", async ({ page, request }) => {
  41 |   const tip = await mainTip(request);
  42 |   await page.goto(`/#/file/${tip}/plan`);
  43 |   await expect(page.locator(".empty-state, .banner")).toBeVisible();
  44 | });
  45 | 
  46 | test("browse to a missing path shows an error state", async ({ page, request }) => {
  47 |   const tip = await mainTip(request);
  48 |   await page.goto(`/#/browse/${tip}/no/such/dir`);
  49 |   await expect(page.locator(".empty-state")).toBeVisible();
  50 | });
  51 | 
  52 | test("nav links are keyboard-reachable and switch views", async ({ page }) => {
  53 |   await page.goto("/");
  54 |   for (const nav of ["history", "browse", "compare", "overview"]) {
  55 |     await page.locator(`[data-nav='${nav}']`).click();
  56 |     await expect(page.locator(".skeleton, .empty-state, #log-list, #cmp-results, #stat-chips, .tree-row").first()).toBeVisible();
  57 |   }
  58 | });
  59 | 
  60 | test("mobile viewport: nav wraps and tables stay reachable", async ({ page, request }) => {
  61 |   await page.setViewportSize({ width: 375, height: 812 });
  62 |   await page.goto("/");
  63 |   await expect(page.locator("[data-nav='overview']")).toBeVisible();
  64 |   await expect(page.locator("[data-nav='compare']")).toBeVisible();
  65 |   await page.locator("[data-nav='history']").click();
  66 |   await expect(page.locator("#log-list .log-row").first()).toBeVisible();
  67 | });
  68 | 
  69 | test("global ref selector drives page defaults", async ({ page, request }) => {
  70 |   await mainTip(request);
  71 |   await page.goto("/");
  72 |   const sel = page.locator("#ref-select");
  73 |   await expect(sel).toBeVisible();
  74 |   await expect(sel.locator("option").first()).toBeAttached();
  75 |   // Selection persists across navigation.
  76 |   await sel.selectOption({ index: 0 });
  77 |   await page.locator("[data-nav='history']").click();
  78 |   await expect(page.locator("#log-list .log-row").first()).toBeVisible();
  79 |   await expect(sel).toHaveValue(/.+/);
  80 | });
  81 | 
```