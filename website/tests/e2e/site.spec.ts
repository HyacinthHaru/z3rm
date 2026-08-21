import AxeBuilder from "@axe-core/playwright";
import { expect, test } from "@playwright/test";

for (const locale of ["en", "zh"] as const) {
  test(`${locale} landing is navigable and accessible`, async ({ page }) => {
    await page.addInitScript(() => localStorage.setItem("z3rm-theme", "light"));
    await page.goto(`${locale}/`);
    await expect(page.locator("h1")).toContainText(locale === "zh" ? "Shell" : "shells");
    await expect(page.locator('img[src*="z3rm-terminal-grid.png"]').first()).toHaveAttribute("alt", /.+/);
    await expect(page.locator('a[href*="quick-start"]')).toHaveCount(3);
    expect(await page.evaluate(() => document.documentElement.scrollWidth)).toBeLessThanOrEqual(await page.evaluate(() => innerWidth));
    const results = await new AxeBuilder({ page }).analyze();
    expect(results.violations).toEqual([]);
  });
}

test("language and theme preferences survive navigation", async ({ page }) => {
  await page.goto("en/");
  await page.getByRole("button", { name: "Toggle theme" }).click();
  await expect(page.locator("html")).toHaveAttribute("data-theme", "dark");
  const localeHref = await page.getByRole("link", { name: "简体中文" }).getAttribute("href");
  expect(localeHref).toBe("/z3rm/zh/");
  await page.goto(localeHref!);
  await expect(page).toHaveURL(/\/zh\/$/);
  await expect(page.locator("html")).toHaveAttribute("data-theme", "dark");
});

test("theme toggle keeps pointer and keyboard activation semantically synchronized", async ({ page }) => {
  await page.addInitScript(() => localStorage.setItem("z3rm-theme", "light"));
  await page.goto("en/");

  const toggle = page.getByRole("button", { name: "Toggle theme" });
  await expect(toggle).toHaveAttribute("aria-pressed", "false");

  await toggle.focus();
  await page.keyboard.press("Space");
  await expect(page.locator("html")).toHaveAttribute("data-theme", "dark");
  await expect(toggle).toHaveAttribute("aria-pressed", "true");

  await page.keyboard.press("Enter");
  await expect(page.locator("html")).toHaveAttribute("data-theme", "light");
  await expect(toggle).toHaveAttribute("aria-pressed", "false");

  await toggle.click();
  await expect(page.locator("html")).toHaveAttribute("data-theme", "dark");
  await expect(toggle).toHaveAttribute("aria-pressed", "true");
});

test("documentation routes expose navigation landmarks", async ({ page }) => {
  await page.goto("en/reference/cli/");
  await expect(page.getByRole("heading", { level: 1 })).toContainText("CLI");
  await expect(page.locator('nav[aria-label="Documentation"]')).toHaveCount(1);
  await expect(page.getByRole("main")).toContainText("capture-pane");
});

test("implementation status renders verified evidence rows", async ({ page }) => {
  await page.goto("en/implementation-status/");
  await expect(page.locator("#z3rm-mux-001")).toContainText("Verified");
  await expect(page.locator(".status-row")).toHaveCount(13);
});

test("GPUI WASM demo and Proto UI controls work together", async ({ page }) => {
  await page.goto("en/");
  const demo = page.locator("[data-z3rm-wasm-demo]");
  await expect(demo).toContainText("One mux snapshot");

  const frame = page.frameLocator('iframe[title="Z3rm GPUI WebAssembly session projection"]');
  await expect(frame.locator("canvas")).toBeVisible({ timeout: 120_000 });

  const contractTab = page.getByRole("tab", { name: "Data contract" });
  await contractTab.scrollIntoViewIfNeeded();
  await contractTab.click();
  await expect(demo.locator(".contract-panel")).toContainText("SessionSnapshot");
  await page.getByRole("tab", { name: "Session" }).click();

  await demo.locator("proto-ui-base-dialog-trigger").click();
  await expect(page.getByRole("dialog")).toContainText("What is actually running");
  await page.locator("proto-ui-base-dialog-close").click();

  await demo.locator("proto-ui-base-select-trigger").click();
  await page.locator("proto-ui-base-select-item").filter({ hasText: "observe" }).click();
  await expect(demo.locator("iframe")).toHaveAttribute("src", /window=window-1/);
  await expect(frame.locator("canvas")).toBeVisible({ timeout: 120_000 });
});
