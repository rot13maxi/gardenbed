import { expect, test } from "@playwright/test";
import { log, mainTip } from "./util";

test("file finder opens on 't', fuzzy-filters, and opens the top hit", async ({ page, request }) => {
  await mainTip(request);
  await page.goto("/");
  await page.keyboard.press("t");
  await expect(page.locator("#file-finder")).toBeVisible();
  await expect(page.locator("#file-finder-input")).toBeFocused();

  await page.keyboard.type("bedsjson");
  const rows = page.locator(".finder-row");
  await expect(rows.first()).toContainText("data/beds.json");
  await expect(page.locator("#file-finder")).toContainText(/of \d+ files/);

  await page.keyboard.press("Enter");
  await expect(page.locator("#file-header")).toContainText("beds.json");
});

test("file finder arrows advance exactly one row per press", async ({ page }) => {
  await page.goto("/");
  await page.keyboard.press("t");
  await expect(page.locator("#file-finder")).toBeVisible();
  await page.keyboard.type("js");
  const rows = page.locator(".finder-row");
  await expect(rows.first()).toBeVisible();
  // Let the debounced re-filter finish before measuring selection deltas.
  await page.waitForTimeout(400);
  // Arrow handling lives in the document-level keydown handler only when
  // the finder input holds focus; under suite load autofocus can lag.
  await expect(page.locator("#file-finder-input")).toBeFocused();

  // The invariant from the review fix is the per-press DELTA, not the
  // absolute starting index (async row refresh may move the start).
  const selectedIndexOnce = async () => {
    const names = await rows.evaluateAll((els) => els.map((el) => el.textContent));
    const sel = page.locator(".finder-row.kbd-selected");
    const cur = (await sel.count()) ? await sel.first().textContent() : null;
    return names.findIndex((n) => n === cur);
  };
  const selectedIndex = async (expected: number) =>
    expect.poll(selectedIndexOnce).toBe(expected);

  await selectedIndex(0);
  await page.keyboard.press("ArrowDown");
  await selectedIndex(1);
  await page.keyboard.press("ArrowUp");
  await selectedIndex(0);

  await page.keyboard.press("Escape");
  await expect(page.locator("#file-finder")).toBeHidden();
});

test("j/k/Enter walk the history list; ? shows the shortcut help", async ({ page, request }) => {
  const tip = await mainTip(request);
  const entries = await log(request, tip, 5);
  await page.goto("/#/log");
  await page.locator("#log-list .log-row").first().waitFor();

  await page.keyboard.press("j");
  const sel = page.locator("#log-list .log-row.kbd-selected");
  await expect(sel).toHaveCount(1);
  await expect(sel).toHaveAttribute("data-cs", entries[0].id);
  await page.keyboard.press("j");
  await expect(sel).toHaveAttribute("data-cs", entries[1].id);
  await page.keyboard.press("k");
  await expect(sel).toHaveAttribute("data-cs", entries[0].id);

  // ? opens the help overlay; Escape closes it.
  await page.keyboard.press("?");
  await expect(page.locator("#kbd-help")).toBeVisible();
  await expect(page.locator("#kbd-help")).toContainText("file finder");
  await page.keyboard.press("Escape");
  await expect(page.locator("#kbd-help")).toBeHidden();

  await page.keyboard.press("Enter");
  await expect(page).toHaveURL(new RegExp(`#/changeset/${entries[0].id}`));
});

test("keys do not fire while typing in inputs", async ({ page }) => {
  await page.goto("/#/compare");
  await page.locator("#cmp-a").fill("abc");
  await page.keyboard.press("j");
  await page.keyboard.press("t");
  await expect(page.locator("#file-finder")).toHaveCount(0);
  await expect(page.locator("#cmp-a")).toHaveValue("abcjt");
});

test("permalink button copies the current URL", async ({ context, page, request }) => {
  await context.grantPermissions(["clipboard-read", "clipboard-write"]);
  const tip = await mainTip(request);
  await page.goto(`/#/changeset/${tip}`);
  await expect(page.locator("#cs-header")).toBeVisible();
  await page.locator("#cs-header button", { hasText: /permalink|link/i }).first().click();
  const clip = await page.evaluate(() => navigator.clipboard.readText());
  expect(clip).toContain(`#/changeset/${tip}`);
});
