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
