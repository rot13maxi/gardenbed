import { defineConfig } from "@playwright/test";

export const E2E_ROOT = "/tmp/gardenbed-web-e2e-root";
export const BASE_URL = "http://127.0.0.1:18430";
const BIN = `${__dirname}/../target/debug/gardenbed-web`;

export default defineConfig({
  testDir: "./tests",
  globalSetup: "./global-setup.ts",
  timeout: 30_000,
  retries: 0,
  workers: 1,
  reporter: [["list"]],
  use: {
    baseURL: BASE_URL,
    headless: true,
    screenshot: "only-on-failure",
    trace: "retain-on-failure",
  },
  webServer: {
    command: `${BIN} --root ${E2E_ROOT} --addr 127.0.0.1:18430`,
    url: `${BASE_URL}/api/refs`,
    reuseExistingServer: false,
    timeout: 30_000,
  },
});
