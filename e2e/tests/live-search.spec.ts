import { expect, test } from "@playwright/test";
import { findByMessage, log, mainTip } from "./util";

test("live mode toasts when the selected ref moves", async ({ page, request }) => {
  const tip = await mainTip(request);
  await page.goto("/");
  await expect(page.locator("#stat-chips .chip").first()).toBeVisible();
  await expect(page.locator("#live-toast")).toHaveCount(0);

  // Simulate the ref moving: every subsequent /api/refs poll answers with
  // a fabricated tip. The log fetch for the fake tip 404s — toast must
  // still show the bare ids.
  const fake = "ab".repeat(32);
  await page.route("**/api/refs", async (route) => {
    const real = await route.fetch();
    const json = await real.json();
    json.refs = json.refs.map((r: { name: string; cs: string }) =>
      r.name === "refs/heads/main" ? { ...r, cs: fake } : r,
    );
    await route.fulfill({ response: real, json });
  });
  await expect(page.locator("#live-toast")).toContainText(tip.slice(0, 8), { timeout: 20_000 });
  await expect(page.locator("#live-toast")).toContainText(fake.slice(0, 8));

  // Dismissable.
  await page.locator("#live-toast .toast-close").click();
  await expect(page.locator("#live-toast")).toBeHidden();
});

test("blame age heat shades old and new attributions differently", async ({ page, request }) => {
  const tip = await mainTip(request);
  await page.goto(`/#/file/${tip}/README.md`);
  await page.locator("#blame-toggle").click();
  const blocks = page.locator("#code-lines .gutter-block");
  await blocks.first().waitFor();

  // Group hues by attributed changeset. README has ≥2 attributions with
  // distinct ages; distinct ages must get distinct colors.
  const byCs = new Map<string, string>();
  for (const b of await blocks.all()) {
    const cs = await b.getAttribute("data-cs");
    const style = await b.getAttribute("style");
    if (cs && style) byCs.set(cs, style);
  }
  expect(byCs.size).toBeGreaterThanOrEqual(2);
  expect(new Set(byCs.values()).size).toBe(byCs.size);
});

test("global search finds seeded content and navigates", async ({ page }) => {
  await page.goto("/");
  const box = page.locator("#global-search");
  await box.fill("renderBedGrid");
  await box.press("Enter");
  await expect(page).toHaveURL(/#\/search\?/);
  await expect(page.locator("#search-results .search-file", { hasText: "garden.js" }).first()).toBeVisible();
  await expect(page.locator("#search-results mark").first()).toHaveText(/renderbedgrid/i);
  await page.locator("#search-results .search-file a", { hasText: "garden.js" }).first().click();
  await expect(page.locator("#file-header")).toContainText("garden.js");
});
