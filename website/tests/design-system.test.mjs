import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

const root = new URL("../", import.meta.url);

test("Proto UI web component facades register the site controls", async () => {
  const source = await readFile(new URL("proto-ui/components/wc/index.ts", root), "utf8");
  for (const element of ["proto-ui-base-button", "proto-ui-base-tabs-root", "proto-ui-base-select-root", "proto-ui-base-dialog-root", "proto-ui-base-toggle"]) {
    assert.match(source, new RegExp(element));
  }
});

test("the custom theme exposes light and dark GPUI surface tokens", async () => {
  const css = await readFile(new URL("src/styles/tokens.css", root), "utf8");
  for (const token of ["--surface-app", "--surface-panel", "--border-muted", "--accent-blue", "--font-ui", "--font-mono"]) {
    assert.match(css, new RegExp(token));
  }
  assert.match(css, /\[data-theme="dark"\]/);
  assert.doesNotMatch(css, /linear-gradient|radial-gradient|backdrop-filter/);
});
