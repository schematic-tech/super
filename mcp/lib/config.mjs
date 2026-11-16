import { readFileSync } from 'node:fs';
import { parseArgs } from 'node:util';
import path from 'node:path';

export const PACKAGE = JSON.parse(
  readFileSync(new URL('../package.json', import.meta.url), 'utf8'),
);
export const MINIMUM_CLI_VERSION = '0.14.3';

export function configuration(args = process.argv.slice(2)) {
  const { values } = parseArgs({
    args,
    strict: true,
    allowPositionals: false,
    options: {
      directory: { type: 'string', default: process.cwd() },
      'super-bin': { type: 'string' },
      'read-only': { type: 'boolean', default: false },
      'timeout-seconds': { type: 'string', default: '120' },
      'print-config': { type: 'boolean', default: false },
      help: { type: 'boolean', short: 'h', default: false },
      version: { type: 'boolean', short: 'v', default: false },
    },
  });
  if (!/^[1-9]\d*$/.test(values['timeout-seconds']) || Number(values['timeout-seconds']) > 600) {
    throw new Error('--timeout-seconds must be an integer from 1 to 600.');
  }
  return {
    directory: values.directory,
    binary: values['super-bin'],
    readOnly: values['read-only'],
    timeoutMs: Number(values['timeout-seconds']) * 1000,
    printConfig: values['print-config'],
    help: values.help,
    version: values.version,
  };
}

export const HELP = `Usage: super-mcp [options]

Serve Super CLI tools, resources, and prompts over MCP stdio.
Requires Super CLI ${MINIMUM_CLI_VERSION} or later. Run super login in your terminal first.

  --directory PATH       Git worktree root to serve (default: current directory)
  --super-bin PATH        Use a particular Super executable
  --read-only             Expose inspection tools without starting checks or making changes
  --timeout-seconds N     Interrupt a CLI command after 1–600 seconds (default: 120)
  --print-config          Print MCP client JSON configuration and exit
  --version, -v           Print the MCP sidecar version
  --help, -h              Show this help

Configure one server per Git worktree. Authentication uses the CLI's existing profile.
Canceling an MCP request stops observation; use super_cancel to cancel remote checks.
An in-flight patch application is allowed to finish when interrupted.
`;

export function clientConfiguration(options, directory) {
  const args = ['--yes', `${PACKAGE.name}@${PACKAGE.version}`, '--directory', directory];
  if (options.binary)
    args.push(
      '--super-bin',
      options.binary.includes('/') || options.binary.includes('\\')
        ? path.resolve(options.binary)
        : options.binary,
    );
  if (options.readOnly) args.push('--read-only');
  if (options.timeoutMs !== 120000)
    args.push('--timeout-seconds', String(options.timeoutMs / 1000));
  return { mcpServers: { super: { command: 'npx', args } } };
}
