import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import { once } from 'node:events';
import { chmodSync, copyFileSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { setTimeout as delay } from 'node:timers/promises';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

const root = fileURLToPath(new URL('../', import.meta.url));
const entry = join(root, 'install.mjs');
const metadata = JSON.parse(readFileSync(join(root, 'package.json'), 'utf8'));

function temporary(t) {
  const path = mkdtempSync(join(tmpdir(), 'super npm test '));
  t.after(() => rmSync(path, { recursive: true, force: true }));
  return path;
}

function invoke(args, options = {}) {
  return spawnSync(process.execPath, [entry, ...args], { encoding: 'utf8', timeout: 10_000, ...options });
}

function stub(path, source) {
  writeFileSync(path, `#!${process.execPath}\n${source}\n`);
  chmodSync(path, 0o755);
}

function environment(t) {
  const path = temporary(t);
  const bin = join(path, 'bin');
  mkdirSync(bin);
  const env = {
    ...process.env,
    HOME: path,
    CARGO_HOME: join(path, 'cargo home'),
    RUSTUP_HOME: join(path, 'rustup home'),
    PATH: `${bin}:${process.env.PATH}`,
    TMPDIR: path,
    TEST_LOG: join(path, 'cargo.json'),
    SUPER_INSTALL_DOCS_ORIGIN: 'https://docs.schematic.tech',
  };
  stub(join(bin, 'rustc'), 'console.log("rustc 1.97.1 (test)");');
  stub(join(bin, 'cargo'), `
    const fs = require('node:fs');
    fs.writeFileSync(process.env.TEST_LOG, JSON.stringify({
      args: process.argv.slice(2), home: process.env.CARGO_HOME, pid: process.pid,
    }));
    if (process.env.TEST_WAIT) setInterval(() => {}, 1000);
    else process.exit(Number(process.env.TEST_EXIT ?? 0));
  `);
  return { path, env };
}

test('help, version, and invalid options never start the installer', (t) => {
  const { env } = environment(t);
  const help = invoke(['--help'], { env });
  assert.equal(help.status, 0, help.stderr);
  assert.match(help.stdout, /npx @schematic-tech\/super/);
  assert.match(help.stdout, /Sets up Rust/);
  const version = invoke(['--version'], { env });
  assert.equal(version.status, 0, version.stderr);
  assert.equal(version.stdout.trim(), `${metadata.name} ${metadata.version}`);
  const invalid = invoke(['--unknown; exit 0'], { env });
  assert.equal(invalid.status, 1);
  assert.match(invalid.stderr, /Unknown installer option/);
  assert.equal(existsSync(env.TEST_LOG), false);
});

test('runs the bundled installer from another directory and preserves Cargo configuration', (t) => {
  const { path, env } = environment(t);
  const result = invoke(['--yes', '--no-skills'], { cwd: path, env });
  assert.equal(result.status, 0, result.stderr);
  assert.match(result.stderr, /Super CLI installed/);
  const command = JSON.parse(readFileSync(env.TEST_LOG, 'utf8'));
  assert.deepEqual(command.args, ['install', 'super-tool']);
  assert.equal(command.home, env.CARGO_HOME);
  assert.doesNotMatch(result.stderr, /Set up your agent/);
});

test('a failed Cargo install fails the npm command', (t) => {
  const { env } = environment(t);
  const result = invoke(['--no-skills'], { env: { ...env, TEST_EXIT: '42' } });
  assert.equal(result.status, 1);
  assert.match(result.stderr, /Could not install the Super CLI/);
  assert.doesNotMatch(result.stderr, /Super CLI installed/);
});

test('inherits standard input and output and preserves installer exit status', (t) => {
  const path = temporary(t);
  copyFileSync(entry, join(path, 'install.mjs'));
  copyFileSync(join(root, 'package.json'), join(path, 'package.json'));
  writeFileSync(join(path, 'cli.sh'), 'read -r answer\nprintf "answer=%s\\n" "$answer"\nprintf "problem\\n" >&2\nexit 37\n');
  const result = spawnSync(process.execPath, [join(path, 'install.mjs')], {
    input: 'hello world\n', encoding: 'utf8', timeout: 10_000,
  });
  assert.equal(result.status, 37);
  assert.equal(result.stdout, 'answer=hello world\n');
  assert.equal(result.stderr, 'problem\n');
});

test('forwards termination so the installer cleans up its active Cargo process', { timeout: 15_000 }, async (t) => {
  const { env } = environment(t);
  const child = spawn(process.execPath, [entry, '--no-skills'], {
    env: { ...env, TEST_WAIT: '1' }, stdio: 'ignore',
  });
  t.after(() => { if (child.exitCode === null) child.kill('SIGKILL'); });
  const completed = once(child, 'exit');
  for (let attempt = 0; !existsSync(env.TEST_LOG) && attempt < 100; attempt++) await delay(50);
  assert.equal(existsSync(env.TEST_LOG), true, 'Cargo did not start');
  const { pid } = JSON.parse(readFileSync(env.TEST_LOG, 'utf8'));
  t.after(() => { try { process.kill(pid, 'SIGKILL'); } catch {} });
  child.kill('SIGTERM');
  assert.deepEqual(await completed, [143, null]);
  assert.throws(() => process.kill(pid, 0), { code: 'ESRCH' });
});

test('the packed package works through npx without install scripts or a native command collision', { timeout: 30_000 }, (t) => {
  const path = temporary(t);
  const packed = spawnSync('npm', ['pack', '--json', '--ignore-scripts', '--pack-destination', path], {
    cwd: root, encoding: 'utf8', timeout: 10_000,
  });
  assert.equal(packed.status, 0, packed.stderr);
  const [info] = JSON.parse(packed.stdout);
  assert.deepEqual(info.files.map((file) => file.path).sort(), ['LICENSE', 'README.md', 'cli.sh', 'install.mjs', 'package.json']);
  assert.deepEqual(metadata.bin, { 'super-install': 'install.mjs' });
  assert.deepEqual(Object.keys(metadata.scripts), ['test']);
  const result = spawnSync('npx', ['--yes', '--offline', '--ignore-scripts', '--cache', join(path, 'cache'),
    `file:${join(path, info.filename)}`, '--help'], {
    cwd: path, encoding: 'utf8', timeout: 15_000,
  });
  assert.equal(result.status, 0, result.stderr);
  assert.match(result.stdout, /Usage: npx @schematic-tech\/super/);
});
