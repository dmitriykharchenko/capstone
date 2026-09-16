import assert from "node:assert/strict";
import { existsSync, mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { spawn } from "node:child_process";
import { once } from "node:events";
import test, { after, before } from "node:test";
import { createServer } from "vite";

const appRoot = fileURLToPath(new URL("..", import.meta.url));
const chromeCandidates = [
  process.env.CHROME_BIN,
  "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
  "/usr/bin/google-chrome",
  "/usr/bin/chromium",
  "/usr/bin/chromium-browser",
].filter(Boolean);
const chromePath = chromeCandidates.find(existsSync);
let browser;
let client;
let profile;
let vite;

function waitForDebugger(process) {
  return new Promise((resolve, reject) => {
    let output = "";
    const timeout = setTimeout(() => reject(new Error(`Chrome did not start: ${output}`)), 10_000);
    process.stderr.on("data", (chunk) => {
      output += chunk;
      const match = output.match(/DevTools listening on (ws:\/\/[^\s]+)/);
      if (match) {
        clearTimeout(timeout);
        resolve(match[1]);
      }
    });
    process.once("exit", (code) => reject(new Error(`Chrome exited before startup (${code}): ${output}`)));
  });
}

async function connect(url) {
  const socket = new WebSocket(url);
  await new Promise((resolve, reject) => {
    socket.addEventListener("open", resolve, { once: true });
    socket.addEventListener("error", reject, { once: true });
  });
  let id = 0;
  const pending = new Map();
  socket.addEventListener("message", ({ data }) => {
    const message = JSON.parse(data);
    if (message.id && pending.has(message.id)) {
      const { resolve, reject } = pending.get(message.id);
      pending.delete(message.id);
      message.error ? reject(new Error(message.error.message)) : resolve(message.result);
    }
  });
  return {
    close: () => socket.close(),
    send(method, params = {}) {
      const requestId = ++id;
      socket.send(JSON.stringify({ id: requestId, method, params }));
      return new Promise((resolve, reject) => pending.set(requestId, { resolve, reject }));
    },
  };
}

async function evaluate(expression) {
  const result = await client.send("Runtime.evaluate", { expression, returnByValue: true, awaitPromise: true });
  if (result.exceptionDetails) throw new Error(result.exceptionDetails.text);
  return result.result.value;
}

async function waitFor(expression, timeoutMs = 10_000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const value = await evaluate(expression);
    if (value) return value;
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
  const page = await evaluate(`({ url: location.href, text: document.body.innerText, html: document.body.innerHTML })`);
  throw new Error(`Timed out waiting for mounted graph: ${expression}\n${JSON.stringify(page)}`);
}

before(async () => {
  assert.ok(chromePath, "Chrome or Chromium is required for the mounted Svelte Flow regression");
  vite = await createServer({
    root: appRoot,
    appType: "mpa",
    logLevel: "silent",
    server: { host: "127.0.0.1", port: 0 },
  });
  await vite.listen();
  const address = vite.httpServer.address();
  profile = mkdtempSync(join(tmpdir(), "capstone-graph-chrome-"));
  browser = spawn(chromePath, [
    "--headless=new",
    "--disable-gpu",
    "--no-first-run",
    "--no-default-browser-check",
    `--user-data-dir=${profile}`,
    "--remote-debugging-port=0",
    "about:blank",
  ], { stdio: ["ignore", "ignore", "pipe"] });
  const debuggerUrl = new URL(await waitForDebugger(browser));
  const fixture = `http://127.0.0.1:${address.port}/tests/fixtures/graph-flow-mounted.html`;
  const page = await fetch(`http://127.0.0.1:${debuggerUrl.port}/json/new?${encodeURIComponent(fixture)}`, {
    method: "PUT",
  }).then((response) => response.json());
  client = await connect(page.webSocketDebuggerUrl);
  await client.send("Runtime.enable");
});

after(async () => {
  client?.close();
  if (browser && browser.exitCode === null) {
    browser.kill("SIGTERM");
    await once(browser, "exit");
  }
  await vite?.close();
  if (profile) rmSync(profile, { recursive: true, force: true, maxRetries: 3, retryDelay: 100 });
});

test("mounted Svelte Flow renders and keeps both canonical edges attached", async () => {
  const initial = await waitFor(`(() => {
    const paths = [...document.querySelectorAll('.svelte-flow__edge-path')];
    if (paths.length !== 2 || paths.some((path) => !path.getAttribute('d'))) return null;
    return {
      paths: Object.fromEntries(paths.map((path) => [path.closest('.svelte-flow__edge').dataset.id, path.getAttribute('d')])),
      labels: [...document.querySelectorAll('.svelte-flow__edge-label')].map((label) => label.textContent.trim()).sort(),
      handles: [...document.querySelectorAll('.svelte-flow__handle')].map((handle) => ({
        id: handle.dataset.handleid,
        node: handle.dataset.nodeid,
        connectable: handle.classList.contains('connectable'),
      })),
      transform: document.querySelector('.svelte-flow__viewport').style.transform,
    };
  })()`);
  assert.deepEqual(initial.labels, ["authored by", "has topic"]);
  assert.deepEqual(initial.handles.map(({ id, node }) => [id, node]).sort(), [
    ["authored_by", "author-a"], ["authored_by", "paper-a"],
    ["has_topic", "paper-a"], ["has_topic", "topic-a"],
  ]);
  assert.ok(initial.handles.every((handle) => !handle.connectable));

  await evaluate(`document.querySelector('button[aria-label="Fit View"]').click()`);
  const fittedTransform = await waitFor(`document.querySelector('.svelte-flow__viewport').style.transform !== ${JSON.stringify(initial.transform)} && document.querySelector('.svelte-flow__viewport').style.transform`);
  assert.notEqual(fittedTransform, initial.transform);

  await evaluate(`document.querySelector('#move-topic').click()`);
  const movedPaths = await waitFor(`(() => {
    const paths = Object.fromEntries([...document.querySelectorAll('.svelte-flow__edge-path')].map((path) => [path.closest('.svelte-flow__edge').dataset.id, path.getAttribute('d')]));
    return paths['has_topic:paper-a:topic-a'] !== ${JSON.stringify(initial.paths["has_topic:paper-a:topic-a"])} && paths;
  })()`);
  assert.equal(movedPaths["authored_by:paper-a:author-a"], initial.paths["authored_by:paper-a:author-a"]);
  assert.notEqual(movedPaths["has_topic:paper-a:topic-a"], initial.paths["has_topic:paper-a:topic-a"]);
});