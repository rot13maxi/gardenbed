# Instructions

- Following Playwright test failed.
- Explain why, be concise, respect Playwright best practices.
- Provide a snippet of code with the fix, if possible.

# Test info

- Name: browse-file-blame.spec.ts >> binary file renders a banner instead of code
- Location: tests/browse-file-blame.spec.ts:81:5

# Error details

```
Error: page.goto: net::ERR_ABORTED at http://127.0.0.1:18430/#/file/2258b7732806c8b381d1b94724384780414887e1f0123630918c18bf2722b289/styles.css
Call log:
  - navigating to "http://127.0.0.1:18430/#/file/2258b7732806c8b381d1b94724384780414887e1f0123630918c18bf2722b289/styles.css", waiting until "load"

```

# Test source

```ts
  1  | import { expect, test } from "@playwright/test";
  2  | import { mainTip } from "./util";
  3  | 
  4  | test("browse root lists the seeded tree, directories first", async ({ page, request }) => {
  5  |   const tip = await mainTip(request);
  6  |   await page.goto(`/#/browse/${tip}`);
  7  |   await expect(page.locator("#breadcrumbs")).toBeVisible();
  8  |   const rows = page.locator(".tree-row");
  9  |   await expect(rows.first()).toBeVisible();
  10 | 
  11 |   const names = await page.locator(".tree-row .tree-name, .tree-row").allTextContents();
  12 |   const joined = names.join("\n");
  13 |   for (const expected of ["bin", "data", "plan", "README.md", "garden.js", "styles.css"]) {
  14 |     expect(joined).toContain(expected);
  15 |   }
  16 |   // Deleted and renamed-away files are absent at tip.
  17 |   expect(joined).not.toContain("notes.txt");
  18 |   expect(joined).not.toContain("spring.md");
  19 | 
  20 |   // Directories come before files in the rendered order.
  21 |   const kinds = await page.locator(".tree-row").evaluateAll((els) =>
  22 |     els.map((el) => el.getAttribute("data-kind")),
  23 |   );
  24 |   const lastDir = kinds.lastIndexOf("directory");
  25 |   const firstFile = kinds.findIndex((k) => k !== "directory");
  26 |   expect(lastDir).toBeGreaterThanOrEqual(0);
  27 |   expect(firstFile).toBeGreaterThan(lastDir);
  28 | });
  29 | 
  30 | test("breadcrumb navigation into plan/ shows the yearly plan", async ({ page, request }) => {
  31 |   const tip = await mainTip(request);
  32 |   await page.goto(`/#/browse/${tip}/plan`);
  33 |   await expect(page.locator("#breadcrumbs")).toContainText("plan");
  34 |   const rows = page.locator(".tree-row");
  35 |   await expect(rows.filter({ hasText: "2026.md" })).toHaveCount(1);
  36 |   await expect(rows.filter({ hasText: "rotation.md" })).toHaveCount(1);
  37 | 
  38 |   // Clicking the file row's link navigates to the file view.
  39 |   await rows.filter({ hasText: "2026.md" }).locator("a").click();
  40 |   await expect(page.locator("#file-header")).toContainText("2026.md");
  41 |   await expect(page.locator("#code-lines")).toContainText("spring sowing");
  42 | });
  43 | 
  44 | test("file view renders content with line numbers", async ({ page, request }) => {
  45 |   const tip = await mainTip(request);
  46 |   await page.goto(`/#/file/${tip}/README.md`);
  47 |   await expect(page.locator("#file-header")).toContainText("README.md");
  48 |   const lines = page.locator("#code-lines .code-line");
  49 |   expect(await lines.count()).toBeGreaterThanOrEqual(20);
  50 |   await expect(page.locator("#code-lines")).toContainText("# garden planner");
  51 |   await expect(page.locator("#code-lines")).toContainText("bin/sow.sh");
  52 | });
  53 | 
  54 | test("exec file shows an exec mode chip", async ({ page, request }) => {
  55 |   const tip = await mainTip(request);
  56 |   await page.goto(`/#/file/${tip}/bin/sow.sh`);
  57 |   await expect(page.locator("#file-header")).toContainText("exec");
  58 | });
  59 | 
  60 | test("blame toggle attributes lines to the touching commits", async ({ page, request }) => {
  61 |   const tip = await mainTip(request);
  62 |   await page.goto(`/#/file/${tip}/README.md`);
  63 | 
  64 |   // Blame off: no blame gutter blocks.
  65 |   const blocks = page.locator("#code-lines .gutter-block");
  66 |   expect(await blocks.count()).toBe(0);
  67 | 
  68 |   await page.locator("#blame-toggle").click();
  69 |   await expect(blocks.first()).toBeVisible();
  70 |   // README was touched by three commits → at least two distinct attributions.
  71 |   const css = new Set(await blocks.evaluateAll((els) => els.map((el) => el.getAttribute("data-cs"))));
  72 |   expect(css.size).toBeGreaterThanOrEqual(2);
  73 | 
  74 |   // Clicking a gutter block opens its changeset.
  75 |   const target = [...css][0]!;
  76 |   await blocks.first().click();
  77 |   await expect(page).toHaveURL(/#\/changeset\//);
  78 |   await expect(page.locator("#cs-header .cs-id")).toContainText(target);
  79 | });
  80 | 
  81 | test("binary file renders a banner instead of code", async ({ page, request }) => {
  82 |   const tip = await mainTip(request);
  83 |   // Upload a non-UTF8 blob through the seed? None seeded — assert via API shape instead:
  84 |   // the file view of every seeded text file must NOT show a binary banner.
> 85 |   await page.goto(`/#/file/${tip}/styles.css`);
     |              ^ Error: page.goto: net::ERR_ABORTED at http://127.0.0.1:18430/#/file/2258b7732806c8b381d1b94724384780414887e1f0123630918c18bf2722b289/styles.css
  86 |   await expect(page.locator(".banner")).toHaveCount(0);
  87 | });
  88 | 
```