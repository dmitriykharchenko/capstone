import assert from "node:assert/strict";
import {
  lstatSync,
  mkdtempSync,
  mkdirSync,
  readFileSync,
  readlinkSync,
  symlinkSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

import {
  parseArguments,
  preflight,
  resetOwnedDirectory,
  restoreBundledLinks,
  runStage,
  sanitizedBuildEnvironment,
} from "../scripts/build-macos-arm64.mjs";

test("package arguments accept only help or the default build", () => {
  assert.deepEqual(parseArguments([]), { help: false });
  assert.deepEqual(parseArguments(["--help"]), { help: true });
  assert.throws(() => parseArguments(["--release"]), /unsupported argument/);
});

test("preflight rejects unsupported hosts before executing commands", () => {
  let calls = 0;
  assert.throws(
    () => preflight({ platform: "linux", arch: "arm64", spawn: () => calls++ }),
    /native macOS arm64/,
  );
  assert.equal(calls, 0);
});

test("preflight reports a missing prerequisite stage", () => {
  const spawn = (command) =>
    command === "conda" ? { status: 127 } : { status: 0 };
  assert.throws(
    () => preflight({ platform: "darwin", arch: "arm64", spawn }),
    /check conda failed with exit code 127/,
  );
});

test("preflight uses a read-only hdiutil availability probe", () => {
  const calls = [];
  preflight({
    platform: "darwin",
    arch: "arm64",
    spawn: (command, args) => {
      calls.push([command, args]);
      return { status: 0 };
    },
  });
  assert.ok(
    calls.some(([command, args]) => command === "hdiutil" && args[0] === "info"),
  );
});

test("stage failures identify the stage and exit code", () => {
  assert.throws(
    () => runStage("frontend", "npm", ["run", "build"], {
      spawn: () => ({ status: 9 }),
      stdio: "ignore",
    }),
    /frontend failed with exit code 9/,
  );
});

test("cleanup is constrained to a direct owned output directory", () => {
  const owner = mkdtempSync(join(tmpdir(), "capstone-package-test-"));
  const output = join(owner, "output");
  resetOwnedDirectory(output, owner);
  assert.throws(() => resetOwnedDirectory(owner, owner), /non-owned path/);
  assert.throws(() => resetOwnedDirectory(join(owner, "nested", "output"), owner), /non-owned/);
  const elsewhere = mkdtempSync(join(tmpdir(), "capstone-package-other-"));
  const link = join(owner, "linked-output");
  mkdirSync(elsewhere, { recursive: true });
  symlinkSync(elsewhere, link);
  assert.throws(() => resetOwnedDirectory(link, owner), /symlinked output/);
});

test("provider credentials are removed from build subprocesses", () => {
  const environment = sanitizedBuildEnvironment({
    PATH: "/bin",
    OPENAI_API_KEY: "redacted",
    OTHER_ACCESS_TOKEN: "redacted",
  });
  assert.deepEqual(environment, { PATH: "/bin" });
});

test("package script inspects the generated DMG read-only", () => {
  const script = new URL("../scripts/build-macos-arm64.mjs", import.meta.url);
  const source = readFileSync(script, "utf8");
  assert.match(source, /"attach", "-readonly", "-nobrowse"/);
  assert.match(source, /validateAppBundle\(onlyMatch\(VERIFY_MOUNT, "\.app"\)\)/);
});

test("Tauri's copied MuPDF aliases are restored as relative links", () => {
  const app = mkdtempSync(join(tmpdir(), "capstone-package-app-"));
  const internal = join(
    app,
    "Contents/Resources/backend/macos-arm64/capstone-cli/_internal",
  );
  mkdirSync(join(internal, "pymupdf"), { recursive: true });
  for (const [name, target] of [
    ["libmupdf.dylib", "pymupdf/libmupdf.dylib"],
    ["libmupdfcpp.so", "pymupdf/libmupdfcpp.so"],
  ]) {
    writeFileSync(join(internal, target), name);
    writeFileSync(join(internal, name), name);
  }
  restoreBundledLinks(app);
  for (const [name, target] of [
    ["libmupdf.dylib", "pymupdf/libmupdf.dylib"],
    ["libmupdfcpp.so", "pymupdf/libmupdfcpp.so"],
  ]) {
    const link = join(internal, name);
    assert.equal(lstatSync(link).isSymbolicLink(), true);
    assert.equal(readlinkSync(link), target);
  }
});