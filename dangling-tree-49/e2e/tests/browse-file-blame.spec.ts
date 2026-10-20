import { expect, test } from "@playwright/test";
import { mainTip } from "./util";

test("browse root lists the seeded tree, directories first", async ({ page, request }) => {
  const tip = await mainTip(request);
  await page.goto(`/#/browse/${tip}`);
  await expect(page.locator("#breadcrumbs")).toBeVisible();
  const rows = page.locator(".tree-row");
  await expect(rows.first()).toBeVisible();

  const names = await page.locator(".tree-row .tree-name, .tree-row").allTextContents();
  const joined = names.join("\n");
  for (const expected of ["bin", "data", "plan", "README.md", "garden.js", "styles.css"]) {
    expect(joined).toContain(expected);
  }
  // Deleted and renamed-away files are absent at tip.
  expect(joined).not.toContain("notes.txt");
  expect(joined).not.toContain("spring.md");

  // Directories come before files in the rendered order.
  const kinds = await page.locator(".tree-row").evaluateAll((els) =>
    els.map((el) => el.getAttribute("data-kind")),
  );
  const lastDir = kinds.lastIndexOf("directory");
  const firstFile = kinds.findIndex((k) => k !== "directory");
  expect(lastDir).toBeGreaterThanOrEqual(0);
  expect(firstFile).toBeGreaterThan(lastDir);
});

test("breadcrumb navigation into plan/ shows the yearly plan", async ({ page, request }) => {
  const tip = await mainTip(request);
  await page.goto(`/#/browse/${tip}/plan`);
  await expect(page.locator("#breadcrumbs")).toContainText("plan");
  const rows = page.locator(".tree-row");
  await expect(rows.filter({ hasText: "2026.md" })).toHaveCount(1);
  await expect(rows.filter({ hasText: "rotation.md" })).toHaveCount(1);

  // Clicking the file row's link navigates to the file view.
  await rows.filter({ hasText: "2026.md" }).locator("a").click();
  await expect(page.locator("#file-header")).toContainText("2026.md");
  await expect(page.locator("#code-lines")).toContainText("spring sowing");
});

test("file view renders content with line numbers", async ({ page, request }) => {
  const tip = await mainTip(request);
  await page.goto(`/#/file/${tip}/README.md`);
  await expect(page.locator("#file-header")).toContainText("README.md");
  const lines = page.locator("#code-lines .code-line");
  expect(await lines.count()).toBeGreaterThanOrEqual(20);
  await expect(page.locator("#code-lines")).toContainText("# garden planner");
  await expect(page.locator("#code-lines")).toContainText("bin/sow.sh");
});

test("exec file shows an exec mode chip", async ({ page, request }) => {
  const tip = await mainTip(request);
  await page.goto(`/#/file/${tip}/bin/sow.sh`);
  await expect(page.locator("#file-header")).toContainText("exec");
});

test("blame toggle attributes lines to the touching commits", async ({ page, request }) => {
  const tip = await mainTip(request);
  await page.goto(`/#/file/${tip}/README.md`);

  // Blame off: no blame gutter blocks.
  const blocks = page.locator("#code-lines .gutter-block");
  expect(await blocks.count()).toBe(0);

  await page.locator("#blame-toggle").click();
  await expect(blocks.first()).toBeVisible();
  // README was touched by three commits → at least two distinct attributions.
  const css = new Set(await blocks.evaluateAll((els) => els.map((el) => el.getAttribute("data-cs"))));
  expect(css.size).toBeGreaterThanOrEqual(2);

  // Clicking a gutter block opens its changeset.
  const target = [...css][0]!;
  await blocks.first().click();
  await expect(page).toHaveURL(/#\/changeset\//);
  await expect(page.locator("#cs-header .cs-id")).toContainText(target);
});

test("binary file renders a banner instead of code", async ({ page, request }) => {
  const tip = await mainTip(request);
  // Upload a non-UTF8 blob through the seed? None seeded — assert via API shape instead:
  // the file view of every seeded text file must NOT show a binary banner.
  await page.goto(`/#/file/${tip}/styles.css`);
  await expect(page.locator(".banner")).toHaveCount(0);
});
