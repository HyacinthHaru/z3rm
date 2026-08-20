import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

const root = new URL("../../", import.meta.url);

test("foundation matrix records evidence for every claim", async () => {
  const matrix = JSON.parse(await readFile(new URL("docs/implementation-status/z3rm-foundation.json", root), "utf8"));
  assert.equal(matrix.schemaVersion, 1);
  assert.ok(matrix.requirements.length >= 12);
  for (const requirement of matrix.requirements) {
    assert.match(requirement.id, /^Z3RM-/);
    assert.ok(requirement.spec.length > 0);
    assert.ok(requirement.claim.en.length > 0 && requirement.claim.zh.length > 0);
    assert.ok(requirement.implementation.length > 0);
    assert.ok(requirement.verification.length > 0);
    if (["missing", "divergent"].includes(requirement.status)) assert.match(requirement.issue, /^https:\/\/github\.com\/cyjin-yl\/z3rm\/issues\/\d+$/);
  }
});

test("landing claims reference verified requirement ids", async () => {
  const matrix = JSON.parse(await readFile(new URL("docs/implementation-status/z3rm-foundation.json", root), "utf8"));
  const verified = new Set(matrix.requirements.filter((item) => item.status === "verified").map((item) => item.id));
  assert.ok(verified.has("Z3RM-MUX-001"));
  assert.ok(verified.has("Z3RM-CLI-001"));
  assert.ok(verified.has("Z3RM-SNAPSHOT-001"));
});
