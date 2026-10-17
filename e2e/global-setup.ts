import { execFileSync, execSync } from "node:child_process";
import { rmSync } from "node:fs";
import { E2E_ROOT } from "./playwright.config";

export default async function globalSetup() {
  // Build the web binary (debug) from the workspace root.
  execSync("cargo build -p gardenbed-web", {
    cwd: `${__dirname}/..`,
    stdio: "inherit",
  });
  const bin = `${__dirname}/../target/debug/gardenbed-web`;

  // Fresh deterministic seed every run.
  rmSync(E2E_ROOT, { recursive: true, force: true });
  const out = execFileSync(bin, ["seed", "--root", E2E_ROOT], {
    encoding: "utf8",
  });
  process.stdout.write(out);
  if (!/^seeded tip [0-9a-f]{64}$/m.test(out)) {
    throw new Error(`seed did not print a tip id: ${JSON.stringify(out)}`);
  }
}
