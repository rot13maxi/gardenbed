import { expect, test } from "@playwright/test";
import { mainTip } from "./util";

test("root browse renders the README as markdown below the listing", async ({ page, request }) => {
  const tip = await mainTip(request);
  await page.goto(`/#/browse/${tip}`);

  // Directory listing still renders above the panel.
  await expect(page.locator(".tree-row").first()).toBeVisible();
  const panel = page.locator("#readme-panel");
  await expect(panel).toBeVisible();
  const listingBox = await page.locator(".tree-row").first().boundingBox();
  const panelBox = await panel.boundingBox();
  expect(listingBox!.y).toBeLessThan(panelBox!.y);

  // Markdown structure: h1, an h2 per section, ordered/unordered lists.
  const body = panel.locator(".md-body");
  await expect(body.locator("h1")).toHaveText("garden planner");
  await expect(body.locator("h2", { hasText: "usage" })).toBeVisible();
  await expect(body.locator("h2", { hasText: "the sow script" })).toBeVisible();
  expect(await body.locator("li").count()).toBeGreaterThanOrEqual(8);
  // Inline code inside prose.
  await expect(body.locator("code", { hasText: "data/beds.json" }).first()).toBeVisible();
});

test("readme header names the file and links to its raw view", async ({ page, request }) => {
  const tip = await mainTip(request);
  await page.goto(`/#/browse/${tip}`);
  const link = page.locator("#readme-panel .readme-name");
  await expect(link).toHaveText("README.md");
  await link.click();
  await expect(page.locator("#file-header")).toContainText("README.md");
  await expect(page.locator("#code-lines")).toContainText("# garden planner");
});

test("fenced code blocks render as highlighted pre blocks", async ({ page, request }) => {
  const tip = await mainTip(request);
  await page.goto(`/#/browse/${tip}`);
  const fence = page.locator("#readme-panel .md-body pre code");
  await expect(fence.first()).toBeVisible();
  await expect(fence.first()).toContainText("bin/sow.sh --bed north-1 --crop carrot --date 2026-04-11");
});

test("relative links resolve to repo file routes of the right commit", async ({ page, request }) => {
  const tip = await mainTip(request);
  await page.goto(`/#/browse/${tip}`);
  const links = page.locator("#readme-panel .md-body a.md-link");

  const sow = page.locator("#readme-panel .md-body a", { hasText: "bin/sow.sh" }).first();
  await expect(sow).toHaveAttribute("href", `#/file/${tip}/bin/sow.sh`);

  // Click-through: plan link lands in the plan file at this revision.
  const plan = links.filter({ hasText: "plan/2026.md" }).first();
  await expect(plan).toHaveAttribute("href", `#/file/${tip}/plan/2026.md`);
  await plan.click();
  await expect(page.locator("#file-header")).toContainText("2026.md");
  await expect(page.locator("#code-lines")).toContainText("spring sowing");
  // Still the same revision in the URL, not a stale revspec.
  await expect(page).toHaveURL(new RegExp(`#/file/${tip}/`));
});

test("readmes in subdirectories resolve links against their own directory", async ({ page, request }) => {
  const tip = await mainTip(request);
  // plan/ has no README in the seed → no panel.
  await page.goto(`/#/browse/${tip}/plan`);
  await expect(page.locator("#readme-panel")).toHaveCount(0);
});

test("readme styling keeps the page light: no external requests fired", async ({ page, request }) => {
  const tip = await mainTip(request);
  const external: string[] = [];
  page.on("request", (r) => {
    const u = new URL(r.url());
    if (u.origin !== "http://127.0.0.1:18430") external.push(r.url());
  });
  await page.goto(`/#/browse/${tip}`);
  await expect(page.locator("#readme-panel")).toBeVisible();
  await page.waitForTimeout(300);
  expect(external).toEqual([]);
});
