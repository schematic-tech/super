import assert from 'node:assert/strict';
import { EventEmitter } from 'node:events';
import { request as httpRequest } from 'node:http';
import test from 'node:test';
import { Dashboard, launchBrowser } from '../lib/dashboard.mjs';
import { fixture, clientFor } from './helpers.mjs';
import { checksFrom, category } from '../web/src/lib/history.js';

async function setup(t, options = {}) {
  const fixtureData = fixture(t);
  const dashboard = new Dashboard(fixtureData.bridge, options);
  t.after(() => dashboard.close());
  const url = new URL(await dashboard.start());
  const headers = {
    authorization: `Bearer ${new URLSearchParams(url.hash.slice(1)).get('token')}`,
  };
  return { ...fixtureData, dashboard, url, headers };
}

test('dashboard serves only authenticated, read-only worktree history and exact IDs', async (t) => {
  const s = await setup(t);
  const base = s.url.origin;
  assert.equal((await fetch(base)).status, 200);
  assert.equal((await fetch(`${base}/api/history`)).status, 401);
  assert.equal(
    (
      await fetch(`${base}/api/history`, {
        headers: { ...s.headers, origin: 'https://other.test' },
      })
    ).status,
    403,
  );
  assert.equal(
    (await fetch(`${base}/api/history`, { method: 'POST', headers: s.headers })).status,
    405,
  );
  assert.equal(
    (await fetch(`${base}/api/history?before=0`, { headers: s.headers })).status,
    400,
  );
  assert.equal(
    (
      await fetch(`${base}/api/check/9223372036854775808`, {
        headers: s.headers,
      })
    ).status,
    400,
  );
  assert.equal(
    (await fetch(`${base}/api/history?path=secret`, { headers: s.headers })).status,
    400,
  );
  assert.equal((await fetch(`${base}/package.json`)).status, 404);
  const report = await (await fetch(`${base}/api/history`, { headers: s.headers })).json();
  assert.equal(report.directory, s.directory);
  assert.equal(report.rows[0].current.number, 23);
  await fetch(`${base}/api/history?before=9223372036854775807`, {
    headers: s.headers,
  });
  await fetch(`${base}/api/check/9223372036854775807`, { headers: s.headers });
  assert.deepEqual(
    s.calls().map((c) => c.args),
    [
      ['--json', 'status', '--history', '--', '.'],
      ['--json', 'status', '--history', '--before=9223372036854775807', '--', '.'],
      ['--json', 'status', '--check=9223372036854775807'],
    ],
  );
  const rejectedHost = await new Promise((resolve, reject) => {
    const req = httpRequest(base, { headers: { host: 'evil.test' } }, (res) => {
      res.resume();
      resolve(res.statusCode);
    });
    req.on('error', reject);
    req.end();
  });
  assert.equal(rejectedHost, 403);
});

test('multiple viewers share one snapshot and receive later refreshes', async (t) => {
  const s = await setup(t, { cacheMs: 0 });
  await Promise.all(
    Array.from({ length: 5 }, () =>
      fetch(`${s.url.origin}/api/history`, { headers: s.headers }),
    ),
  );
  assert.equal(s.calls().length, 1);
  await fetch(`${s.url.origin}/api/history`, { headers: s.headers });
  assert.equal(s.calls().length, 2);
  await s.dashboard.close();
  await assert.rejects(fetch(s.url.origin));
  await assert.rejects(s.dashboard.start(), /shutting down/);
});

test('browser launch requests a new instance, supports URL-only use and reports headless failures', async (t) => {
  const launches = [];
  const s = await setup(t, {
    openBrowser: async (...args) => {
      launches.push(args);
      const child = new EventEmitter();
      setTimeout(() => child.emit('exit', 0), 5);
      return child;
    },
  });
  assert.equal((await s.dashboard.show({ openBrowser: false })).browser_requested, false);
  assert.equal(launches.length, 0);
  assert.equal((await s.dashboard.show()).browser_requested, true);
  assert.deepEqual(launches[0], [s.dashboard.url, { newInstance: true, wait: false }]);
  s.dashboard.openBrowser = async () => {
    throw new Error('No desktop');
  };
  const fallback = await s.dashboard.show();
  assert.equal(fallback.browser_requested, false);
  assert.equal(fallback.url, s.dashboard.url);
  assert.match(fallback.message, /same machine/);
});

test('the MCP exposes the browser dashboard in read-only mode and closes it on disconnect', async (t) => {
  const s = fixture(t);
  const { client } = await clientFor(t, s, ['--read-only']);
  const tools = await client.listTools();
  assert.ok(tools.tools.some((tool) => tool.name === 'super_open_dashboard'));
  const result = await client.callTool({
    name: 'super_open_dashboard',
    arguments: { open_browser: false },
  });
  assert.equal(result.isError, false);
  const url = result.structuredContent.url;
  assert.equal((await fetch(url)).status, 200);
  await client.close();
  await assert.rejects(fetch(url));
});

test('history keeps large check identities exact and does not equate terminality with passing', () => {
  const checks = checksFrom({
    history: [
      { number: '9223372036854775806', terminal: true },
      { number: 2, result: { outcome: 'pass' } },
    ],
    rows: [
      {
        current: {
          number: '9223372036854775807',
          result: { outcome: 'fail' },
          terminal: true,
        },
      },
    ],
  });
  assert.deepEqual(
    checks.map((c) => c.number),
    ['9223372036854775807', '9223372036854775806', 2],
  );
  assert.equal(category(checks[0]), 'fail');
  assert.equal(category(checks[1]), 'other');
  assert.equal(
    category({
      terminal: true,
      operational_error: { message: 'x' },
      result: { outcome: 'pass' },
    }),
    'error',
  );
});

test('browser dispatch requests a new window on Windows and Linux with a native fallback', async () => {
  for (const platform of ['win32', 'linux', 'darwin']) {
    const calls = [];
    await launchBrowser(
      'http://127.0.0.1:1234/#token=test',
      { newInstance: true, wait: false },
      {
        platform,
        launch: async (...args) => {
          calls.push(args);
          return {};
        },
      },
    );
    assert.equal(calls[0][1].newInstance, true);
    if (platform !== 'darwin') assert.deepEqual(calls[0][1].app.arguments, ['--new-window']);
  }
  const calls = [];
  await launchBrowser(
    'http://127.0.0.1:1234/',
    { newInstance: true },
    {
      platform: 'linux',
      launch: async (...args) => {
        calls.push(args);
        if (args[1].app) throw new Error('Unknown default browser');
        return {};
      },
    },
  );
  assert.equal(calls.length, 2);
  assert.equal(calls[1][1].app, undefined);
});
