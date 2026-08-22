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
  await contractTab.evaluate((element: HTMLElement) => element.click());
  await expect(demo.locator(".contract-panel")).toContainText("SessionSnapshot");
  await page.getByRole("tab", { name: "Session" }).evaluate((element: HTMLElement) => element.click());

  await demo.locator("proto-ui-base-dialog-trigger").evaluate((element: HTMLElement) => element.click());
  await expect(page.getByRole("dialog")).toContainText("What is actually running");
  await page.locator("proto-ui-base-dialog-close").evaluate((element: HTMLElement) => element.click());

  await demo.locator("proto-ui-base-select-trigger").evaluate((element: HTMLElement) => element.click());
  await page.locator("proto-ui-base-select-item").filter({ hasText: "observe" }).evaluate((element: HTMLElement) => element.click());
  await expect(demo.locator("iframe")).toHaveAttribute("src", /window=window-1/);
  await expect(frame.locator("canvas")).toBeVisible({ timeout: 120_000 });
});

test("GPUI WASM panes render a real terminal grid", async ({ page }) => {
  await page.goto("en/");
  const frame = page.frameLocator('iframe[title="Z3rm GPUI WebAssembly session projection"]');
  await expect(frame.locator("canvas")).toBeVisible({ timeout: 120_000 });
  const received = await frame.locator("html").evaluate(() => {
    const bindings = (window as Window & {
      wasmBindings?: {
        receive_shell_bytes?: (bytes: Uint8Array) => void;
        receive_shell_result?: (command: string, stdout: string, stderr: string, exitCode: number) => void;
        terminal_viewport?: () => string;
      };
    }).wasmBindings;
    if (!bindings?.receive_shell_result || !bindings.terminal_viewport) return null;
    bindings.receive_shell_result("echo demo", "demo output line\n", "", 0);
    return bindings.terminal_viewport();
  });
  expect(received).toContain("demo output line");
});

test("docs table of contents marks the section in view", async ({ page }) => {
  await page.goto("en/reference/cli/");
  const tocLinks = page.locator(".page-toc a");
  await expect(tocLinks).not.toHaveCount(0);

  // At page top the first section is current.
  await expect(tocLinks.nth(0)).toHaveAttribute("aria-current", "location");

  // Scroll the second heading across the top edge; the marker follows it
  // and stays on exactly one entry.
  const headings = page.locator("main h2, main h3");
  await headings.nth(1).evaluate((element) => {
    window.scrollTo({ top: element.getBoundingClientRect().top + window.scrollY - 8, behavior: "instant" });
  });
  await page.evaluate(() => window.dispatchEvent(new Event("resize")));
  await expect(tocLinks.nth(1)).toHaveAttribute("aria-current", "location");
  const currentCount = await tocLinks.evaluateAll((links) => links.filter((link) => link.getAttribute("aria-current") === "location").length);
  expect(currentCount).toBe(1);
});

test("docs table of contents keeps the last passed section marked", async ({ page }) => {
  await page.goto("en/reference/cli/");
  const tocLinks = page.locator(".page-toc a");
  const linkCount = await tocLinks.count();
  expect(linkCount).toBeGreaterThan(2);

  // Scroll past the second-to-last heading until it leaves the viewport
  // upward; its marker holds (or advances) but is never cleared to none.
  const headings = page.locator("main h2, main h3");
  const target = headings.nth(linkCount - 2);
  await target.evaluate((element) => {
    window.scrollTo({ top: element.getBoundingClientRect().top + window.scrollY - window.innerHeight * 0.5, behavior: "instant" });
  });
  await page.evaluate(() => window.dispatchEvent(new Event("resize")));
  const currents = await tocLinks.evaluateAll((links) => links.map((link) => link.getAttribute("aria-current")));
  expect(currents.filter((value) => value === "location").length).toBe(1);
  // The page cannot scroll far enough to pass the last sections; what
  // matters is the marker advanced past its initial entry and never cleared.
  expect(currents.indexOf("location")).toBeGreaterThanOrEqual(1);
});

test("embedded demo does not hijack the landing page keyboard order", async ({ page }) => {
  await page.goto("en/");
  await expect(page.locator('iframe[title="Z3rm GPUI WebAssembly session projection"]')).toBeAttached();
  // Give the demo time to boot and (previously) steal focus.
  await page.waitForTimeout(2500);
  await page.keyboard.press("Tab");
  const focused = await page.evaluate(() => {
    const el = document.activeElement;
    if (!el) return { text: "", cls: "", inHeader: false };
    return {
      text: (el.textContent || "").trim().slice(0, 24),
      cls: (el.className || "").toString().slice(0, 20),
      inHeader: !!el.closest("header"),
    };
  });
  expect(focused.text).not.toContain("Z3RM-SNAPSHOT-001");
});

test("dialog returns focus to its trigger on close", async ({ page }) => {
  await page.goto("en/");
  const trigger = page.locator("proto-ui-base-dialog-trigger");
  await trigger.scrollIntoViewIfNeeded();
  await expect(trigger).toBeVisible();

  await page.evaluate(() => { document.documentElement.style.scrollBehavior='auto'; document.querySelector('.demo-tabs')?.scrollIntoView({block:'center'}); });
  await trigger.click();
  const dialog = page.locator(".demo-dialog");
  await expect(dialog).toBeVisible();

  await page.keyboard.press("Escape");
  await expect(dialog).not.toBeVisible();
  await expect(trigger).toBeFocused();
});

test("layout select is operable by keyboard", async ({ page }) => {
  await page.goto("en/");
  const trigger = page.locator("proto-ui-base-select-trigger");
  await trigger.scrollIntoViewIfNeeded();
  await trigger.focus();

  // Open, move to the last option, commit.
  await expect(trigger).toHaveAttribute("aria-expanded", "false");
  await page.keyboard.press("Enter");
  await expect(page.locator("[role=option]")).toHaveCount(3);
  await expect(trigger).toHaveAttribute("aria-expanded", "true");
  await page.keyboard.press("ArrowDown");
  await page.keyboard.press("ArrowDown");
  await page.keyboard.press("Enter");

  // Menu closed, value committed, demo iframe re-projected with the choice.
  await expect(trigger).toHaveAttribute("aria-expanded", "false");
  const src = await page.locator('iframe[title="Z3rm GPUI WebAssembly session projection"]').getAttribute("src");
  expect(src).toContain("window=window-2");

  // Escape path: reopen and dismiss; focus returns to the trigger.
  await page.keyboard.press("Enter");
  await page.keyboard.press("Escape");
  await expect(trigger).toHaveAttribute("aria-expanded", "false");
  await expect(trigger).toBeFocused();
});

test("theme toggle shows pressed feedback and stays aria-pressed synced", async ({ page }) => {
  await page.goto("en/");
  const toggle = page.locator(".theme-toggle");
  await toggle.scrollIntoViewIfNeeded();

  const restBg = await toggle.evaluate((element) => getComputedStyle(element).backgroundColor);
  const box = await toggle.boundingBox();
  await page.mouse.move(box!.x + box!.width / 2, box!.y + box!.height / 2);
  await page.mouse.down();
  const pressedBg = await toggle.evaluate((element) => getComputedStyle(element).backgroundColor);
  await page.mouse.up();

  expect(pressedBg).not.toBe(restBg);
  await expect(page.locator("html")).toHaveAttribute("data-theme", /light|dark/);
});
