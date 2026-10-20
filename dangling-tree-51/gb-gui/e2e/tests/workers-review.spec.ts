import { APIRequestContext, expect, test } from "@playwright/test";
import { mainTip } from "./util";

interface ActivityEntry {
  id: string;
  parents: string[];
  message: string;
  author: { name: string };
}

async function activity(request: APIRequestContext): Promise<ActivityEntry[]> {
  const r = await request.get("/api/activity?limit=200");
  expect(r.ok()).toBeTruthy();
  return (await r.json()).changesets;
}

test.beforeEach(async ({ page }) => {
  // Watchlist is localStorage state — isolate per test.
  await page.goto("/");
  await page.evaluate(() => localStorage.clear());
});

test("workers panel shows the seeded unpublished authors", async ({ page, request }) => {
  const drafts = (await activity(request)).filter((e) =>
    ["Pike Rowan", "Moss Hale"].includes(e.author.name),
  );
  expect(drafts.length).toBeGreaterThanOrEqual(4); // 3 drafts + 1 rebase twin

  await page.goto("/");
  for (const author of ["Pike Rowan", "Moss Hale"]) {
    const card = page.locator(`#workers-panel .worker-card[data-author='${author}']`);
    await expect(card).toBeVisible();
    await expect(card.locator(".badge-draft").first()).toBeVisible();
  }
});

test("rebase pair surfaces as an event row", async ({ page, request }) => {
  const all = await activity(request);
  const pair = all.filter((e) => e.message.startsWith("Add evening mulch reminder"));
  expect(pair.length).toBe(2);

  await page.goto("/");
  const row = page.locator("#events-panel .event-row[data-kind='rebase']");
  await expect(row).toHaveCount(1);
  await expect(row).toContainText("Pike Rowan");
  await expect(row).toContainText("Add evening mulch reminder");
});

test("watched changeset appears and flips to landed when main moves over it", async ({
  page,
  request,
}) => {
  const drafts = (await activity(request)).filter((e) =>
    e.author.name === "Pike Rowan" && e.message.startsWith("Draft: drip irrigation"),
  );
  expect(drafts.length).toBe(1);
  const draft = drafts[0];

  await page.goto("/");
  await page.locator("#watch-add").fill(draft.id);
  await page.locator("#watch-add").press("Enter");
  const row = page.locator("#watch-list .watch-row", { hasText: "drip irrigation" });
  await expect(row).toBeVisible();
  await expect(row.locator(".badge-draft")).toBeVisible();

  // Simulate main fast-forwarding over the watched id.
  await page.route("**/api/refs", async (route) => {
    const real = await route.fetch();
    const json = await real.json();
    json.refs = json.refs.map((r: { name: string; cs: string }) =>
      r.name === "refs/heads/main" ? { ...r, cs: draft.id } : r,
    );
    await route.fulfill({ response: real, json });
  });

  // 10s poll cadence + loadavg-200 shared box ⇒ generous window.
  await expect(row.locator(".badge-published")).toBeVisible({ timeout: 60_000 });
  const landed = page.locator("#events-panel .event-row[data-kind='landed']");
  await expect(landed.first()).toContainText(draft.id.slice(0, 8));
});

test("unknown changeset ids show a missing row, not an error page", async ({ page }) => {
  await page.goto("/");
  await page.locator("#watch-add").fill("deadbeefdeadbeef");
  await page.locator("#watch-add").press("Enter");
  await expect(page.locator("#watch-list .watch-row.missing")).toBeVisible();
});

test("review page renders diff, parents, merge chip, and land commands", async ({ page, request }) => {
  await mainTip(request);
  const r = await request.get("/api/activity?limit=200");
  const all = (await r.json()).changesets as ActivityEntry[];
  const merge = all.find((e) => e.parents.length === 2)!;
  expect(merge.message).toContain("Merge watering");

  await page.goto(`/#/review/${merge.id}`);
  await expect(page.locator("#review-header")).toContainText("Merge watering");
  // The merge is reachable from main → published badge.
  await expect(page.locator("#review-header .badge-published")).toBeVisible();
  // Both parents linked.
  for (const p of merge.parents) {
    await expect(page.locator(`#review-header a[href*='${p}']`).first()).toBeAttached();
  }
  // Diff rows present and expandable.
  await expect(page.locator(".change-row .diff-expander").first()).toBeVisible();
  // Read-only land card with bed commands.
  await expect(page.locator(".land-commands")).toContainText("bed rebase main && bed publish");
  await expect(page.locator("#readonly-note")).toBeVisible();
});

test("review page marks seeded drafts as unpublished", async ({ page, request }) => {
  const draft = (await activity(request)).find(
    (e) => e.message.startsWith("draft: calmer empty-bed colors"),
  )!;
  await page.goto(`/#/review/${draft.id}`);
  await expect(page.locator("#review-header .badge-draft")).toBeVisible();
});
