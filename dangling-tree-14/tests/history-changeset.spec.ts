import { expect, test } from "@playwright/test";
import { changeset, findByMessage, log, mainTip } from "./util";

test("history timeline lists the seeded commits newest first", async ({ page, request }) => {
  const tip = await mainTip(request);
  const entries = await log(request, tip);
  expect(entries.length).toBeGreaterThanOrEqual(10);

  await page.goto("/#/log");
  await expect(page.locator("#log-list")).toBeVisible();
  const rows = page.locator("#log-list .log-row");
  await expect(rows.first()).toBeVisible();
  expect(await rows.count()).toBeGreaterThanOrEqual(10);

  // Same order as the API answer: first row is the tip commit.
  await expect(rows.first()).toHaveAttribute("data-cs", entries[0].id);
  await expect(page.locator("#log-list")).toContainText("Document the sow script");
  await expect(page.locator("#log-list")).toContainText("Initial commit: README");
});

test("clicking a history row navigates to the changeset page", async ({ page, request }) => {
  const tip = await mainTip(request);
  const entries = await log(request, tip);
  await page.goto("/#/log");
  await page.locator(`#log-list .log-row[data-cs='${entries[1].id}'] a`).first().click();
  await expect(page).toHaveURL(new RegExp(`#/changeset/${entries[1].id}`));
  await expect(page.locator("#cs-header .cs-id")).toHaveText(entries[1].id);
});

test("changeset page shows metadata and the seeded change list", async ({ page, request }) => {
  const tip = await mainTip(request);
  const entries = await log(request, tip);
  const grid = findByMessage(entries, "Build interactive bed grid");

  await page.goto(`/#/changeset/${grid.id}`);
  await expect(page.locator("#cs-header")).toContainText("Build interactive bed grid");
  await expect(page.locator("#cs-header")).toContainText("Ken Furrow");

  const add = page.locator(".change-row[data-op='add']");
  await expect(add).toHaveCount(1);
  await expect(add.locator(".change-path")).toHaveText("garden.js");
});

test("rename changeset renders src to dst", async ({ page, request }) => {
  const rename = findByMessage(await log(request, await mainTip(request)), "Rename spring plan");
  const detail = await changeset(request, rename.id);
  const rc = detail.changes.find((c) => c.op === "rename");
  expect(rc).toBeTruthy();

  await page.goto(`/#/changeset/${rename.id}`);
  const rows = page.locator(".change-row[data-op='rename']");
  await expect(rows).toHaveCount(1);
  await expect(rows.locator(".change-path")).toContainText("plan/spring.md");
  await expect(rows.locator(".change-path")).toContainText("plan/2026.md");
});

test("changeset page links parent chips to parent changesets", async ({ page, request }) => {
  const tip = await mainTip(request);
  const entries = await log(request, tip);
  const doc = findByMessage(entries, "Document the sow script");
  expect(doc.parents.length).toBe(1);

  await page.goto(`/#/changeset/${doc.id}`);
  const parent = page.locator(`#cs-header a[href*='#/changeset/${doc.parents[0]}']`);
  await expect(parent.first()).toBeVisible();
  await parent.first().click();
  await expect(page.locator("#cs-header .cs-id")).toHaveText(doc.parents[0]);
});

test("view-file link on a change row opens the file at that changeset", async ({ page, request }) => {
  const tip = await mainTip(request);
  const sow = findByMessage(await log(request, tip), "Add sow script");
  await page.goto(`/#/changeset/${sow.id}`);
  await page.locator(".change-row .view-file").first().click();
  await expect(page).toHaveURL(/#\/file\/[0-9a-f]{64}\/bin%2Fsow\.sh|#\/file\/[0-9a-f]{64}\/bin\/sow\.sh/);
  await expect(page.locator("#file-header")).toContainText("sow.sh");
  await expect(page.locator("#code-lines")).toContainText("#!/bin/sh");
});
