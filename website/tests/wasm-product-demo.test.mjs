import assert from "node:assert/strict";
import { readFile, readdir } from "node:fs/promises";
import test from "node:test";

const root = new URL("../", import.meta.url);

test("WASM build emits a loadable GPUI product bundle", async () => {
  const bundleRoot = new URL("public/gpui-demo/", root);
  const html = await readFile(new URL("index.html", bundleRoot), "utf8");
  const files = await readdir(bundleRoot);
  const wasmName = files.find((file) => file.endsWith(".wasm"));
  assert.ok(wasmName, "compiled WebAssembly module is present");
  assert.match(html, new RegExp(wasmName.replaceAll(".", "\\.")));
  const wasm = await readFile(new URL(wasmName, bundleRoot));
  assert.deepEqual([...wasm.subarray(0, 4)], [0x00, 0x61, 0x73, 0x6d]);
  assert.ok(wasm.byteLength > 1_000_000, "bundle contains the GPUI renderer, not an empty shim");
});

