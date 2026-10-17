import { APIRequestContext, expect, test } from "@playwright/test";
import { findByMessage, log, mainTip } from "./util";

async function fileAt(
  request: APIRequestContext,
  cs: string,
  path: string,
): Promise<string> {
  const r = await request.get(
    `/api/file?cs=${encodeURIComponent(cs)}&path=${encodeURIComponent(path)}`,
  );
  expect(r.ok()).toBeTruthy();
  const j = await r.json();
  return j.content ?? "";
}



test("modify rows auto-expand into unified diffs with real +/- lines", async ({ page, request }) => {
  const tip = await mainTip(request);
  const tune = findByMessage(await log(request, tip), "Tune grid spacing");
  const parent = tune.parents[0];
  const oldText = await fileAt(request, parent, "garden.js");
  const newText = await fileAt(request, tune.id, "garden.js");

  await page.goto(`/#/changeset/${tune.id}`);
  const row = page.locator(".change-row", { hasText: "garden.js" });
  const expander = row.locator(".diff-expander");
  // Small diff → auto-expanded.
  await expect(expander).toHaveAttribute("aria-expanded", "true");
  const block = page.locator(".diff-block").filter({ hasText: /./ }).first();
  // Diff bodies load after the expander flips: wait for lines before
  // collecting (allTextContents is a point-in-time read).
  await expect(block.locator(".diff-line").first()).toBeVisible();

  // This edit is purely additive: adds present, no dels.
  const adds = await block.locator(".diff-line.diff-add .diff-text").allTextContents();
  const dels = await block.locator(".diff-line.diff-del .diff-text").allTextContents();
  expect(adds.length).toBeGreaterThan(0);
  expect(dels.length).toBe(0);
  for (const a of adds) expect(newText).toContain(a);
  await expect(block.locator(".diff-line.diff-add .diff-sign").first()).toHaveText("+");
  expect(oldText).toContain("renderBedGrid"); // baseline: both sides fetched
});

test("deleted files render all-minus diffs against the empty new side", async ({ page, request }) => {
  const tip = await mainTip(request);
  const prune = findByMessage(await log(request, tip), "Prune outdated notes");
  const oldText = await fileAt(request, prune.parents[0], "notes.txt");
  expect(oldText).toContain("scratch notes");

  await page.goto(`/#/changeset/${prune.id}`);
  // Auto-expanded small diff: only delete lines, sign '-', text from the
  // pre-delete content.
  const row = page.locator(".change-row", { hasText: "notes.txt" });
  await expect(row.locator(".diff-expander")).toHaveAttribute("aria-expanded", "true");
  const block = page.locator(".diff-block").first();
  await expect(block.locator(".diff-line.diff-del").first()).toBeVisible();
  expect(await block.locator(".diff-line.diff-add").count()).toBe(0);
  await expect(block.locator(".diff-line.diff-del .diff-sign").first()).toHaveText("-");
  const dels = await block.locator(".diff-line.diff-del .diff-text").allTextContents();
  for (const d of dels) expect(oldText).toContain(d);
});

test("collapse and re-expand a diff block", async ({ page, request }) => {
  const tip = await mainTip(request);
  const tune = findByMessage(await log(request, tip), "Tune grid spacing");
  await page.goto(`/#/changeset/${tune.id}`);
  const expander = page.locator(".diff-expander").first();
  await expect(expander).toHaveAttribute("aria-expanded", "true");
  await expander.click();
  await expect(expander).toHaveAttribute("aria-expanded", "false");
  await expect(page.locator(".diff-block").first()).toBeHidden();
  await expander.click();
  await expect(expander).toHaveAttribute("aria-expanded", "true");
});

test("pure rename shows a content-unchanged note instead of a diff", async ({ page, request }) => {
  const note = "content unchanged";
  const rename = findByMessage(await log(request, await mainTip(request)), "Rename spring plan");
  await page.goto(`/#/changeset/${rename.id}`);
  await expect(page.locator(".diff-note")).toContainText(note);
});

test("added file diffs are all-plus against an empty old side", async ({ page, request }) => {
  const tip = await mainTip(request);
  const grid = findByMessage(await log(request, tip), "Build interactive bed grid");
  await page.goto(`/#/changeset/${grid.id}`);
  const block = page.locator(".diff-block").first();
  await expect(block.locator(".diff-line.diff-add").first()).toBeVisible();
  expect(await block.locator(".diff-line.diff-del").count()).toBe(0);
  expect(await block.locator(".diff-line.diff-add").count()).toBeGreaterThan(30);
});

test("compare page renders expandable diffs between arbitrary revs", async ({ page, request }) => {
  const tip = await mainTip(request);
  const entries = await log(request, tip);
  const root = entries[entries.length - 1];
  await page.goto(`/#/compare?a=${root.id}&b=main`);
  await page.locator("#cmp-run").click();
  await expect(page.locator("#cmp-results .change-row").first()).toBeVisible();
  // Root → tip adds many files; the README add row is expanded with plus lines.
  const readme = page.locator("#cmp-results .change-row", { hasText: "README.md" });
  await expect(readme).toBeVisible();
  const blocks = page.locator("#cmp-results .diff-block");
  await expect(blocks.first().locator(".diff-line").first()).toBeVisible();
});
