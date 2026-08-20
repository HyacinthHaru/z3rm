import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

const root = new URL("../", import.meta.url);
const screenshots = [
  "z3rm-terminal-grid.png",
  "z3rm-extension-status-bar.png",
  "z3rm-notification-severity.png",
];

test("published screenshots are real PNG captures", async () => {
  for (const name of screenshots) {
    const bytes = await readFile(new URL(`public/media/${name}`, root));
    assert.deepEqual([...bytes.subarray(0, 8)], [137, 80, 78, 71, 13, 10, 26, 10], name);
    assert.ok(bytes.length > 1_000, `${name} is unexpectedly small`);
  }
});

test("architecture diagrams carry accessible descriptions", async () => {
  for (const name of ["server-canonical-architecture.svg", "session-lifecycle.svg"]) {
    const source = await readFile(new URL(`public/media/${name}`, root), "utf8");
    assert.match(source, /role="img"/);
    assert.match(source, /<title/);
    assert.match(source, /<desc/);
  }
});
