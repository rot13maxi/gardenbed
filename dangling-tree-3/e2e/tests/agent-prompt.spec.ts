import { expect, test } from "@playwright/test";
import { E2E_ROOT } from "../playwright.config";
import { mainTip } from "./util";

test("agent prompt modal opens with machine-specific facts", async ({ page, request }) => {
  const tip = await mainTip(request);
  await page.goto("/");
  await page.locator("#agent-prompt-btn").click();

  const modal = page.locator("#agent-modal");
  await expect(modal).toBeVisible();
  const prompt = page.locator("#agent-prompt-text");

  // The prompt is grounded in this server's real state.
  await expect(prompt).toContainText(tip);
  await expect(prompt).toContainText(E2E_ROOT);
  await expect(prompt).toContainText("refs/heads/main");
});

test("agent prompt teaches the connect-and-work loop", async ({ page }) => {
  await page.goto("/");
  await page.locator("#agent-prompt-btn").click();
  const prompt = page.locator("#agent-prompt-text");
  for (const needle of [
    "bed clone",
    "bed sparse add",
    "bed publish",
    "bed rebase main",
    "gardenbed-mcp",
    "sparse",
    "changeset",
  ]) {
    await expect(prompt).toContainText(needle);
  }
});

test("copy button puts the prompt on the clipboard", async ({ context, page }) => {
  await context.grantPermissions(["clipboard-read", "clipboard-write"]);
  await page.goto("/");
  await page.locator("#agent-prompt-btn").click();
  await page.locator("#copy-prompt-btn").click();
  await expect(page.locator("#copy-prompt-btn")).toContainText(/copied/i);

  const clip = await page.evaluate(() => navigator.clipboard.readText());
  const shown = await page.locator("#agent-prompt-text").innerText();
  expect(clip).toEqual(shown);
  expect(clip.length).toBeGreaterThan(200);
});

test("modal closes via Esc, backdrop, and button", async ({ page }) => {
  await page.goto("/");
  const open = async () => {
    await page.locator("#agent-prompt-btn").click();
    await expect(page.locator("#agent-modal")).toBeVisible();
  };

  await open();
  await page.keyboard.press("Escape");
  await expect(page.locator("#agent-modal")).toBeHidden();

  await open();
  // Backdrop: click the viewport corner, outside the dialog card.
  await page.mouse.click(5, 5);
  await expect(page.locator("#agent-modal")).toBeHidden();

  await open();
  await page.locator("#agent-modal button", { hasText: /close|✕|×/i }).first().click();
  await expect(page.locator("#agent-modal")).toBeHidden();

  // Focus returns to the trigger.
  await expect(page.locator("#agent-prompt-btn")).toBeFocused();
});
