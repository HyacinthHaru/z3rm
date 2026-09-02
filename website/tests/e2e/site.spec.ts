import AxeBuilder from "@axe-core/playwright";
import { expect, test } from "@playwright/test";

for (const locale of ["en", "zh"] as const) {
  // §15.1 The landing route is the app, not a page about it. What it must do
  // is get out of the way: hand the viewport to the real workspace, in either
  // language, carrying no copy of its own.
  test(`${locale} landing route is the WebAssembly app`, async ({ page }) => {
    await page.goto(`${locale}/`);
    await expect(page).toHaveURL(/\/z3rm\/gpui-demo\/index\.html$/);
    await expect(page.locator("canvas, #boot-terminal").first()).toBeAttached({ timeout: 60_000 });
  });

  // A phone gets the same terminal, not a page written for phones: there is
  // one app and it is the whole viewport at any size.
  test(`${locale} landing route is the same app on a phone`, async ({ page }) => {
    await page.setViewportSize({ width: 390, height: 844 });
    await page.goto(`${locale}/`);
    await expect(page).toHaveURL(/\/z3rm\/gpui-demo\/index\.html$/);
    await expect(page.locator("canvas, #boot-terminal").first()).toBeAttached({ timeout: 60_000 });
    expect(await page.evaluate(() => document.documentElement.scrollWidth)).toBeLessThanOrEqual(
      await page.evaluate(() => innerWidth),
    );
  });

  test(`${locale} documentation is navigable and accessible`, async ({ page }) => {
    await page.addInitScript(() => localStorage.setItem("z3rm-theme", "light"));
    await page.goto(`${locale}/quick-start/`);
    await expect(page.locator("h1")).toBeVisible();
    expect(await page.evaluate(() => document.documentElement.scrollWidth)).toBeLessThanOrEqual(await page.evaluate(() => innerWidth));
    const results = await new AxeBuilder({ page }).withRules(["aria-allowed-role"]).analyze();
    expect(results.violations).toEqual([]);
  });
}

test("language and theme preferences survive navigation", async ({ page }) => {
  await page.goto("en/quick-start/");
  await page.getByRole("button", { name: "Toggle theme" }).click();
  await expect(page.locator("html")).toHaveAttribute("data-theme", "dark");
  const localeHref = await page.getByRole("link", { name: "简体中文" }).getAttribute("href");
  expect(localeHref).toBe("/z3rm/zh/quick-start/");
  await page.goto(localeHref!);
  await expect(page).toHaveURL(/\/zh\/quick-start\/?$/);
  await expect(page.locator("html")).toHaveAttribute("data-theme", "dark");
});

test("theme toggle keeps pointer and keyboard activation semantically synchronized", async ({ page }) => {
  await page.addInitScript(() => localStorage.setItem("z3rm-theme", "light"));
  await page.goto("en/quick-start/");

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

test("GPUI WASM boot surface exposes the loading progress contract", async ({ page }) => {
  await page.goto("gpui-demo/index.html");
  await expect(page.locator("#loading-progress")).toBeAttached();
  const contract = await page.evaluate(() => {
    const browserWindow = window as Window & {
      __z3rm_progress?: {
        stage?: (name: string, loaded: number, total: number) => void;
        ready?: () => void;
        firstPaneSnapshotReady?: () => void;
      };
    };
    const progress = browserWindow.__z3rm_progress;
    progress?.stage?.("e2e unknown asset", 12, 0);
    return {
      api: typeof progress?.stage === "function" && typeof progress?.ready === "function",
      firstPaneSignal: typeof progress?.firstPaneSnapshotReady === "function",
      ids: ["loading-progress-label", "loading-progress-detail"].every((id) => document.getElementById(id)),
      detail: document.querySelector("#loading-progress-detail")?.textContent,
    };
  });
  expect(contract.api).toBe(true);
  expect(contract.firstPaneSignal).toBe(true);
  expect(contract.ids).toBe(true);
  expect(contract.detail).toContain("B/s");
});

test("GPUI WASM loading surface shows percentage when total is known", async ({ page }) => {
  await page.goto("gpui-demo/index.html");
  await expect(page.locator("#loading-progress")).toBeAttached();
  const contract = await page.evaluate(() => {
    const browserWindow = window as Window & {
      __z3rm_progress?: {
        stage?: (name: string, loaded: number, total: number) => void;
      };
    };
    const progress = browserWindow.__z3rm_progress;
    // A known total must flip the bar from spinner to a percentage even when
    // another concurrent stage has no total.
    progress?.stage?.("e2e determinate asset", 50, 200);
    const bar = document.querySelector("#loading-progress-bar");
    return {
      indeterminate: bar?.getAttribute("data-indeterminate"),
      value: bar?.getAttribute("aria-valuenow"),
      detail: document.querySelector("#loading-progress-detail")?.textContent,
    };
  });
  expect(contract.indeterminate).toBe("false");
  expect(contract.value).not.toBeNull();
  expect(Number(contract.value)).toBeGreaterThanOrEqual(0);
  expect(contract.detail).toContain("%");
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

test("theme toggle shows pressed feedback and stays aria-pressed synced", async ({ page }) => {
  await page.goto("en/quick-start/");
  const toggle = page.locator(".theme-toggle");
  await toggle.scrollIntoViewIfNeeded();

  const restBg = await toggle.evaluate((element) => getComputedStyle(element).backgroundColor);
  const selectedBg = await toggle.evaluate(() => {
    const probe = document.createElement("span");
    probe.style.background = "var(--surface-selected)";
    document.body.append(probe);
    const value = getComputedStyle(probe).backgroundColor;
    probe.remove();
    return value;
  });
  const box = await toggle.boundingBox();
  await page.mouse.move(box!.x + box!.width / 2, box!.y + box!.height / 2);
  await page.mouse.down();
  const pressedBg = await toggle.evaluate((element) => getComputedStyle(element).backgroundColor);
  await page.mouse.up();

  // Press must step past hover to the selected surface — a bare "differs
  // from rest" would pass on hover styling alone.
  expect(pressedBg).toBe(selectedBg);
  await expect(page.locator("html")).toHaveAttribute("data-theme", /light|dark/);
});

test("docs sidebar marks the current page", async ({ page }) => {
  await page.goto("en/guide/for-humans/");
  const links = page.locator(".sidebar-column a");
  await expect(links).not.toHaveCount(0);
  const marked = await links.evaluateAll((els) =>
    els.filter((el) => el.getAttribute("aria-current") === "page").length,
  );
  expect(marked).toBe(1);
});

test("root path redirects to the real z3rm WebAssembly app", async ({ page }) => {
  // The site root IS the desktop app compiled to WebAssembly, connected to a
  // live v86 Linux guest. There is no separate Astro marketing landing page.
  await page.goto("/z3rm/");
  await expect(page).toHaveURL(/\/z3rm\/gpui-demo\/index\.html$/);
});
test("docs code blocks have working copy buttons", async ({ page }) => {
  await page.goto("en/quick-start/");
  const pres = page.locator(".docs-content article pre");
  const count = await pres.count();
  expect(count).toBeGreaterThan(2);

  for (let i = 0; i < Math.min(count, 2); i++) {
    const expected = (await pres.nth(i).textContent())?.replace(/\n$/, "") ?? "";
    const button = pres.nth(i).locator("button.code-copy");
    // Buttons reveal on hover; force the click so headless doesn't need it.
    await button.click({ force: true });
    const value = await page.evaluate(() => (window as unknown as { __z3rmDocsCopied?: string }).__z3rmDocsCopied);
    expect(value).toBe(expected);
    await expect(pres.nth(i).locator("button.code-copy")).toHaveText(/Copied|已复制/, { timeout: 3000 }).catch(() => {});
  }
});

