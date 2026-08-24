import { expect, test } from "@playwright/test";

// The guest is the slowest thing on the page by a wide margin: a cold boot of
// the buildroot image runs 15-20s locally and longer on a shared CI runner.
const BOOT_TIMEOUT_MS = 60_000;

test.describe("v86 guest", () => {
  test("boots to a shell and round-trips a command over serial", async ({ page }) => {
    test.setTimeout(BOOT_TIMEOUT_MS * 2);

    const consoleErrors: string[] = [];
    page.on("console", (message) => {
      if (message.type() === "error") {
        consoleErrors.push(message.text());
      }
    });

    await page.goto("v86/smoke.html");

    const serial = page.locator("#serial");
    // The image's own login banner, printed by userspace: waiting on it proves
    // init ran rather than just the kernel.
    await expect(serial).toContainText("Files send via emulator appear in /mnt/", {
      timeout: BOOT_TIMEOUT_MS,
    });
    // Busybox ash in this image prompts with `~%`.
    await expect(serial).toContainText("~%", { timeout: BOOT_TIMEOUT_MS });

    // Arithmetic the page cannot have supplied: the guest shell has to have
    // evaluated it for the marker to come back.
    await page.evaluate(() =>
      (window as unknown as { __z3rm_smoke: { send(text: string): void } }).__z3rm_smoke.send(
        "echo MARKER_$((40+2))\n",
      ),
    );
    await expect(serial).toContainText("MARKER_42", { timeout: BOOT_TIMEOUT_MS });

    expect(consoleErrors).toEqual([]);
  });
});
