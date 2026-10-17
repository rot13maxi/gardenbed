# Instructions

- Following Playwright test failed.
- Explain why, be concise, respect Playwright best practices.
- Provide a snippet of code with the fix, if possible.

# Test info

- Name: diffs.spec.ts >> collapse and re-expand a diff block
- Location: tests/diffs.spec.ts:65:5

# Error details

```
Test timeout of 30000ms exceeded.
```

```
Error: locator.click: Target crashed 
Call log:
  - waiting for locator('.diff-expander').first()
    - locator resolved to <button type="button" aria-expanded="true" class="diff-expander" aria-label="show diff">▾</button>
  - attempting click action
    2 × waiting for element to be visible, enabled and stable
      - element is not stable
    - retrying click action
    - waiting 20ms
    - waiting for element to be visible, enabled and stable
    - element is not stable
  - retrying click action
    - waiting 100ms
    - waiting for element to be visible, enabled and stable
    - element is visible, enabled and stable
    - scrolling into view if needed
    - done scrolling

```

# Test source

```ts
  1   | import { APIRequestContext, expect, test } from "@playwright/test";
  2   | import { findByMessage, log, mainTip } from "./util";
  3   | 
  4   | async function fileAt(
  5   |   request: APIRequestContext,
  6   |   cs: string,
  7   |   path: string,
  8   | ): Promise<string> {
  9   |   const r = await request.get(
  10  |     `/api/file?cs=${encodeURIComponent(cs)}&path=${encodeURIComponent(path)}`,
  11  |   );
  12  |   expect(r.ok()).toBeTruthy();
  13  |   const j = await r.json();
  14  |   return j.content ?? "";
  15  | }
  16  | 
  17  | 
  18  | 
  19  | test("modify rows auto-expand into unified diffs with real +/- lines", async ({ page, request }) => {
  20  |   const tip = await mainTip(request);
  21  |   const tune = findByMessage(await log(request, tip), "Tune grid spacing");
  22  |   const parent = tune.parents[0];
  23  |   const oldText = await fileAt(request, parent, "garden.js");
  24  |   const newText = await fileAt(request, tune.id, "garden.js");
  25  | 
  26  |   await page.goto(`/#/changeset/${tune.id}`);
  27  |   const row = page.locator(".change-row", { hasText: "garden.js" });
  28  |   const expander = row.locator(".diff-expander");
  29  |   // Small diff → auto-expanded.
  30  |   await expect(expander).toHaveAttribute("aria-expanded", "true");
  31  |   const block = page.locator(".diff-block").filter({ hasText: /./ }).first();
  32  |   // Diff bodies load after the expander flips: wait for lines before
  33  |   // collecting (allTextContents is a point-in-time read).
  34  |   await expect(block.locator(".diff-line").first()).toBeVisible();
  35  | 
  36  |   // This edit is purely additive: adds present, no dels.
  37  |   const adds = await block.locator(".diff-line.diff-add .diff-text").allTextContents();
  38  |   const dels = await block.locator(".diff-line.diff-del .diff-text").allTextContents();
  39  |   expect(adds.length).toBeGreaterThan(0);
  40  |   expect(dels.length).toBe(0);
  41  |   for (const a of adds) expect(newText).toContain(a);
  42  |   await expect(block.locator(".diff-line.diff-add .diff-sign").first()).toHaveText("+");
  43  |   expect(oldText).toContain("renderBedGrid"); // baseline: both sides fetched
  44  | });
  45  | 
  46  | test("deleted files render all-minus diffs against the empty new side", async ({ page, request }) => {
  47  |   const tip = await mainTip(request);
  48  |   const prune = findByMessage(await log(request, tip), "Prune outdated notes");
  49  |   const oldText = await fileAt(request, prune.parents[0], "notes.txt");
  50  |   expect(oldText).toContain("scratch notes");
  51  | 
  52  |   await page.goto(`/#/changeset/${prune.id}`);
  53  |   // Auto-expanded small diff: only delete lines, sign '-', text from the
  54  |   // pre-delete content.
  55  |   const row = page.locator(".change-row", { hasText: "notes.txt" });
  56  |   await expect(row.locator(".diff-expander")).toHaveAttribute("aria-expanded", "true");
  57  |   const block = page.locator(".diff-block").first();
  58  |   await expect(block.locator(".diff-line.diff-del").first()).toBeVisible();
  59  |   expect(await block.locator(".diff-line.diff-add").count()).toBe(0);
  60  |   await expect(block.locator(".diff-line.diff-del .diff-sign").first()).toHaveText("-");
  61  |   const dels = await block.locator(".diff-line.diff-del .diff-text").allTextContents();
  62  |   for (const d of dels) expect(oldText).toContain(d);
  63  | });
  64  | 
  65  | test("collapse and re-expand a diff block", async ({ page, request }) => {
  66  |   const tip = await mainTip(request);
  67  |   const tune = findByMessage(await log(request, tip), "Tune grid spacing");
  68  |   await page.goto(`/#/changeset/${tune.id}`);
  69  |   const expander = page.locator(".diff-expander").first();
  70  |   await expect(expander).toHaveAttribute("aria-expanded", "true");
> 71  |   await expander.click();
      |                  ^ Error: locator.click: Target crashed 
  72  |   await expect(expander).toHaveAttribute("aria-expanded", "false");
  73  |   await expect(page.locator(".diff-block").first()).toBeHidden();
  74  |   await expander.click();
  75  |   await expect(expander).toHaveAttribute("aria-expanded", "true");
  76  | });
  77  | 
  78  | test("pure rename shows a content-unchanged note instead of a diff", async ({ page, request }) => {
  79  |   const note = "content unchanged";
  80  |   const rename = findByMessage(await log(request, await mainTip(request)), "Rename spring plan");
  81  |   await page.goto(`/#/changeset/${rename.id}`);
  82  |   await expect(page.locator(".diff-note")).toContainText(note);
  83  | });
  84  | 
  85  | test("added file diffs are all-plus against an empty old side", async ({ page, request }) => {
  86  |   const tip = await mainTip(request);
  87  |   const grid = findByMessage(await log(request, tip), "Build interactive bed grid");
  88  |   await page.goto(`/#/changeset/${grid.id}`);
  89  |   const block = page.locator(".diff-block").first();
  90  |   await expect(block.locator(".diff-line.diff-add").first()).toBeVisible();
  91  |   expect(await block.locator(".diff-line.diff-del").count()).toBe(0);
  92  |   expect(await block.locator(".diff-line.diff-add").count()).toBeGreaterThan(30);
  93  | });
  94  | 
  95  | test("compare page renders expandable diffs between arbitrary revs", async ({ page, request }) => {
  96  |   const tip = await mainTip(request);
  97  |   const entries = await log(request, tip);
  98  |   const root = entries[entries.length - 1];
  99  |   await page.goto(`/#/compare?a=${root.id}&b=main`);
  100 |   await page.locator("#cmp-run").click();
  101 |   await expect(page.locator("#cmp-results .change-row").first()).toBeVisible();
  102 |   // Root → tip adds many files; the README add row is expanded with plus lines.
  103 |   const readme = page.locator("#cmp-results .change-row", { hasText: "README.md" });
  104 |   await expect(readme).toBeVisible();
  105 |   const blocks = page.locator("#cmp-results .diff-block");
  106 |   await expect(blocks.first().locator(".diff-line").first()).toBeVisible();
  107 | });
  108 | 
```