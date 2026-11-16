#!/usr/bin/env node
import { spawn } from 'node:child_process';
import { readFileSync } from 'node:fs';
import { constants } from 'node:os';
import { fileURLToPath } from 'node:url';

const { name, version } = JSON.parse(readFileSync(new URL('package.json', import.meta.url), 'utf8'));
const args = process.argv.slice(2);
const options = new Set(['--yes', '-y', '--no-skills', '--help', '-h', '--version', '-v']);
const unknown = args.find((arg) => !options.has(arg));

if (unknown !== undefined) {
  console.error(`Unknown installer option: ${unknown}`);
  process.exitCode = 1;
} else if (args.includes('--help') || args.includes('-h')) {
  console.log(`Usage: npx ${name} [--yes] [--no-skills]

Install the Super CLI on macOS or Linux (including WSL).
Sets up Rust if needed, builds the CLI, and offers to set up your coding agent.

  --yes, -y      Add skills to detected agents without prompting
  --no-skills    Skip agent skill setup
  --version, -v  Print the npm installer version
  --help, -h     Show this help

After installation, run: super login`);
} else if (args.includes('--version') || args.includes('-v')) {
  console.log(`${name} ${version}`);
} else if (!['darwin', 'linux'].includes(process.platform)) {
  console.error('The Super installer needs macOS or Linux. On Windows, run it inside WSL.');
  process.exitCode = 1;
} else {
  // Use the packaged installer and keep its terminal, prompts, and environment intact.
  // A separate command name leaves the installed native `super` executable on PATH.
  const child = spawn('/bin/sh', [fileURLToPath(new URL('cli.sh', import.meta.url)), ...args], {
    stdio: 'inherit',
  });
  const handlers = new Map(['SIGHUP', 'SIGINT', 'SIGTERM'].map((signal) => [
    signal, () => child.kill(signal),
  ]));
  for (const [signal, handler] of handlers) process.on(signal, handler);
  child.once('error', (error) => {
    console.error(`Could not start the Super installer: ${error.message}`);
    process.exitCode = 1;
  });
  child.once('close', (code, signal) => {
    for (const [name, handler] of handlers) process.off(name, handler);
    process.exitCode = code !== null && code >= 0 ? code : (signal ? 128 + constants.signals[signal] : 1);
  });
}
