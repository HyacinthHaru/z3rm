import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

const root = new URL("../", import.meta.url);

test("status component renders every matrix state and issue link", async () => {
  const source = await readFile(new URL("src/components/StatusMatrix.astro", root), "utf8");
  assert.match(source, /z3rm-foundation\.json/);
  assert.match(source, /requirement\.status/);
  assert.match(source, /requirement\.issue/);
});

test("localized dynamic route includes the status matrix", async () => {
  const source = await readFile(new URL("src/pages/[lang]/[...slug].astro", root), "utf8");
  assert.match(source, /StatusMatrix/);
  assert.match(source, /implementation-status/);
});
