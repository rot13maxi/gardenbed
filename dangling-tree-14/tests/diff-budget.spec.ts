import { expect, test } from "@playwright/test";

test("wholesale rewrite of a 6000-line file renders a budget note, not 12k rows", async ({
  page,
  request,
}) => {
  const r = await request.get("/api/activity?limit=200");
  const stress = (await r.json()).changesets.find((c: { message: string }) =>
    c.message.startsWith("Stress fixture"),
  );
  expect(stress).toBeTruthy();

  await page.goto(`/#/changeset/${stress.id}`);
  const row = page.locator(".change-row", { hasText: "garden.js" });
  await expect(row).toBeVisible();
  // The diff cell shows the budget note and stays empty of line rows —
  // the regression this guards built ~32000 hidden DOM nodes.
  await expect(page.locator(".diff-note").first()).toContainText(/too many lines to diff/);
  expect(await page.locator(".diff-line").count()).toBe(0);

  // Page still interactive: expander click does not build the monster DOM.
  await row.locator(".diff-expander").click();
  expect(await page.locator(".diff-line").count()).toBe(0);
  await expect(page.locator("#cs-header .cs-id")).toHaveText(stress.id);
});

test("budget keeps normal diffs working beside the stress row", async ({ page, request }) => {
  const r = await request.get("/api/activity?limit=200");
  const stress = (await r.json()).changesets.find((c: { message: string }) =>
    c.message.startsWith("Stress fixture"),
  );
  await page.goto(`/#/changeset/${stress.parents[0]}`);
  // The parent changeset (Document the sow script) has a small README
  // modify — it still diffs normally.
  await expect(page.locator(".diff-line").first()).toBeVisible();
});

test("failed diff fetch leaves the block retryable", async ({ page, request }) => {
  const r0 = await request.get("/api/log?cs=main&limit=20");
  const tune = (await r0.json()).entries.find((e: { message: string }) =>
    e.message.startsWith("Tune grid spacing"),
  );

  // First visit: every file fetch 500s → inline error notes, no rows.
  await page.route("**/api/file*", (route) =>
    route.fulfill({ status: 500, contentType: "application/json", body: '{"error":"boom"}' }),
  );
  await page.goto(`/#/changeset/${tune.id}`);
  await expect(
    page.locator(".diff-note", { hasText: "could not load diff" }).first(),
  ).toBeVisible();
  await page.unroute("**/api/file*");

  // One click retries the fetch (job.loaded stayed false on failure) and
  // the recovered block opens with real rows.
  await page.locator(".diff-expander").first().click();
  await expect(page.locator(".diff-line").first()).toBeVisible();
});
