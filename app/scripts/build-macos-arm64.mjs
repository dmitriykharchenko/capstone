import { createHash } from "node:crypto";
import {
  existsSync,
  lstatSync,
  mkdirSync,
  readFileSync,
  readdirSync,
  readlinkSync,
  rmSync,
  statSync,
  symlinkSync,
} from "node:fs";
import { basename, dirname, isAbsolute, join, relative, resolve, sep } from "node:path";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

export const APP_ROOT = fileURLToPath(new URL("..", import.meta.url));
export const REPOSITORY_ROOT = resolve(APP_ROOT, "..");
export const TARGET_TRIPLE = "aarch64-apple-darwin";
export const RELEASE_DMG = join(
  APP_ROOT,
  "release",
  "macos-arm64",
  "Research-assistant_macos-arm64.dmg",
);
const BACKEND_ROOT = join(
  APP_ROOT,
  "src-tauri",
  "resources",
  "backend",
  "macos-arm64",
  "capstone-cli",
);
const BUNDLE_ROOT = join(
  APP_ROOT,
  "src-tauri",
  "target",
  TARGET_TRIPLE,
  "release",
  "bundle",
);
const VERIFY_MOUNT = join(BUNDLE_ROOT, "verify-mount");
const REPAIR_ROOT = join(BUNDLE_ROOT, "symlink-repair");
const REQUIRED_LINKS = new Map([
  ["_internal/libmupdf.dylib", "pymupdf/libmupdf.dylib"],
  ["_internal/libmupdfcpp.so", "pymupdf/libmupdfcpp.so"],
]);

export function parseArguments(args) {
  if (args.length === 0) return { help: false };
  if (args.length === 1 && ["-h", "--help"].includes(args[0])) return { help: true };
  throw new Error(`unsupported argument: ${args.join(" ")}`);
}

export function sanitizedBuildEnvironment(environment = process.env) {
  return Object.fromEntries(
    Object.entries(environment).filter(([name]) => {
      const upper = name.toUpperCase();
      return !(
        upper.endsWith("_API_KEY") ||
        upper.endsWith("_ACCESS_TOKEN") ||
        upper.endsWith("_AUTH_TOKEN") ||
        upper.endsWith("_SECRET") ||
        upper === "OPENAI_API_KEY"
      );
    }),
  );
}

export function runStage(label, command, args, options = {}) {
  process.stdout.write(`\n==> ${label}\n`);
  const result = (options.spawn ?? spawnSync)(command, args, {
    cwd: options.cwd ?? APP_ROOT,
    env: options.env ?? sanitizedBuildEnvironment(),
    stdio: options.stdio ?? "inherit",
  });
  if (result.error) throw new Error(`${label} failed: ${result.error.message}`);
  if (result.status !== 0) {
    throw new Error(`${label} failed with exit code ${result.status ?? "unknown"}`);
  }
  return result;
}

export function preflight({ platform = process.platform, arch = process.arch, spawn } = {}) {
  if (platform !== "darwin" || arch !== "arm64") {
    throw new Error("package build requires a native macOS arm64 host");
  }
  for (const [command, args] of [
    ["npm", ["--version"]],
    ["conda", ["--version"]],
    ["cargo", ["--version"]],
    ["hdiutil", ["info"]],
  ]) {
    runStage(`check ${command}`, command, args, { spawn, stdio: "ignore" });
  }
  for (const path of [
    join(REPOSITORY_ROOT, "tools", "build_macos_backend.py"),
    join(APP_ROOT, "src-tauri", "tauri.conf.json"),
    join(APP_ROOT, "package-lock.json"),
  ]) {
    if (!existsSync(path)) throw new Error(`required build input is missing: ${path}`);
  }
  runStage(
    "check ragcourse PyInstaller",
    "conda",
    [
      "run",
      "-n",
      "ragcourse",
      "sh",
      "-c",
      'exec "$CONDA_PREFIX/bin/python" -c "import PyInstaller"',
    ],
    { spawn, stdio: "ignore" },
  );
}

export function resetOwnedDirectory(target, owner) {
  const resolvedTarget = resolve(target);
  const resolvedOwner = resolve(owner);
  if (dirname(resolvedTarget) !== resolvedOwner || resolvedTarget === resolvedOwner) {
    throw new Error(`refusing to clean non-owned path: ${resolvedTarget}`);
  }
  if (existsSync(resolvedTarget) && lstatSync(resolvedTarget).isSymbolicLink()) {
    throw new Error(`refusing to clean symlinked output: ${resolvedTarget}`);
  }
  rmSync(resolvedTarget, { recursive: true, force: true });
  mkdirSync(resolvedTarget, { recursive: true });
}

function walk(root) {
  const paths = [];
  for (const entry of readdirSync(root, { withFileTypes: true })) {
    const path = join(root, entry.name);
    paths.push(path);
    if (entry.isDirectory()) paths.push(...walk(path));
  }
  return paths;
}

export function validateBackend(root) {
  for (const required of ["capstone-cli", "_internal"]) {
    if (!existsSync(join(root, required))) {
      throw new Error(`frozen backend is incomplete: missing ${required}`);
    }
  }
  for (const [name, expectedTarget] of REQUIRED_LINKS) {
    const link = join(root, name);
    if (!existsSync(link) || !lstatSync(link).isSymbolicLink()) {
      throw new Error(`frozen backend link is missing: ${name}`);
    }
    const actualTarget = readlinkSync(link);
    if (actualTarget !== expectedTarget || isAbsolute(actualTarget)) {
      throw new Error(`frozen backend link is not the required relative link: ${name}`);
    }
    const resolvedTarget = resolve(dirname(link), actualTarget);
    if (!resolvedTarget.startsWith(`${resolve(root)}${sep}`) || !existsSync(resolvedTarget)) {
      throw new Error(`frozen backend link escapes or is broken: ${name}`);
    }
  }
}

function fileDigest(path) {
  return createHash("sha256").update(readFileSync(path)).digest("hex");
}

export function restoreBundledLinks(appBundle) {
  const root = join(
    appBundle,
    "Contents",
    "Resources",
    "backend",
    "macos-arm64",
    "capstone-cli",
  );
  for (const [name, expectedTarget] of REQUIRED_LINKS) {
    const link = join(root, name);
    const target = resolve(dirname(link), expectedTarget);
    if (!existsSync(link) || !existsSync(target)) {
      throw new Error(`cannot restore missing bundled library alias: ${name}`);
    }
    if (lstatSync(link).isSymbolicLink()) continue;
    if (fileDigest(link) !== fileDigest(target)) {
      throw new Error(`Tauri copied the wrong bundled library target: ${name}`);
    }
    rmSync(link);
    symlinkSync(expectedTarget, link);
  }
}

export function validateAppBundle(appBundle) {
  const bundledBackend = join(
    appBundle,
    "Contents",
    "Resources",
    "backend",
    "macos-arm64",
    "capstone-cli",
  );
  validateBackend(bundledBackend);
  const prohibited = walk(appBundle).filter((path) => {
    const name = basename(path).toLowerCase();
    return (
      name === ".env" ||
      name === "conda-meta" ||
      name === "researchpapers" ||
      name.endsWith(".pdf") ||
      name.endsWith(".sqlite") ||
      name.endsWith(".sqlite3") ||
      name === "chat_logs.txt" ||
      name === "retrieval_logs.txt"
    );
  });
  if (prohibited.length) {
    throw new Error(`app bundle contains prohibited data: ${prohibited.join(", ")}`);
  }
}

function onlyMatch(directory, suffix) {
  const matches = readdirSync(directory)
    .filter((name) => name.endsWith(suffix))
    .map((name) => join(directory, name));
  if (matches.length !== 1) {
    throw new Error(`expected exactly one ${suffix} in ${directory}; found ${matches.length}`);
  }
  return matches[0];
}

export function build() {
  preflight();
  const environment = {
    ...sanitizedBuildEnvironment(),
    CARGO_NET_OFFLINE: "true",
    CI: "true",
  };
  resetOwnedDirectory(join(BUNDLE_ROOT, "dmg"), BUNDLE_ROOT);
  resetOwnedDirectory(join(BUNDLE_ROOT, "macos"), BUNDLE_ROOT);
  resetOwnedDirectory(VERIFY_MOUNT, BUNDLE_ROOT);
  resetOwnedDirectory(REPAIR_ROOT, BUNDLE_ROOT);
  resetOwnedDirectory(dirname(RELEASE_DMG), join(APP_ROOT, "release"));

  runStage("build frontend", "npm", ["run", "build"], { env: environment });
  runStage(
    "freeze Python backend",
    "conda",
    [
      "run",
      "-n",
      "ragcourse",
      "sh",
      "-c",
      'exec "$CONDA_PREFIX/bin/python" ../tools/build_macos_backend.py',
    ],
    { env: environment },
  );
  validateBackend(BACKEND_ROOT);
  runStage(
    "bundle unsigned macOS arm64 DMG",
    "npm",
    [
      "exec",
      "--offline",
      "--",
      "tauri",
      "build",
      "--target",
      TARGET_TRIPLE,
      "--bundles",
      "dmg",
      "--no-sign",
      "--ci",
      "--config",
      '{"build":{"beforeBuildCommand":null}}',
    ],
    { env: environment },
  );

  const builtDmg = onlyMatch(join(BUNDLE_ROOT, "dmg"), ".dmg");
  runStage("verify generated DMG", "hdiutil", ["verify", builtDmg], {
    env: environment,
  });
  const writableDmg = join(REPAIR_ROOT, "Research-assistant-writable.dmg");
  runStage(
    "create writable DMG staging copy",
    "hdiutil",
    ["convert", builtDmg, "-format", "UDRW", "-o", writableDmg],
    { env: environment },
  );
  let attached = false;
  try {
    runStage(
      "mount DMG staging copy for link repair",
      "hdiutil",
      ["attach", "-readwrite", "-nobrowse", "-mountpoint", VERIFY_MOUNT, writableDmg],
      { env: environment },
    );
    attached = true;
    const appBundle = onlyMatch(VERIFY_MOUNT, ".app");
    restoreBundledLinks(appBundle);
    validateAppBundle(appBundle);
  } finally {
    if (attached) {
      runStage("detach inspected DMG", "hdiutil", ["detach", VERIFY_MOUNT], {
        env: environment,
      });
    }
    rmSync(VERIFY_MOUNT, { recursive: true, force: true });
  }
  runStage(
    "compress repaired DMG",
    "hdiutil",
    ["convert", writableDmg, "-format", "UDZO", "-o", RELEASE_DMG],
    { env: environment },
  );
  rmSync(builtDmg, { force: true });
  rmSync(REPAIR_ROOT, { recursive: true, force: true });
  runStage("verify final DMG", "hdiutil", ["verify", RELEASE_DMG], {
    env: environment,
  });
  resetOwnedDirectory(VERIFY_MOUNT, BUNDLE_ROOT);
  attached = false;
  try {
    runStage(
      "inspect final DMG read-only",
      "hdiutil",
      ["attach", "-readonly", "-nobrowse", "-mountpoint", VERIFY_MOUNT, RELEASE_DMG],
      { env: environment },
    );
    attached = true;
    validateAppBundle(onlyMatch(VERIFY_MOUNT, ".app"));
  } finally {
    if (attached) {
      runStage("detach final DMG", "hdiutil", ["detach", VERIFY_MOUNT], {
        env: environment,
      });
    }
    rmSync(VERIFY_MOUNT, { recursive: true, force: true });
  }
  const digest = createHash("sha256").update(readFileSync(RELEASE_DMG)).digest("hex");
  process.stdout.write(
    `\nPackage ready: ${relative(APP_ROOT, RELEASE_DMG)}\n` +
      `Size: ${statSync(RELEASE_DMG).size} bytes\nSHA-256: ${digest}\n` +
      "Unsigned and unnotarized; intended for local use only.\n",
  );
}

function help() {
  process.stdout.write(
    "Build one unsigned, unnotarized macOS arm64 DMG for local use.\n" +
      "Usage: npm run package:macos-arm64\n",
  );
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  try {
    const args = parseArguments(process.argv.slice(2));
    if (args.help) help();
    else build();
  } catch (error) {
    process.stderr.write(`macOS package build failed: ${error.message}\n`);
    process.exitCode = 1;
  }
}