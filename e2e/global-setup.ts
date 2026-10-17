import { execSync } from "node:child_process";

/**
 * Builds the gardenbed-web binary the e2e webServer will run. Seeding and
 * serving both live in serve-e2e.sh (the webServer command): Playwright
 * launches the webServer before globalSetup, so setup-time seeding would
 * mutate the root out from under the already-running server.
 */
export default async function globalSetup() {
  execSync("cargo build -p gardenbed-web", {
    cwd: `${__dirname}/..`,
    stdio: "inherit",
  });
}
