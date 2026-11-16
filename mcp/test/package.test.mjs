import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { existsSync, readFileSync } from 'node:fs';
import { join } from 'node:path';
import test from 'node:test';
import { clientFor, entry, fixture, packageRoot } from './helpers.mjs';

test('help and client configuration work without a CLI installation or login', (t) => {
  const setup = fixture(t);
  const help = execFileSync(process.execPath, [entry, '--help'], { encoding: 'utf8' });
  assert.match(help, /MCP stdio/);
  const config = JSON.parse(
    execFileSync(
      process.execPath,
      [entry, '--directory', setup.directory, '--read-only', '--print-config'],
      { encoding: 'utf8' },
    ),
  );
  assert.equal(config.mcpServers.super.command, 'npx');
  const version = JSON.parse(readFileSync(join(packageRoot, 'package.json'), 'utf8')).version;
  assert.ok(config.mcpServers.super.args.includes(`@schematic-tech/super-mcp@${version}`));
  assert.ok(config.mcpServers.super.args.includes('--read-only'));
  assert.ok(config.mcpServers.super.args.includes(setup.directory));
  assert.equal(setup.calls().length, 0);
});

test('the packed sidecar installs offline and serves MCP using only published runtime files', async (t) => {
  const setup = fixture(t);
  const packed = JSON.parse(
    execFileSync('npm', ['pack', '--json', '--ignore-scripts', '--pack-destination', setup.root], {
      cwd: packageRoot,
      encoding: 'utf8',
    }),
  )[0];
  const expected = [
    'LICENSE',
    'bin/super-mcp.mjs',
    'lib/cli.mjs',
    'lib/config.mjs',
    'lib/server.mjs',
    'package.json',
  ];
  if (existsSync(join(packageRoot, 'README.md'))) expected.push('README.md');
  assert.deepEqual(packed.files.map((file) => file.path).sort(), expected.sort());
  const manifest = JSON.parse(readFileSync(join(packageRoot, 'package.json'), 'utf8'));
  assert.deepEqual(Object.keys(manifest.scripts), ['test']);
  execFileSync(
    'npm',
    [
      'install',
      '--prefix',
      setup.root,
      '--offline',
      '--ignore-scripts',
      '--no-audit',
      '--no-fund',
      join(setup.root, packed.filename),
    ],
    { encoding: 'utf8' },
  );
  const installedEntry = join(
    setup.root,
    'node_modules/@schematic-tech/super-mcp/bin/super-mcp.mjs',
  );
  const { client } = await clientFor(t, setup, [], installedEntry);
  const response = await client.callTool({ name: 'super_status', arguments: { check_number: 23 } });
  assert.equal(response.isError, false);
  assert.equal(response.structuredContent.events[0].data.rows[0].current.number, 23);
});
