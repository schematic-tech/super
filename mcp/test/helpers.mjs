import { execFileSync } from 'node:child_process';
import {
  chmodSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  realpathSync,
  rmSync,
  writeFileSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { setTimeout as delay } from 'node:timers/promises';
import { Client } from '@modelcontextprotocol/client';
import { StdioClientTransport } from '@modelcontextprotocol/client/stdio';
import { CliBridge } from '../lib/cli.mjs';

export const entry = fileURLToPath(new URL('../bin/super-mcp.mjs', import.meta.url));
export const packageRoot = fileURLToPath(new URL('../', import.meta.url));

export async function waitUntil(predicate) {
  for (let attempt = 0; attempt < 500; attempt++) {
    if (predicate()) return;
    await delay(10);
  }
  throw new Error('Timed out waiting for the fixture process.');
}

export function fixture(t, mode = 'normal') {
  const root = realpathSync(mkdtempSync(join(tmpdir(), 'super mcp test ')));
  t.after(() => rmSync(root, { recursive: true, force: true }));
  const directory = join(root, 'worktree');
  mkdirSync(directory);
  execFileSync('git', ['init', '-q', directory]);
  const binary = join(root, 'fake-super');
  const source = readFileSync(new URL('fixtures/super.mjs', import.meta.url), 'utf8');
  writeFileSync(binary, source.replace('#!/usr/bin/env node', `#!${process.execPath}`));
  chmodSync(binary, 0o755);
  const environment = {
    ...process.env,
    FAKE_MODE: mode,
    FAKE_LOG: join(root, 'calls.jsonl'),
    FAKE_STOP: join(root, 'stopped'),
    SUPER_CONFIG_DIR: join(root, 'profile'),
  };
  delete environment.SUPER_ACCESS_TOKEN;
  delete environment.SUPER_API_URL;
  const bridge = new CliBridge({ directory, binary, environment, graceMs: 250 });
  t.after(() => bridge.close());
  const calls = () => {
    try {
      return readFileSync(environment.FAKE_LOG, 'utf8')
        .trim()
        .split('\n')
        .map(JSON.parse)
        .filter((call) => call.args[0] !== '--version');
    } catch (error) {
      if (error.code === 'ENOENT') return [];
      throw error;
    }
  };
  return { root, directory, binary, environment, bridge, calls };
}

export async function clientFor(t, test, args = [], entryPoint = entry) {
  const client = new Client({ name: 'super-mcp-tests', version: '1.0.0' });
  const transport = new StdioClientTransport({
    command: process.execPath,
    args: [entryPoint, '--directory', test.directory, '--super-bin', test.binary, ...args],
    env: test.environment,
    stderr: 'pipe',
  });
  const diagnostics = [];
  transport.stderr.on('data', (chunk) => diagnostics.push(chunk.toString()));
  t.after(() => client.close());
  await client.connect(transport);
  return { client, transport, diagnostics };
}
