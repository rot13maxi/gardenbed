# Instructions

- Following Playwright test failed.
- Explain why, be concise, respect Playwright best practices.
- Provide a snippet of code with the fix, if possible.

# Test info

- Name: e2e/tests/agent-prompt.spec.ts >> copy button puts the prompt on the clipboard
- Location: e2e/tests/agent-prompt.spec.ts:37:5

# Error details

```
Error: page.goto: Protocol error (Page.navigate): Cannot navigate to invalid URL
Call log:
  - navigating to "/", waiting until "load"

```

# Test source

```ts
  1  | import { expect, test } from "@playwright/test";
  2  | import { E2E_ROOT } from "../playwright.config";
  3  | import { mainTip } from "./util";
  4  | 
  5  | test("agent prompt modal opens with machine-specific facts", async ({ page, request }) => {
  6  |   const tip = await mainTip(request);
  7  |   await page.goto("/");
  8  |   await page.locator("#agent-prompt-btn").click();
  9  | 
  10 |   const modal = page.locator("#agent-modal");
  11 |   await expect(modal).toBeVisible();
  12 |   const prompt = page.locator("#agent-prompt-text");
  13 | 
  14 |   // The prompt is grounded in this server's real state.
  15 |   await expect(prompt).toContainText(tip);
  16 |   await expect(prompt).toContainText(E2E_ROOT);
  17 |   await expect(prompt).toContainText("refs/heads/main");
  18 | });
  19 | 
  20 | test("agent prompt teaches the connect-and-work loop", async ({ page }) => {
  21 |   await page.goto("/");
  22 |   await page.locator("#agent-prompt-btn").click();
  23 |   const prompt = page.locator("#agent-prompt-text");
  24 |   for (const needle of [
  25 |     "bed clone",
  26 |     "bed sparse add",
  27 |     "bed publish",
  28 |     "bed rebase main",
  29 |     "gardenbed-mcp",
  30 |     "sparse",
  31 |     "changeset",
  32 |   ]) {
  33 |     await expect(prompt).toContainText(needle);
  34 |   }
  35 | });
  36 | 
  37 | test("copy button puts the prompt on the clipboard", async ({ context, page }) => {
  38 |   await context.grantPermissions(["clipboard-read", "clipboard-write"]);
> 39 |   await page.goto("/");
     |              ^ Error: page.goto: Protocol error (Page.navigate): Cannot navigate to invalid URL
  40 |   await page.locator("#agent-prompt-btn").click();
  41 |   await page.locator("#copy-prompt-btn").click();
  42 |   await expect(page.locator("#copy-prompt-btn")).toContainText(/copied/i);
  43 | 
  44 |   const clip = await page.evaluate(() => navigator.clipboard.readText());
  45 |   const shown = await page.locator("#agent-prompt-text").innerText();
  46 |   expect(clip).toEqual(shown);
  47 |   expect(clip.length).toBeGreaterThan(200);
  48 | });
  49 | 
  50 | test("modal closes via Esc, backdrop, and button", async ({ page }) => {
  51 |   await page.goto("/");
  52 |   const open = async () => {
  53 |     await page.locator("#agent-prompt-btn").click();
  54 |     await expect(page.locator("#agent-modal")).toBeVisible();
  55 |   };
  56 | 
  57 |   await open();
  58 |   await page.keyboard.press("Escape");
  59 |   await expect(page.locator("#agent-modal")).toBeHidden();
  60 | 
  61 |   await open();
  62 |   // Backdrop: click the viewport corner, outside the dialog card.
  63 |   await page.mouse.click(5, 5);
  64 |   await expect(page.locator("#agent-modal")).toBeHidden();
  65 | 
  66 |   await open();
  67 |   await page.locator("#agent-modal button", { hasText: /close|✕|×/i }).first().click();
  68 |   await expect(page.locator("#agent-modal")).toBeHidden();
  69 | 
  70 |   // Focus returns to the trigger.
  71 |   await expect(page.locator("#agent-prompt-btn")).toBeFocused();
  72 | });
  73 | 
```