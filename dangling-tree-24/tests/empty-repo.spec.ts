import { expect, test } from "@playwright/test";
import { spawn, type ChildProcess } from "node:child_process";
import { rmSync } from "node:fs";
import { isAbsolute, join } from "node:path";

/**
 * Unseeded repository: the overview must render refs (none) and request
 * counters even though /api/log 404s, with a friendly empty-history state
 * instead of a full-page error.
 */

const EMPTY_ROOT = "/tmp/gardenbed-web-e2e-empty";
const PORT = 18442;
const URL = `http://127.0.0.1:${PORT}`;

// Honor a globally-set CARGO_TARGET_DIR like serve-e2e.sh does (relative
// values resolve against the workspace, matching cargo's build cwd there).
const WORKSPACE = join(__dirname, "..", "..");
const cargoTarget = process.env.CARGO_TARGET_DIR;
const TARGET_DIR = cargoTarget
  ? isAbsolute(cargoTarget)
    ? cargoTarget
    : join(WORKSPACE, cargoTarget)
  : join(WORKSPACE, "target");
const BIN = join(TARGET_DIR, "debug", "gardenbed-web");

let srv: ChildProcess;

test.beforeAll(async () => {
  rmSync(EMPTY_ROOT, { recursive: true, force: true });
  srv = spawn(
    BIN,
    ["--root", EMPTY_ROOT, "--addr", `127.0.0.1:${PORT}`],
    { stdio: ["ignore", "pipe", "inherit"] },
  );
  const ready = Promise.withResolvers<void>();
  const to = setTimeout(
    () => ready.reject(new Error("empty-repo server did not start")),
    15_000,
  );
  srv.stdout!.once("data", () => {
    clearTimeout(to);
    ready.resolve();
  });
  srv.on("exit", (code) => ready.reject(new Error(`empty-repo server exited ${code}`)));
  await ready.promise;
});

test.afterAll(() => {
  srv?.kill();
});

test("empty repo overview renders refs and stats, no full-page error", async ({ page }) => {
  const errors: string[] = [];
  page.on("pageerror", (e) => errors.push(String(e)));
  // Console must stay quiet too, and the doomed /api/log call must not
  // fire at all: with zero refs the overview skips the log fetch.
  const consoleErrors: string[] = [];
  page.on("console", (m) => {
    if (m.type() === "error") consoleErrors.push(m.text());
  });
  const apiCalls: string[] = [];
  page.on("request", (r) => apiCalls.push(r.url()));

  await page.goto(URL + "/");
  await expect(page.locator("#stat-chips .chip").first()).toBeVisible();

  // Friendly empty history, not "Could not load overview".
  await expect(page.locator("#recent-list .empty-state")).toBeVisible();
  await expect(page.locator("#recent-list .empty-state")).toContainText(/no history yet/i);
  await expect(page.locator(".empty-state", { hasText: /could not load/i })).toHaveCount(0);

  expect(errors).toEqual([]);
  expect(consoleErrors).toEqual([]);
  expect(apiCalls.filter((u) => u.includes("/api/log"))).toEqual([]);
  expect(apiCalls.filter((u) => u.includes("/api/refs")).length).toBeGreaterThan(0);

  // Refs chip reports zero; tip chip shows the em-dash placeholder.
  await expect(page.locator("#stat-chips")).toContainText("refs");
  await expect(page.locator("#stat-chips")).toContainText("0");
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
