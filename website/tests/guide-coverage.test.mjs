import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

const root = new URL("../", import.meta.url);
const routes = [
  "features", "quick-start", "guide/cli", "guide/gui", "guide/for-humans", "guide/for-agents",
  "concepts/sessions-and-panes", "concepts/server-canonical-model", "concepts/local-and-remote", "concepts/shadow-snapshots",
  "reference/cli", "reference/keybindings", "reference/configuration", "reference/extension-runtime",
  "troubleshooting", "implementation-status",
];

test("both locales cover every public guide route", async () => {
  for (const locale of ["en", "zh"]) {
    for (const route of routes) {
      const source = await readFile(new URL(`src/content/docs/${locale}/${route}.md`, root), "utf8");
      assert.match(source, /translationKey:/);
      assert.match(source, /^# /m);
    }
  }
});

test("agent guide documents explicit targeting and observation", async () => {
  const source = await readFile(new URL("src/content/docs/en/guide/for-agents.md", root), "utf8");
  assert.match(source, /send-keys/);
  assert.match(source, /capture-pane/);
  assert.match(source, /explicit target/i);
  assert.match(source, /exit status/i);
});

test("CLI reference documents the current command families", async () => {
  const source = await readFile(new URL("src/content/docs/en/reference/cli.md", root), "utf8");
  for (const command of ["split-window", "search-scrollback", "list-changes", "show-buffer", "attach --ssh"]) assert.match(source, new RegExp(command));
});
