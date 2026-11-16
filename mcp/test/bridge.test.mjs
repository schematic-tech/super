import assert from 'node:assert/strict';
import { existsSync, symlinkSync, mkdirSync, readFileSync } from 'node:fs';
import { join } from 'node:path';
import { setTimeout as delay } from 'node:timers/promises';
import test from 'node:test';
import { CliBridge, parseEnvelope, validateSelector } from '../lib/cli.mjs';
import { fixture, waitUntil } from './helpers.mjs';

test('preserves large integer identifiers and evidence exactly', (t) => {
  const value = parseEnvelope(
    '{"schema_version":1,"type":"results","data":{"number":9223372036854775807,"evidence":18446744073709551615,"small":23,"fraction":1.5}}',
  );
  assert.equal(value.data.number, '9223372036854775807');
  assert.equal(value.data.evidence, '18446744073709551615');
  assert.equal(value.data.small, 23);
  assert.equal(value.data.fraction, 1.5);
});

test('runs JSON commands without stdin, daemons, or update processes', async (t) => {
  const { bridge, calls, directory } = fixture(t);
  const response = await bridge.run('status', ['--check=23']);
  assert.equal(response.isError, false);
  assert.equal(response.output.events[0].data.rows[0].current.result.outcome, 'pass');
  assert.deepEqual(calls()[0], {
    ...calls()[0],
    args: ['--json', 'status', '--check=23'],
    cwd: directory,
    noDaemon: '1',
    noUpdates: '1',
    input: '',
  });
});

test('a failed check is a domain result rather than an MCP transport error', async (t) => {
  const { bridge } = fixture(t, 'problem');
  const response = await bridge.run('check', ['--detach', '--commit=HEAD']);
  assert.equal(response.isError, false);
  assert.equal(response.output.exit_code, 1);
  assert.equal(response.output.events[0].data.rows[0].current.problematic, true);
});

test('operational errors retain CLI codes without exposing the configured token', async (t) => {
  const test = fixture(t, 'error');
  test.bridge.environment.SUPER_ACCESS_TOKEN = 'test-key-do-not-return';
  const response = await test.bridge.run('status', []);
  assert.equal(response.isError, true);
  assert.equal(response.output.exit_code, 2);
  assert.equal(response.output.events[0].data.code, 'operation_failed');
  assert.doesNotMatch(JSON.stringify(response), /test-key-do-not-return/);
  assert.match(response.output.diagnostics, /\[redacted\]/);
});

for (const mode of ['invalid', 'unsupported', 'large']) {
  test(`rejects ${mode} CLI output without treating it as success`, async (t) => {
    const { bridge } = fixture(t, mode);
    const response = await bridge.run('status', []);
    assert.equal(response.isError, true);
    assert.ok(response.output.error);
  });
}

test('bounded observation returns its partial events and leaves remote cancellation explicit', async (t) => {
  const { bridge, calls, environment } = fixture(t, 'hold');
  const response = await bridge.run('status', ['--watch', '--check=23'], {
    observation: true,
    timeoutMs: 100,
  });
  assert.equal(response.isError, false);
  assert.equal(response.output.stopped, 'timeout');
  assert.equal(response.output.events.at(-1).type, 'closed');
  assert.equal(response.output.events.at(-1).data.detached, true);
  assert.equal(existsSync(environment.FAKE_STOP), true);
  assert.ok(calls().every((call) => call.args[1] === 'status'));
});

test('request cancellation interrupts and reaps the CLI process', async (t) => {
  const { bridge, calls } = fixture(t, 'hold');
  const controller = new AbortController();
  const running = bridge.run('status', ['--watch', '--check=23'], { signal: controller.signal });
  await waitUntil(() => calls().length > 0);
  await delay(30);
  controller.abort();
  const response = await running;
  assert.equal(response.output.stopped, 'cancelled');
  assert.throws(() => process.kill(calls()[0].pid, 0), { code: 'ESRCH' });
});

test('shutdown escalates when a subprocess ignores interruption', async (t) => {
  const { bridge, calls } = fixture(t, 'hold');
  bridge.environment.FAKE_IGNORE_INTERRUPT = '1';
  const running = bridge.run('status', ['--watch']);
  await waitUntil(() => calls().length > 0);
  await delay(30);
  await bridge.close();
  const response = await running;
  assert.equal(response.output.stopped, 'shutdown');
  assert.throws(() => process.kill(calls()[0].pid, 0), { code: 'ESRCH' });
});

test('cancellation lets an in-flight fix finish beyond the normal process grace period', async (t) => {
  const { bridge, calls, environment } = fixture(t, 'slow-apply');
  bridge.options.graceMs = 30;
  const controller = new AbortController();
  const running = bridge.run('fix', ['--check=23', '--yes'], {
    signal: controller.signal,
    mutatesWorktree: true,
  });
  await waitUntil(() => calls().length > 0);
  await delay(40);
  controller.abort();
  const response = await running;
  assert.equal(response.output.stopped, 'cancelled');
  assert.equal(response.output.events.at(-1).type, 'success');
  assert.equal(readFileSync(environment.FAKE_STOP, 'utf8'), 'application completed');
});

test('rejects traversal and escaping symlinks while allowing historical paths', async (t) => {
  const { directory, root } = fixture(t);
  mkdirSync(join(root, 'elsewhere'));
  symlinkSync(join(root, 'elsewhere'), join(directory, 'escape'));
  for (const selector of [
    '../elsewhere',
    '/etc/passwd',
    'C:\\elsewhere',
    '..\\elsewhere',
    'escape/file.js::law',
    'bad\npath',
  ]) {
    await assert.rejects(validateSelector(directory, selector), /worktree|relative path/i);
  }
  await validateSelector(directory, 'removed/history.js::law');
  await validateSelector(directory, './file with spaces.js::law');
});

test('requires the configured directory to be a worktree root', async (t) => {
  const setup = fixture(t);
  const child = join(setup.directory, 'subdirectory');
  mkdirSync(child);
  const bridge = new CliBridge({
    directory: child,
    binary: setup.binary,
    environment: setup.environment,
  });
  await assert.rejects(bridge.run('status', []), /worktree root/);
  assert.equal(setup.calls().length, 0);
});

test('serializes commands that can change the worktree or link', async (t) => {
  const { bridge, calls } = fixture(t, 'hold');
  const running = bridge.run('check', ['--detach'], { mutatesWorktree: true });
  await waitUntil(() => calls().length > 0);
  await assert.rejects(
    bridge.run('fix', ['--check=23', '--yes'], { mutatesWorktree: true }),
    /Another command/,
  );
  await bridge.close();
  await running;
});

test('refuses unsupported CLI versions and never starts a state-changing command', async (t) => {
  const { bridge, calls } = fixture(t);
  bridge.environment.FAKE_VERSION = '0.13.0';
  await assert.rejects(bridge.run('check', ['--detach']), /0.14.3/);
  assert.equal(calls().length, 0);
});
