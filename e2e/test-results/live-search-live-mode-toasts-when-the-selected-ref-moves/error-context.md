# Instructions

- Following Playwright test failed.
- Explain why, be concise, respect Playwright best practices.
- Provide a snippet of code with the fix, if possible.

# Test info

- Name: live-search.spec.ts >> live mode toasts when the selected ref moves
- Location: tests/live-search.spec.ts:4:5

# Error details

```
Test timeout of 30000ms exceeded.
```

```
Test timeout of 30000ms exceeded.
```

```
Error: page.goto: Page crashed
Call log:
  - navigating to "http://127.0.0.1:18430/", waiting until "load"

```

# Test source

```ts
  1  | import { expect, test } from "@playwright/test";
  2  | import { findByMessage, log, mainTip } from "./util";
  3  | 
  4  | test("live mode toasts when the selected ref moves", async ({ page, request }) => {
  5  |   const tip = await mainTip(request);
> 6  |   await page.goto("/");
     |              ^ Error: page.goto: Page crashed
  7  |   await expect(page.locator("#stat-chips .chip").first()).toBeVisible();
  8  |   await expect(page.locator("#live-toast")).toHaveCount(0);
  9  | 
  10 |   // Simulate the ref moving: every subsequent /api/refs poll answers with
  11 |   // a fabricated tip. The log fetch for the fake tip 404s — toast must
  12 |   // still show the bare ids.
  13 |   const fake = "ab".repeat(32);
  14 |   await page.route("**/api/refs", async (route) => {
  15 |     const real = await route.fetch();
  16 |     const json = await real.json();
  17 |     json.refs = json.refs.map((r: { name: string; cs: string }) =>
  18 |       r.name === "refs/heads/main" ? { ...r, cs: fake } : r,
  19 |     );
  20 |     await route.fulfill({ response: real, json });
  21 |   });
  22 |   // 10s poll cadence + loadavg-200 shared box ⇒ generous window.
  23 |   await expect(page.locator("#live-toast")).toContainText(tip.slice(0, 8), { timeout: 60_000 });
  24 |   await expect(page.locator("#live-toast")).toContainText(fake.slice(0, 8));
  25 | 
  26 |   // Dismissable.
  27 |   await page.locator("#live-toast .toast-close").click();
  28 |   await expect(page.locator("#live-toast")).toBeHidden();
  29 | });
  30 | 
  31 | test("blame age heat shades old and new attributions differently", async ({ page, request }) => {
  32 |   const tip = await mainTip(request);
  33 |   await page.goto(`/#/file/${tip}/README.md`);
  34 |   await page.locator("#blame-toggle").click();
  35 |   const blocks = page.locator("#code-lines .gutter-block");
  36 |   await blocks.first().waitFor();
  37 | 
  38 |   // Group hues by attributed changeset. README has ≥2 attributions with
  39 |   // distinct ages; distinct ages must get distinct colors.
  40 |   const byCs = new Map<string, string>();
  41 |   for (const b of await blocks.all()) {
  42 |     const cs = await b.getAttribute("data-cs");
  43 |     const style = await b.getAttribute("style");
  44 |     if (cs && style) byCs.set(cs, style);
  45 |   }
  46 |   expect(byCs.size).toBeGreaterThanOrEqual(2);
  47 |   expect(new Set(byCs.values()).size).toBe(byCs.size);
  48 | });
  49 | 
  50 | test("global search finds seeded content and navigates", async ({ page }) => {
  51 |   await page.goto("/");
  52 |   const box = page.locator("#global-search");
  53 |   await box.fill("renderBedGrid");
  54 |   await box.press("Enter");
  55 |   await expect(page).toHaveURL(/#\/search\?/);
  56 |   await expect(page.locator("#search-results .search-file", { hasText: "garden.js" }).first()).toBeVisible();
  57 |   await expect(page.locator("#search-results mark").first()).toHaveText(/renderbedgrid/i);
  58 |   await page.locator("#search-results .search-file a", { hasText: "garden.js" }).first().click();
  59 |   await expect(page.locator("#file-header")).toContainText("garden.js");
  60 | });
  61 | 
```