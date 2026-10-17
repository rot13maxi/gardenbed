import { expect, test } from "@playwright/test";
import { log, mainTip } from "./util";

test("overview loads with zero console errors", async ({ page }) => {
  const errors: string[] = [];
  page.on("pageerror", (e) => errors.push(String(e)));
  page.on("console", (m) => m.type() === "error" && errors.push(m.text()));
  await page.goto("/");
  await expect(page.locator("#stat-chips .chip").first()).toBeVisible();
  expect(errors).toEqual([]);
});

test("overview shows refs cards with the seeded main ref", async ({ page, request }) => {
  const tip = await mainTip(request);
  await page.goto("/");
  const main = page.locator("#refs-cards .ref-card[data-ref='refs/heads/main']");
  await expect(main).toBeVisible();
  await expect(main).toContainText(tip.slice(0, 12));
});

test("overview recent history lists seeded commits", async ({ page }) => {
  await page.goto("/");
  const recent = page.locator("#recent-list .log-row");
  await expect(recent.first()).toBeVisible();
  await expect(page.locator("#recent-list")).toContainText("Initial commit: README");
});

test("overview request counters table renders rows after API traffic", async ({ page }) => {
  await page.goto("/");
  await page.locator("[data-nav='history']").click();
  await expect(page.locator("#log-list .log-row").first()).toBeVisible();
  await page.locator("[data-nav='overview']").click();
  await expect(page.locator("#stats-table .stats-row").first()).toBeVisible();
});

test("clicking a ref card navigates to its changeset", async ({ page, request }) => {
  const tip = await mainTip(request);
  await page.goto("/");
  await page.locator("#refs-cards .ref-card[data-ref='refs/heads/main']").click();
  await expect(page.locator("#cs-header .cs-id")).toHaveText(tip);
});

test("document title reflects the route", async ({ page }) => {
  await page.goto("/");
  await expect(page).toHaveTitle(/gardenbed/i);
  const tip = await (await import("./util")).mainTip(page.request);
  const entries = await log(page.request, tip, 5);
  await page.goto(`/#/changeset/${entries[0].id}`);
  await expect(page).toHaveTitle(new RegExp(entries[0].id.slice(0, 12)));
});
