#!/usr/bin/env node
// Headless-Chrome smoke test for the Indexa web UI.
//
// Zero npm deps — relies on Node >= 22 globals (fetch, WebSocket) and the
// repo's documented CDP harness pattern (--headless=new + DevTools protocol).
// The whole bundle is compile-time-concatenated into one app.js, so a single
// runtime ReferenceError blanks the UI; only *executing* the page catches
// that class of bug — syntax checks and grep cannot (see
// memory/feedback_browser_verification.md).
//
// Env:
//   INDEXA_BIN  path to the indexa binary            (required)
//   CHROME_BIN  path to a Chrome/Chromium binary     (required)
//   PORT        web UI port                          (default 7787)
//
// Flow: throwaway HOME + small fixture tree → `indexa scan` → `indexa serve`
// → headless Chrome → CDP asserts. Smoke-level only: app shell renders,
// tree shows the fixture root, search round-trips, zero console errors.

import { spawn } from 'node:child_process';
import { mkdtempSync, mkdirSync, writeFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { basename, join } from 'node:path';

const INDEXA_BIN = process.env.INDEXA_BIN;
const CHROME_BIN = process.env.CHROME_BIN;
const PORT = Number(process.env.PORT || 7787);
const CDP_PORT = PORT + 1;

if (!INDEXA_BIN || !CHROME_BIN) {
  console.error('FAIL: INDEXA_BIN and CHROME_BIN env vars are required');
  process.exit(1);
}
if (typeof WebSocket === 'undefined') {
  console.error(`FAIL: global WebSocket missing — Node >= 22 required (running ${process.version})`);
  process.exit(1);
}

// ── Result collection ─────────────────────────────────────────────────────────
const results = [];
function check(name, ok, detail) {
  results.push({ name, ok, detail });
  console.log(`${ok ? 'PASS' : 'FAIL'}: ${name}${detail ? ` — ${detail}` : ''}`);
}

// ── Throwaway HOME + fixture tree ─────────────────────────────────────────────
// Isolated HOME keeps the real index/config untouched: indexa resolves its
// data dir via the `directories` crate, which follows $HOME on macOS and
// $XDG_* on Linux — set all of them under the sandbox.
const sandbox = mkdtempSync(join(tmpdir(), 'indexa-smoke-'));
const home = join(sandbox, 'home');
const fixture = join(sandbox, 'smoke-fixture');
mkdirSync(join(home, '.config'), { recursive: true });
mkdirSync(join(home, '.local', 'share'), { recursive: true });
mkdirSync(join(fixture, 'notes'), { recursive: true });
writeFileSync(join(fixture, 'alpha.txt'), 'The quick brown fox jumps over the lazy dog.\n');
writeFileSync(join(fixture, 'beta.txt'), 'Beta release notes: nothing of consequence.\n');
writeFileSync(join(fixture, 'notes', 'gamma.md'), '# Gamma\n\nA small markdown fixture file.\n');

const childEnv = {
  ...process.env,
  HOME: home,
  XDG_CONFIG_HOME: join(home, '.config'),
  XDG_DATA_HOME: join(home, '.local', 'share'),
};

// ── Child-process bookkeeping ─────────────────────────────────────────────────
const children = [];
function spawnChild(cmd, args, opts = {}) {
  const child = spawn(cmd, args, { stdio: ['ignore', 'pipe', 'pipe'], ...opts });
  children.push(child);
  return child;
}

function killChildren() {
  for (const c of children) {
    if (c.exitCode === null && c.signalCode === null) c.kill('SIGTERM');
  }
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// Hard watchdog: a wedged renderer or lost CDP response must never hang CI —
// fail loudly and tear everything down. (Locally observed: Chrome navigation
// can stall indefinitely with the wrong flag set; see launch flags below.)
const WATCHDOG_MS = 90_000;
const watchdog = setTimeout(() => {
  console.error(`FAIL: watchdog — smoke test exceeded ${WATCHDOG_MS / 1000}s, killing children`);
  killChildren();
  setTimeout(() => process.exit(1), 1_000);
}, WATCHDOG_MS);
watchdog.unref?.();


// Poll `fn` every `interval` ms until it returns a truthy value or `timeout`
// ms elapse. No fixed long sleeps — total runtime stays bounded by readiness.
async function poll(fn, { timeout = 20_000, interval = 250, label = 'condition' } = {}) {
  const deadline = Date.now() + timeout;
  let lastErr;
  while (Date.now() < deadline) {
    try {
      const v = await fn();
      if (v) return v;
    } catch (e) {
      lastErr = e;
    }
    await sleep(interval);
  }
  throw new Error(`timed out waiting for ${label}${lastErr ? `: ${lastErr.message}` : ''}`);
}

// ── Minimal CDP client over the global WebSocket ──────────────────────────────
class Cdp {
  constructor(ws) {
    this.ws = ws;
    this.nextId = 1;
    this.pending = new Map();
    this.listeners = new Map();
    ws.addEventListener('message', (ev) => {
      const msg = JSON.parse(ev.data);
      if (msg.id !== undefined && this.pending.has(msg.id)) {
        const { resolve, reject } = this.pending.get(msg.id);
        this.pending.delete(msg.id);
        msg.error ? reject(new Error(msg.error.message)) : resolve(msg.result);
      } else if (msg.method) {
        for (const fn of this.listeners.get(msg.method) || []) fn(msg.params);
      }
    });
  }
  static connect(url) {
    return new Promise((resolve, reject) => {
      const ws = new WebSocket(url);
      ws.addEventListener('open', () => resolve(new Cdp(ws)));
      ws.addEventListener('error', () => reject(new Error(`WebSocket failed: ${url}`)));
    });
  }
  send(method, params = {}) {
    return new Promise((resolve, reject) => {
      const id = this.nextId++;
      // Per-command deadline: a blocked renderer silently swallows commands
      // (no error frame ever comes back) — surface that as a named failure.
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error(`CDP ${method} got no response in 15s (renderer hung?)`));
      }, 15_000);
      this.pending.set(id, {
        resolve: (v) => {
          clearTimeout(timer);
          resolve(v);
        },
        reject: (e) => {
          clearTimeout(timer);
          reject(e);
        },
      });
      this.ws.send(JSON.stringify({ id, method, params }));
    });
  }
  on(method, fn) {
    if (!this.listeners.has(method)) this.listeners.set(method, []);
    this.listeners.get(method).push(fn);
  }
  // Evaluate an expression in the page; returns the JSON-decoded value.
  async eval(expression, { awaitPromise = false } = {}) {
    const { result, exceptionDetails } = await this.send('Runtime.evaluate', {
      expression,
      returnByValue: true,
      awaitPromise,
    });
    if (exceptionDetails) {
      throw new Error(`page eval threw: ${exceptionDetails.text} ${result?.description || ''}`);
    }
    return result.value;
  }
}

// Refuse to start against leftovers: a stale `indexa serve` on PORT would make
// readiness lie about OUR server, and a stale Chrome on CDP_PORT would make the
// harness attach to the wrong (possibly wedged) browser.
async function assertPortFree(port, what) {
  const inUse = await fetch(`http://127.0.0.1:${port}/`, { signal: AbortSignal.timeout(1_000) })
    .then(() => true)
    .catch(() => false);
  if (inUse) {
    throw new Error(`port ${port} already in use — stale ${what} from a previous run? Kill it first.`);
  }
}

// ── Main ──────────────────────────────────────────────────────────────────────
let exitCode = 1;
try {
  await assertPortFree(PORT, 'indexa serve');
  await assertPortFree(CDP_PORT, 'Chrome CDP endpoint');

  // 1. Scan the fixture into the throwaway index. Scan is the walk/classify
  //    stage — no Ollama required (deep/summarize are, and are not run here).
  await new Promise((resolve, reject) => {
    const scan = spawnChild(INDEXA_BIN, ['scan', fixture], { env: childEnv });
    let err = '';
    scan.stderr.on('data', (d) => (err += d));
    const timer = setTimeout(() => {
      scan.kill('SIGKILL');
      reject(new Error('scan timed out after 60s'));
    }, 60_000);
    scan.on('exit', (code) => {
      clearTimeout(timer);
      code === 0 ? resolve() : reject(new Error(`scan exited ${code}: ${err.slice(-500)}`));
    });
  });

  // 2. Serve the web UI; readiness = /api/stats answering 200.
  const serve = spawnChild(INDEXA_BIN, ['serve', '--port', String(PORT)], { env: childEnv });
  serve.on('exit', (code) => {
    // Dying before we kill it is a hard failure — surface it via the poll.
    serve.unexpectedExit = code;
  });
  const base = `http://127.0.0.1:${PORT}`;
  await poll(
    async () => {
      if (serve.unexpectedExit !== undefined) {
        throw new Error(`serve exited early (${serve.unexpectedExit})`);
      }
      const r = await fetch(`${base}/api/stats`);
      return r.status === 200;
    },
    { label: '/api/stats readiness' },
  );

  // 3. Headless Chrome with a CDP endpoint. Flag notes:
  //    - --remote-allow-origins=* avoids Chrome 111+ WebSocket origin rejection.
  //    - NO --disable-gpu: on macOS (Chrome 149) it wedged the compositor —
  //      navigations started but never committed and Runtime.evaluate never
  //      answered. The documented working harness uses the minimal flag set.
  //    - --no-sandbox only on Linux, where containerized CI lacks the
  //      user-namespace setup Chrome's sandbox needs.
  //    Chrome must run with the REAL environment: on macOS an isolated $HOME
  //    wedges it (navigations start but never commit — reproduced on Chrome
  //    149). Its isolation is --user-data-dir, not $HOME.
  spawnChild(CHROME_BIN, [
    '--headless=new',
    `--remote-debugging-port=${CDP_PORT}`,
    `--user-data-dir=${join(sandbox, 'chrome-profile')}`,
    '--remote-allow-origins=*',
    '--no-first-run',
    '--no-default-browser-check',
    '--mute-audio',
    ...(process.platform === 'linux' ? ['--no-sandbox'] : []),
    'about:blank',
  ]);
  const targets = await poll(
    async () => {
      const r = await fetch(`http://127.0.0.1:${CDP_PORT}/json/list`);
      const list = await r.json();
      return list.find((t) => t.type === 'page') ? list : null;
    },
    { label: 'Chrome CDP endpoint' },
  );
  const page = targets.find((t) => t.type === 'page');
  const cdp = await Cdp.connect(page.webSocketDebuggerUrl);

  // Subscribe to console errors BEFORE navigating so load-time errors count.
  const consoleErrors = [];
  cdp.on('Runtime.consoleAPICalled', (p) => {
    if (p.type === 'error') {
      consoleErrors.push(p.args.map((a) => a.value ?? a.description ?? '').join(' '));
    }
  });
  cdp.on('Runtime.exceptionThrown', (p) => {
    consoleErrors.push(`uncaught: ${p.exceptionDetails?.text} ${p.exceptionDetails?.exception?.description || ''}`);
  });
  cdp.on('Log.entryAdded', (p) => {
    // A lone /favicon.ico 404 is the only known-harmless console error
    // (documented pre-existing); everything else fails the run.
    if (p.entry.level === 'error' && !/favicon\.ico/.test(p.entry.url || '')) {
      consoleErrors.push(`[${p.entry.source}] ${p.entry.text} ${p.entry.url || ''}`);
    }
  });
  await cdp.send('Page.enable');
  await cdp.send('Runtime.enable');
  await cdp.send('Log.enable');

  // Cancellable timeout, and a pre-attached catch guard: if Page.navigate
  // itself hangs, `loaded` has no awaiter yet — without the guard its timeout
  // rejection would crash Node as an unhandled rejection instead of failing
  // through the try/catch.
  const loaded = new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error('load event timeout (15s)')), 15_000);
    cdp.on('Page.loadEventFired', () => {
      clearTimeout(timer);
      resolve();
    });
  });
  loaded.catch(() => {});
  await cdp.send('Page.navigate', { url: `${base}/` });
  await loaded;

  // Check 1 — app shell loads: title + #stats exists.
  const title = await cdp.eval('document.title');
  check('page loads with app shell (title "Indexa", #stats present)',
    title === 'Indexa' && (await cdp.eval('!!document.getElementById("stats")')),
    `title=${JSON.stringify(title)}`);

  // Check 2 — #stats renders real text (loadStats replaced the "Loading…" placeholder).
  const statsText = await poll(
    async () => {
      const t = await cdp.eval('document.getElementById("stats").textContent');
      return t && !/Loading/.test(t) ? t : null;
    },
    { label: '#stats text render', timeout: 10_000 },
  ).catch(() => null);
  check('#stats element renders index stats text',
    statsText !== null && /files/.test(statsText),
    `stats=${JSON.stringify(statsText)}`);

  // Check 3 — tree tab shows the fixture root. /api/roots registers the
  // PARENT of the scanned path, so the root row's label is the sandbox dir
  // name (indexa-smoke-XXXXXX), not "smoke-fixture".
  const rootName = basename(sandbox);
  const treeText = await poll(
    async () => {
      const t = await cdp.eval('document.getElementById("tree-list").textContent');
      return t && t.includes(rootName) ? t : null;
    },
    { label: 'fixture root in tree', timeout: 10_000 },
  ).catch(() => null);
  check('tree sidebar shows the fixture root', treeText !== null,
    treeText ? `root "${rootName}" visible` : `root "${rootName}" not found in #tree-list`);

  // Check 4 — search round-trips from inside the page: /api/search?q=alpha
  // is the sidebar path-typeahead (substring LIKE), so the fixture's
  // alpha.txt must come back.
  const search = await cdp.eval(
    `fetch('/api/search?q=alpha').then(async r => ({
       status: r.status,
       body: await r.json(),
     }))`,
    { awaitPromise: true },
  );
  check('search round-trip (/api/search?q=alpha → 200, hit returned)',
    search.status === 200 && Array.isArray(search.body) &&
      search.body.some((n) => String(n.path || '').includes('alpha.txt')),
    `status=${search.status} hits=${Array.isArray(search.body) ? search.body.length : 'n/a'}`);

  // Give late async fetches (update check, telemetry stream) a beat to land
  // before tallying console errors — bounded, not load-bearing.
  await sleep(500);

  // Check 5 — zero console errors across the whole session.
  check('no console errors during session', consoleErrors.length === 0,
    consoleErrors.length ? consoleErrors.slice(0, 5).join(' | ') : undefined);

  cdp.ws.close();

  // Check 6 — clean shutdown: both children die on SIGTERM.
  killChildren();
  const allDead = await poll(
    () => children.every((c) => c.exitCode !== null || c.signalCode !== null),
    { label: 'children exit', timeout: 10_000, interval: 100 },
  ).catch(() => false);
  check('clean exit: serve + chrome terminated', !!allDead);

  exitCode = results.every((r) => r.ok) ? 0 : 1;
} catch (e) {
  console.error(`FAIL: harness error — ${e.message}`);
  exitCode = 1;
} finally {
  killChildren();
  // SIGKILL stragglers before exiting so CI never leaks an orphaned Chrome —
  // an unref'd timer would never fire past the process.exit below.
  await poll(() => children.every((c) => c.exitCode !== null || c.signalCode !== null), {
    timeout: 3_000,
    interval: 100,
    label: 'children to exit',
  }).catch(() => {
    for (const c of children) {
      if (c.exitCode === null && c.signalCode === null) c.kill('SIGKILL');
    }
  });
  try {
    rmSync(sandbox, { recursive: true, force: true });
  } catch {
    /* best effort */
  }
}

console.log(`\n${results.filter((r) => r.ok).length}/${results.length} checks passed`);
process.exit(exitCode);
