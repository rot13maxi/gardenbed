# Instructions

- Following Playwright test failed.
- Explain why, be concise, respect Playwright best practices.
- Provide a snippet of code with the fix, if possible.

# Test info

- Name: workers-review.spec.ts >> unknown changeset ids show a missing row, not an error page
- Location: tests/workers-review.spec.ts:82:5

# Error details

```
Test timeout of 30000ms exceeded while running "beforeEach" hook.
```

# Test source

```ts
  1   | import { APIRequestContext, expect, test } from "@playwright/test";
  2   | import { mainTip } from "./util";
  3   | 
  4   | interface ActivityEntry {
  5   |   id: string;
  6   |   parents: string[];
  7   |   message: string;
  8   |   author: { name: string };
  9   | }
  10  | 
  11  | async function activity(request: APIRequestContext): Promise<ActivityEntry[]> {
  12  |   const r = await request.get("/api/activity?limit=200");
  13  |   expect(r.ok()).toBeTruthy();
  14  |   return (await r.json()).changesets;
  15  | }
  16  | 
> 17  | test.beforeEach(async ({ page }) => {
      |      ^ Test timeout of 30000ms exceeded while running "beforeEach" hook.
  18  |   // Watchlist is localStorage state — isolate per test.
  19  |   await page.goto("/");
  20  |   await page.evaluate(() => localStorage.clear());
  21  | });
  22  | 
  23  | test("workers panel shows the seeded unpublished authors", async ({ page, request }) => {
  24  |   const drafts = (await activity(request)).filter((e) =>
  25  |     ["Pike Rowan", "Moss Hale"].includes(e.author.name),
  26  |   );
  27  |   expect(drafts.length).toBeGreaterThanOrEqual(4); // 3 drafts + 1 rebase twin
  28  | 
  29  |   await page.goto("/");
  30  |   for (const author of ["Pike Rowan", "Moss Hale"]) {
  31  |     const card = page.locator(`#workers-panel .worker-card[data-author='${author}']`);
  32  |     await expect(card).toBeVisible();
  33  |     await expect(card.locator(".badge-draft").first()).toBeVisible();
  34  |   }
  35  | });
  36  | 
  37  | test("rebase pair surfaces as an event row", async ({ page, request }) => {
  38  |   const all = await activity(request);
  39  |   const pair = all.filter((e) => e.message.startsWith("Add evening mulch reminder"));
  40  |   expect(pair.length).toBe(2);
  41  | 
  42  |   await page.goto("/");
  43  |   const row = page.locator("#events-panel .event-row[data-kind='rebase']");
  44  |   await expect(row).toHaveCount(1);
  45  |   await expect(row).toContainText("Pike Rowan");
  46  |   await expect(row).toContainText("Add evening mulch reminder");
  47  | });
  48  | 
  49  | test("watched changeset appears and flips to landed when main moves over it", async ({
  50  |   page,
  51  |   request,
  52  | }) => {
  53  |   const drafts = (await activity(request)).filter((e) =>
  54  |     e.author.name === "Pike Rowan" && e.message.startsWith("Draft: drip irrigation"),
  55  |   );
  56  |   expect(drafts.length).toBe(1);
  57  |   const draft = drafts[0];
  58  | 
  59  |   await page.goto("/");
  60  |   await page.locator("#watch-add").fill(draft.id);
  61  |   await page.locator("#watch-add").press("Enter");
  62  |   const row = page.locator("#watch-list .watch-row", { hasText: "drip irrigation" });
  63  |   await expect(row).toBeVisible();
  64  |   await expect(row.locator(".badge-draft")).toBeVisible();
  65  | 
  66  |   // Simulate main fast-forwarding over the watched id.
  67  |   await page.route("**/api/refs", async (route) => {
  68  |     const real = await route.fetch();
  69  |     const json = await real.json();
  70  |     json.refs = json.refs.map((r: { name: string; cs: string }) =>
  71  |       r.name === "refs/heads/main" ? { ...r, cs: draft.id } : r,
  72  |     );
  73  |     await route.fulfill({ response: real, json });
  74  |   });
  75  | 
  76  |   // 10s poll cadence + loadavg-200 shared box ⇒ generous window.
  77  |   await expect(row.locator(".badge-published")).toBeVisible({ timeout: 60_000 });
  78  |   const landed = page.locator("#events-panel .event-row[data-kind='landed']");
  79  |   await expect(landed.first()).toContainText(draft.id.slice(0, 8));
  80  | });
  81  | 
  82  | test("unknown changeset ids show a missing row, not an error page", async ({ page }) => {
  83  |   await page.goto("/");
  84  |   await page.locator("#watch-add").fill("deadbeefdeadbeef");
  85  |   await page.locator("#watch-add").press("Enter");
  86  |   await expect(page.locator("#watch-list .watch-row.missing")).toBeVisible();
  87  | });
  88  | 
  89  | test("review page renders diff, parents, merge chip, and land commands", async ({ page, request }) => {
  90  |   await mainTip(request);
  91  |   const r = await request.get("/api/activity?limit=200");
  92  |   const all = (await r.json()).changesets as ActivityEntry[];
  93  |   const merge = all.find((e) => e.parents.length === 2)!;
  94  |   expect(merge.message).toContain("Merge watering");
  95  | 
  96  |   await page.goto(`/#/review/${merge.id}`);
  97  |   await expect(page.locator("#review-header")).toContainText("Merge watering");
  98  |   // The merge is reachable from main → published badge.
  99  |   await expect(page.locator("#review-header .badge-published")).toBeVisible();
  100 |   // Both parents linked.
  101 |   for (const p of merge.parents) {
  102 |     await expect(page.locator(`#review-header a[href*='${p}']`).first()).toBeAttached();
  103 |   }
  104 |   // Diff rows present and expandable.
  105 |   await expect(page.locator(".change-row .diff-expander").first()).toBeVisible();
  106 |   // Read-only land card with bed commands.
  107 |   await expect(page.locator(".land-commands")).toContainText("bed rebase main && bed publish");
  108 |   await expect(page.locator("#readonly-note")).toBeVisible();
  109 | });
  110 | 
  111 | test("review page marks seeded drafts as unpublished", async ({ page, request }) => {
  112 |   const draft = (await activity(request)).find(
  113 |     (e) => e.message.startsWith("draft: calmer empty-bed colors"),
  114 |   )!;
  115 |   await page.goto(`/#/review/${draft.id}`);
  116 |   await expect(page.locator("#review-header .badge-draft")).toBeVisible();
  117 | });
```