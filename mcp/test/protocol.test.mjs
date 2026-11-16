import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { once } from 'node:events';
import { existsSync } from 'node:fs';
import { setTimeout as delay } from 'node:timers/promises';
import test from 'node:test';
import { clientFor, entry, fixture, waitUntil } from './helpers.mjs';

test('advertises typed tools, read-only resources, templates, and prompts over stdio', async (t) => {
  const setup = fixture(t);
  const { client, diagnostics } = await clientFor(t, setup);
  const { tools } = await client.listTools();
  assert.equal(tools.length, 10);
  assert.equal(tools.find((tool) => tool.name === 'super_status').annotations.readOnlyHint, true);
  assert.equal(
    tools.find((tool) => tool.name === 'super_apply_fix').annotations.destructiveHint,
    true,
  );
  assert.equal(tools.find((tool) => tool.name === 'super_check').annotations.readOnlyHint, false);
  assert.equal((await client.listResources()).resources.length, 3);
  assert.equal((await client.listResourceTemplates()).resourceTemplates.length, 2);
  assert.equal((await client.listPrompts()).prompts.length, 2);
  const info = await client.callTool({ name: 'super_info', arguments: {} });
  assert.equal(info.structuredContent.cli.version, '0.14.3');
  const resource = await client.readResource({ uri: 'super://checks/23' });
  assert.equal(JSON.parse(resource.contents[0].text).events[0].type, 'results');
  const prompt = await client.getPrompt({
    name: 'review-check',
    arguments: { check_number: '23' },
  });
  assert.match(prompt.messages[0].content.text, /super_preview_fix/);
  assert.deepEqual(diagnostics, []);
});

test('maps supported workflows to exact noninteractive CLI arguments', async (t) => {
  const setup = fixture(t);
  const { client } = await clientFor(t, setup);
  const cases = [
    [
      'super_check',
      { path: 'src/file.js::law' },
      ['--json', 'check', '--detach', '--commit=HEAD', '--', 'src/file.js::law'],
    ],
    [
      'super_check',
      { include_uncommitted: true, problems_only: true },
      ['--json', 'check', '--detach', '--dirty', '--problems'],
    ],
    [
      'super_status',
      { check_number: '9223372036854775807', history: true, before: 22 },
      ['--json', 'status', '--history', '--before=22', '--check=9223372036854775807'],
    ],
    [
      'super_usage',
      { period: '7d', repository: 'a repository' },
      ['--json', 'usage', '--period=7d', '--repo=a repository'],
    ],
    ['super_preview_fix', { check_number: 23 }, ['--json', 'fix', '--check=23', '--dry-run']],
    ['super_apply_fix', { check_number: 23 }, ['--json', 'fix', '--check=23', '--yes']],
    ['super_link', {}, ['--json', 'link', '--', '.']],
    ['super_unlink', {}, ['--json', 'unlink', '--', '.']],
    ['super_cancel', { check_number: 23 }, ['--json', 'cancel', '--check=23']],
  ];
  for (const [name, args, expected] of cases) {
    const response = await client.callTool({ name, arguments: args });
    assert.equal(response.isError, false, JSON.stringify(response));
    assert.deepEqual(setup.calls().at(-1).args, expected);
  }
});

test('validates conflicts, unknown fields, numbers, and selectors before CLI execution', async (t) => {
  const setup = fixture(t);
  const { client } = await clientFor(t, setup);
  for (const [name, args] of [
    ['super_check', { commit: 'HEAD', include_uncommitted: true }],
    ['super_status', { check_number: 23, path: '.' }],
    ['super_status', { before: 23 }],
    ['super_status', { check_number: 9007199254740992 }],
    ['super_status', { check_number: '9223372036854775808' }],
    ['super_status', { command: 'login', token: 'not accepted' }],
    ['super_cancel', {}],
    ['super_check', { commit: '--help' }],
    ['super_check', { path: '../outside' }],
  ]) {
    try {
      const response = await client.callTool({ name, arguments: args });
      assert.equal(response.isError, true, JSON.stringify({ name, args, response }));
    } catch (error) {
      assert.notEqual(error.name, 'AssertionError');
    }
  }
  assert.equal(setup.calls().length, 0);
});

test('progress follows streamed events and retains the exact patch and result data', async (t) => {
  const setup = fixture(t);
  const { client } = await clientFor(t, setup);
  const progress = [];
  const response = await client.callTool(
    { name: 'super_wait', arguments: { check_number: 23 } },
    { onprogress: (update) => progress.push(update.progress) },
  );
  assert.equal(response.isError, false);
  assert.deepEqual(progress, [1, 2, 3]);
  assert.deepEqual(
    response.structuredContent.events.map((event) => event.type),
    ['results', 'check_updated', 'closed'],
  );
  const proposal = await client.callTool({
    name: 'super_preview_fix',
    arguments: { check_number: 23 },
  });
  assert.equal(
    proposal.structuredContent.events[0].data.proposal.diff,
    '--- a/code.js\n+++ b/code.js\n@@ -1 +1 @@\n-old\n+new\n',
  );
});

test('read-only mode removes mutation tools rather than relying on annotations', async (t) => {
  const setup = fixture(t);
  const { client } = await clientFor(t, setup, ['--read-only']);
  const { tools } = await client.listTools();
  assert.deepEqual(tools.map((tool) => tool.name).sort(), [
    'super_info',
    'super_preview_fix',
    'super_status',
    'super_usage',
    'super_wait',
  ]);
  try {
    const result = await client.callTool({ name: 'super_check', arguments: {} });
    assert.equal(result.isError, true);
  } catch (error) {
    assert.notEqual(error.name, 'AssertionError');
  }
  assert.equal(setup.calls().length, 0);
});

test('MCP request cancellation stops local observation without issuing a remote cancel', async (t) => {
  const setup = fixture(t, 'hold');
  const { client } = await clientFor(t, setup);
  const controller = new AbortController();
  const running = client.callTool(
    { name: 'super_wait', arguments: { check_number: 23 } },
    { signal: controller.signal },
  );
  const rejected = assert.rejects(running);
  await waitUntil(() => setup.calls().length > 0);
  await delay(40);
  controller.abort();
  await rejected;
  for (let attempt = 0; !existsSync(setup.environment.FAKE_STOP) && attempt < 100; attempt++)
    await delay(10);
  assert.equal(existsSync(setup.environment.FAKE_STOP), true);
  assert.ok(setup.calls().every((call) => call.args[1] === 'status'));
});

test('disconnecting a client reaps even a CLI process that ignores interruption', async (t) => {
  const setup = fixture(t, 'hold');
  setup.environment.FAKE_IGNORE_INTERRUPT = '1';
  const { client } = await clientFor(t, setup);
  const running = client.callTool({ name: 'super_wait', arguments: { check_number: 23 } });
  const rejected = assert.rejects(running);
  await waitUntil(() => setup.calls().length > 0);
  await delay(40);
  const pid = setup.calls()[0].pid;
  await client.close();
  await rejected;
  assert.throws(() => process.kill(pid, 0), { code: 'ESRCH' });
});

test('also negotiates with existing 2025 MCP hosts and writes only protocol JSON to stdout', async (t) => {
  const setup = fixture(t);
  const child = spawn(
    process.execPath,
    [entry, '--directory', setup.directory, '--super-bin', setup.binary],
    { env: setup.environment, stdio: ['pipe', 'pipe', 'pipe'] },
  );
  t.after(() => {
    if (child.exitCode === null) child.kill('SIGKILL');
  });
  let output = '';
  const messages = [];
  child.stdout.on('data', (chunk) => {
    output += chunk;
    let newline;
    while ((newline = output.indexOf('\n')) >= 0) {
      messages.push(JSON.parse(output.slice(0, newline)));
      output = output.slice(newline + 1);
    }
  });
  child.stdin.write(
    `${JSON.stringify({ jsonrpc: '2.0', id: 1, method: 'initialize', params: { protocolVersion: '2025-11-25', capabilities: {}, clientInfo: { name: 'legacy-test', version: '1.0' } } })}\n`,
  );
  for (let attempt = 0; !messages.some((message) => message.id === 1) && attempt < 100; attempt++)
    await delay(20);
  const initialized = messages.find((message) => message.id === 1);
  assert.equal(initialized.result.protocolVersion, '2025-11-25');
  child.stdin.write(`${JSON.stringify({ jsonrpc: '2.0', method: 'notifications/initialized' })}\n`);
  child.stdin.write(
    `${JSON.stringify({ jsonrpc: '2.0', id: 2, method: 'tools/list', params: {} })}\n`,
  );
  for (let attempt = 0; !messages.some((message) => message.id === 2) && attempt < 100; attempt++)
    await delay(20);
  assert.equal(messages.find((message) => message.id === 2).result.tools.length, 10);
  const exited = once(child, 'exit');
  child.stdin.end();
  await exited;
});
