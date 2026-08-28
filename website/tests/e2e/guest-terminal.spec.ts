import { expect, test } from "@playwright/test";
import { stat } from "node:fs/promises";

const VIEWPORT = { width: 780, height: 437 };
const GUEST_BOOT_TIMEOUT = 120_000;

// The real guest is intentionally exercised only in the desktop Chromium
// project. The mobile project still covers the surrounding site; booting a
// second v86 instance there adds time without testing another code path.
test("real guest renders Kitty media and handles terminal actions", async ({ page }, testInfo) => {
  test.skip(testInfo.project.name !== "chromium", "the v86 interaction check runs in desktop Chromium");
  test.setTimeout(180_000);

  await page.setViewportSize(VIEWPORT);
  const appOrigin = new URL(
    testInfo.project.use.baseURL ?? "http://127.0.0.1:4331/z3rm/",
  ).origin;
  await page.context().grantPermissions(["clipboard-read", "clipboard-write"], {
    origin: appOrigin,
  });
  await page.goto("gpui-demo/index.html");
  await expect(page.locator("#loading-progress")).toBeVisible({ timeout: 10_000 });

  const html = page.locator("html");
  await expect(html).toHaveAttribute("data-gpui-ready", "true", {
    timeout: GUEST_BOOT_TIMEOUT,
  });
  await expect(html).toHaveAttribute("data-first-pane-snapshot-ready", "true", {
    timeout: GUEST_BOOT_TIMEOUT,
  });
  await expect(page.locator("canvas")).toBeVisible();
  await expect(page.locator("#loading-progress")).toHaveCSS("visibility", "hidden");
  await page.waitForTimeout(5_000);
  await expect(page.locator("#boot-terminal-output")).toContainText("/mnt/start-mux.sh");

  const canvas = page.locator("canvas");
  const box = await canvas.boundingBox();
  expect(box).not.toBeNull();
  const canvasBox = box!;

  await page.mouse.click(canvasBox.x + 700, canvasBox.y + 100);
  await page.mouse.move(canvasBox.x + 390, canvasBox.y + 220);
  await page.mouse.wheel(0, 40);
  await page.waitForTimeout(1_500);

  let download = null;
  let downloadY = 0;
  const downloadEvent = page
    .waitForEvent("download", { timeout: 60_000 })
    .catch(() => null);
  for (const candidateY of Array.from({ length: 34 }, (_, index) => 80 + index * 10)) {
    await page.mouse.click(canvasBox.x + 120, canvasBox.y + candidateY);
    const candidate = await Promise.race([
      downloadEvent,
      page.waitForTimeout(300).then(() => null),
    ]);
    if (candidate) {
      download = candidate;
      downloadY = candidateY;
      break;
    }
  }
  if (!download) download = await downloadEvent;
  expect(download).not.toBeNull();
  expect(download!.suggestedFilename()).toBe("z3rm-server");
  expect(download!.url()).toMatch(/\/v86\/z3rm-server\.bin$/);
  const downloadPath = await download!.path();
  expect(downloadPath).not.toBeNull();
  const downloadStats = await stat(downloadPath!);
  expect(downloadStats.size).toBeGreaterThan(3_000_000);

  let copiedText = "";
  for (const candidateY of [downloadY, ...Array.from({ length: 34 }, (_, index) => 80 + index * 10)]) {
    await page.mouse.click(canvasBox.x + 430, canvasBox.y + candidateY);
    copiedText = await page.evaluate(() =>
      Promise.race([
        navigator.clipboard?.readText() ?? Promise.resolve(""),
        new Promise<string>((resolve) => setTimeout(() => resolve(""), 500)),
      ]),
    );
    if (copiedText === "cargo install z3rm") break;
  }
  expect(copiedText).toBe("cargo install z3rm");

});

test("loading surface reports errors without claiming completion", async ({ page }, testInfo) => {
  test.skip(testInfo.project.name !== "chromium", "the loading contract check runs in desktop Chromium");
  await page.goto("gpui-demo/index.html");
  await expect(page.locator("#loading-progress")).toBeAttached();
  await page.evaluate(() => {
    const progress = (window as Window & {
      __z3rm_progress?: { error?: (stage: string, message: string) => void };
    }).__z3rm_progress;
    progress?.error?.("test resource", "network refused");
  });
  await expect(page.locator("#loading-progress")).toHaveAttribute("data-state", "error");
  await expect(page.locator("#loading-progress-label")).toContainText("Unable to load");
  await expect(page.locator("#loading-progress-detail")).toContainText("network refused");
  await expect(page.locator("#loading-progress-retry")).toBeVisible();
  await expect(page.locator("#loading-progress-bar")).not.toHaveAttribute("aria-valuenow");
});
