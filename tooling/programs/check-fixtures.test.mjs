import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { copyFileSync, cpSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";

const source = dirname(fileURLToPath(import.meta.url));

function fixtureProject(t) {
  const root = mkdtempSync(join(tmpdir(), "program-fixture-test-"));
  t.after(() => rmSync(root, { recursive: true, force: true }));
  const programs = join(root, "tooling/programs");
  const fixtures = join(root, "crates/workflow-runtime/tests/fixtures");
  mkdirSync(programs, { recursive: true });
  for (const file of ["check-fixtures.sh", "tsconfig.json", "tsconfig.build.json", "program-sdk.d.ts"]) {
    copyFileSync(join(source, file), join(programs, file));
  }
  cpSync(join(source, "examples"), join(programs, "examples"), { recursive: true });
  cpSync(resolve(source, "../../crates/workflow-runtime/tests/fixtures"), fixtures, { recursive: true });
  return {
    programs,
    fixtures,
    check() {
      return spawnSync("bash", [join(programs, "check-fixtures.sh")], { encoding: "utf8" });
    },
  };
}

test("accepts matching emitted fixture contents and inventory", (t) => {
  const project = fixtureProject(t);
  const result = project.check();
  assert.equal(result.status, 0, result.stdout + result.stderr);
});

test("rejects a new example whose emitted fixture is absent", (t) => {
  const project = fixtureProject(t);
  writeFileSync(join(project.programs, "examples/additional.ts"), "export const additional = true;\n");
  const result = project.check();
  assert.equal(result.status, 1, result.stdout + result.stderr);
  assert.match(result.stdout, /Only in .*: additional\.js/);
});

test("rejects a stale fixture after its example is removed", (t) => {
  const project = fixtureProject(t);
  rmSync(join(project.programs, "examples/session.ts"));
  const result = project.check();
  assert.equal(result.status, 1, result.stdout + result.stderr);
  assert.match(result.stdout, /Only in .*: session\.js/);
});

test("rejects changed fixture contents", (t) => {
  const project = fixtureProject(t);
  writeFileSync(join(project.fixtures, "session.js"), "export default null;\n");
  const result = project.check();
  assert.equal(result.status, 1, result.stdout + result.stderr);
  assert.match(result.stdout, /export default null/);
});
