# Instructions

- Following Playwright test failed.
- Explain why, be concise, respect Playwright best practices.
- Provide a snippet of code with the fix, if possible.

# Test info

- Name: finder-keyboard.spec.ts >> j/k/Enter walk the history list; ? shows the shortcut help
- Location: tests/finder-keyboard.spec.ts:54:5

# Error details

```
Test timeout of 30000ms exceeded.
```

```
Error: locator.waitFor: Target crashed 
Call log:
  - waiting for locator('#log-list .log-row').first() to be visible

```

# Test source

```ts
  1  | import { expect, test } from "@playwright/test";
  2  | import { log, mainTip } from "./util";
  3  | 
  4  | test("file finder opens on 't', fuzzy-filters, and opens the top hit", async ({ page, request }) => {
  5  |   await mainTip(request);
  6  |   await page.goto("/");
  7  |   await page.keyboard.press("t");
  8  |   await expect(page.locator("#file-finder")).toBeVisible();
  9  |   await expect(page.locator("#file-finder-input")).toBeFocused();
  10 | 
  11 |   await page.keyboard.type("bedsjson");
  12 |   const rows = page.locator(".finder-row");
  13 |   await expect(rows.first()).toContainText("data/beds.json");
  14 |   await expect(page.locator("#file-finder")).toContainText(/of \d+ files/);
  15 | 
  16 |   await page.keyboard.press("Enter");
  17 |   await expect(page.locator("#file-header")).toContainText("beds.json");
  18 | });
  19 | 
  20 | test("file finder arrows advance exactly one row per press", async ({ page }) => {
  21 |   await page.goto("/");
  22 |   await page.keyboard.press("t");
  23 |   await expect(page.locator("#file-finder")).toBeVisible();
  24 |   await page.keyboard.type("js");
  25 |   const rows = page.locator(".finder-row");
  26 |   await expect(rows.first()).toBeVisible();
  27 |   // Let the debounced re-filter finish before measuring selection deltas.
  28 |   await page.waitForTimeout(400);
  29 |   // Arrow handling lives in the document-level keydown handler only when
  30 |   // the finder input holds focus; under suite load autofocus can lag.
  31 |   await expect(page.locator("#file-finder-input")).toBeFocused();
  32 | 
  33 |   // The invariant from the review fix is the per-press DELTA, not the
  34 |   // absolute starting index (async row refresh may move the start).
  35 |   const selectedIndexOnce = async () => {
  36 |     const names = await rows.evaluateAll((els) => els.map((el) => el.textContent));
  37 |     const sel = page.locator(".finder-row.kbd-selected");
  38 |     const cur = (await sel.count()) ? await sel.first().textContent() : null;
  39 |     return names.findIndex((n) => n === cur);
  40 |   };
  41 |   const selectedIndex = async (expected: number) =>
  42 |     expect.poll(selectedIndexOnce).toBe(expected);
  43 | 
  44 |   await selectedIndex(0);
  45 |   await page.keyboard.press("ArrowDown");
  46 |   await selectedIndex(1);
  47 |   await page.keyboard.press("ArrowUp");
  48 |   await selectedIndex(0);
  49 | 
  50 |   await page.keyboard.press("Escape");
  51 |   await expect(page.locator("#file-finder")).toBeHidden();
  52 | });
  53 | 
  54 | test("j/k/Enter walk the history list; ? shows the shortcut help", async ({ page, request }) => {
  55 |   const tip = await mainTip(request);
  56 |   const entries = await log(request, tip, 5);
  57 |   await page.goto("/#/log");
> 58 |   await page.locator("#log-list .log-row").first().waitFor();
     |                                                    ^ Error: locator.waitFor: Target crashed 
  59 | 
  60 |   await page.keyboard.press("j");
  61 |   const sel = page.locator("#log-list .log-row.kbd-selected");
  62 |   await expect(sel).toHaveCount(1);
  63 |   await expect(sel).toHaveAttribute("data-cs", entries[0].id);
  64 |   await page.keyboard.press("j");
  65 |   await expect(sel).toHaveAttribute("data-cs", entries[1].id);
  66 |   await page.keyboard.press("k");
  67 |   await expect(sel).toHaveAttribute("data-cs", entries[0].id);
  68 | 
  69 |   // ? opens the help overlay; Escape closes it.
  70 |   await page.keyboard.press("?");
  71 |   await expect(page.locator("#kbd-help")).toBeVisible();
  72 |   await expect(page.locator("#kbd-help")).toContainText("file finder");
  73 |   await page.keyboard.press("Escape");
  74 |   await expect(page.locator("#kbd-help")).toBeHidden();
  75 | 
  76 |   await page.keyboard.press("Enter");
  77 |   await expect(page).toHaveURL(new RegExp(`#/changeset/${entries[0].id}`));
  78 | });
  79 | 
  80 | test("keys do not fire while typing in inputs", async ({ page }) => {
  81 |   await page.goto("/#/compare");
  82 |   await page.locator("#cmp-a").fill("abc");
  83 |   await page.keyboard.press("j");
  84 |   await page.keyboard.press("t");
  85 |   await expect(page.locator("#file-finder")).toHaveCount(0);
  86 |   await expect(page.locator("#cmp-a")).toHaveValue("abcjt");
  87 | });
  88 | 
  89 | test("permalink button copies the current URL", async ({ context, page, request }) => {
  90 |   await context.grantPermissions(["clipboard-read", "clipboard-write"]);
  91 |   const tip = await mainTip(request);
  92 |   await page.goto(`/#/changeset/${tip}`);
  93 |   await expect(page.locator("#cs-header")).toBeVisible();
  94 |   await page.locator("#cs-header button", { hasText: /permalink|link/i }).first().click();
  95 |   const clip = await page.evaluate(() => navigator.clipboard.readText());
  96 |   expect(clip).toContain(`#/changeset/${tip}`);
  97 | });
  98 | 
```