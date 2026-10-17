# Instructions

- Following Playwright test failed.
- Explain why, be concise, respect Playwright best practices.
- Provide a snippet of code with the fix, if possible.

# Test info

- Name: e2e/tests/overview.spec.ts >> overview recent history lists seeded commits
- Location: e2e/tests/overview.spec.ts:21:5

# Error details

```
Error: page.goto: Protocol error (Page.navigate): Cannot navigate to invalid URL
Call log:
  - navigating to "/", waiting until "load"

```

# Test source

```ts
  1  | import { expect, test } from "@playwright/test";
  2  | import { log, mainTip } from "./util";
  3  | 
  4  | test("overview loads with zero console errors", async ({ page }) => {
  5  |   const errors: string[] = [];
  6  |   page.on("pageerror", (e) => errors.push(String(e)));
  7  |   page.on("console", (m) => m.type() === "error" && errors.push(m.text()));
  8  |   await page.goto("/");
  9  |   await expect(page.locator("#stat-chips .chip").first()).toBeVisible();
  10 |   expect(errors).toEqual([]);
  11 | });
  12 | 
  13 | test("overview shows refs cards with the seeded main ref", async ({ page, request }) => {
  14 |   const tip = await mainTip(request);
  15 |   await page.goto("/");
  16 |   const main = page.locator("#refs-cards .ref-card[data-ref='refs/heads/main']");
  17 |   await expect(main).toBeVisible();
  18 |   await expect(main).toContainText(tip.slice(0, 12));
  19 | });
  20 | 
  21 | test("overview recent history lists seeded commits", async ({ page }) => {
> 22 |   await page.goto("/");
     |              ^ Error: page.goto: Protocol error (Page.navigate): Cannot navigate to invalid URL
  23 |   const recent = page.locator("#recent-list .log-row");
  24 |   await expect(recent.first()).toBeVisible();
  25 |   await expect(page.locator("#recent-list")).toContainText("Initial commit: README");
  26 | });
  27 | 
  28 | test("overview request counters table renders rows after API traffic", async ({ page }) => {
  29 |   await page.goto("/");
  30 |   await page.locator("[data-nav='history']").click();
  31 |   await expect(page.locator("#log-list .log-row").first()).toBeVisible();
  32 |   await page.locator("[data-nav='overview']").click();
  33 |   await expect(page.locator("#stats-table .stats-row").first()).toBeVisible();
  34 | });
  35 | 
  36 | test("clicking a ref card navigates to its changeset", async ({ page, request }) => {
  37 |   const tip = await mainTip(request);
  38 |   await page.goto("/");
  39 |   await page.locator("#refs-cards .ref-card[data-ref='refs/heads/main']").click();
  40 |   await expect(page.locator("#cs-header .cs-id")).toHaveText(tip);
  41 | });
  42 | 
  43 | test("document title reflects the route", async ({ page }) => {
  44 |   await page.goto("/");
  45 |   await expect(page).toHaveTitle(/gardenbed/i);
  46 |   const tip = await mainTip(page.request);
  47 |   const entries = await log(page.request, tip, 5);
  48 |   await page.goto(`/#/changeset/${entries[0].id}`);
  49 |   await expect(page).toHaveTitle(new RegExp(entries[0].id.slice(0, 12)));
  50 | });
  51 | 
```