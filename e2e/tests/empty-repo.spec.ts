import { expect, test } from "@playwright/test";
import { spawn, type ChildProcess } from "node:child_process";
import { rmSync } from "node:fs";

/**
 * Unseeded repository: the overview must render refs (none) and request
 * counters even though /api/log 404s, with a friendly empty-history state
 * instead of a full-page error.
 */

const EMPTY_ROOT = "/tmp/gardenbed-web-e2e-empty";
const PORT = 18442;
const URL = `http://127.0.0.1:${PORT}`;

let srv: ChildProcess;

test.beforeAll(async () => {
  rmSync(EMPTY_ROOT, { recursive: true, force: true });
  srv = spawn(
    `${__dirname}/../../target/debug/gardenbed-web`,
    ["--root", EMPTY_ROOT, "--addr", `127.0.0.1:${PORT}`],
    { stdio: ["ignore", "pipe", "inherit"] },
  );
  await new Promise<void>((resolve, reject) => {
    const to = setTimeout(() => reject(new Error("empty-repo server did not start")), 15_000);
    srv.stdout!.once("data", () => {
      clearTimeout(to);
      resolve();
    });
    srv.on("exit", (code) => reject(new Error(`empty-repo server exited ${code}`)));
  });
});

test.afterAll(() => {
  srv?.kill();
});

test("empty repo overview renders refs and stats, no full-page error", async ({ page }) => {
  const errors: string[] = [];
  page.on("pageerror", (e) => errors.push(String(e)));

  await page.goto(URL + "/");
  await expect(page.locator("#stat-chips .chip").first()).toBeVisible();

  // Friendly empty history, not "Could not load overview".
  await expect(page.locator("#recent-list .empty-state")).toBeVisible();
  await expect(page.locator("#recent-list .empty-state")).toContainText(/no history yet/i);
  await expect(page.locator(".empty-state", { hasText: /could not load/i })).toHaveCount(0);

  // Refs chip reports zero; tip chip shows the em-dash placeholder.
  await expect(page.locator("#stat-chips")).toContainText("refs");
  await expect(page.locator("#stat-chips")).toContainText("0");
  expect(errors).toEqual([]);
});

test("empty repo hides the all-zero gRPC requests chip", async ({ page }) => {
  await page.goto(URL + "/");
  await expect(page.locator("#stat-chips .chip").first()).toBeVisible();
  // All counters are zero in a web-only process → chip hidden, never "0".
  await expect(
    page.locator("#stat-chips .chip", { hasText: /requests served/i }),
  ).toHaveCount(0);
});

test("empty repo request counters panel still renders its rows", async ({ page }) => {
  await page.goto(URL + "/");
  await expect(page.locator("#stat-chips .chip").first()).toBeVisible();
  await expect(page.locator("#stats-table .stats-row").first()).toBeVisible();
});

test("no ref cards on an unseeded repository", async ({ page }) => {
  await page.goto(URL + "/");
  await expect(page.locator("#stat-chips .chip").first()).toBeVisible();
  await expect(page.locator("#refs-cards .ref-card")).toHaveCount(0);
});
