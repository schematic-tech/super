import { execFile, spawn } from 'node:child_process';
import { constants } from 'node:fs';
import { access, realpath, stat } from 'node:fs/promises';
import { homedir } from 'node:os';
import path from 'node:path';
import { StringDecoder } from 'node:string_decoder';
import { promisify } from 'node:util';
import { MINIMUM_CLI_VERSION } from './config.mjs';

const exec = promisify(execFile);
const COMMANDS = new Set(['link', 'unlink', 'check', 'status', 'usage', 'cancel', 'fix']);
const MAX_DIAGNOSTIC_BYTES = 16384;

function capture(file, args, options) {
  const running = exec(file, args, options);
  running.child.stdin?.end();
  return running;
}

export class BridgeError extends Error {
  constructor(code, message) {
    super(message);
    this.code = code;
  }
}

export function parseEnvelope(line) {
  // Keep identifiers and integer evidence exact across Rust's i64/u64 and JavaScript.
  const value = JSON.parse(line, (_key, item, context) => {
    if (typeof item === 'number' && Number.isInteger(item) && !Number.isSafeInteger(item)) {
      if (context?.source && /^-?\d+$/.test(context.source)) return context.source;
      if (!context?.source)
        throw new Error('This Node.js version cannot preserve large JSON integers.');
    }
    return item;
  });
  if (
    !value ||
    value.schema_version !== 1 ||
    typeof value.type !== 'string' ||
    !('data' in value)
  ) {
    throw new Error('Expected Super CLI JSON schema version 1.');
  }
  return value;
}

function contained(root, candidate) {
  const relative = path.relative(root, candidate);
  return (
    relative === '' ||
    (!path.isAbsolute(relative) && relative !== '..' && !relative.startsWith(`..${path.sep}`))
  );
}

export async function validateSelector(directory, selector) {
  if (selector === undefined) return;
  const file = selector.split('::', 1)[0];
  if (
    !file ||
    /[\x00-\x1f\x7f]/.test(selector) ||
    path.posix.isAbsolute(file) ||
    path.win32.isAbsolute(file) ||
    file.split(/[\\/]/).includes('..')
  ) {
    throw new BridgeError(
      'invalid_selector',
      'Use a relative path inside the configured worktree, optionally followed by ::supertest_name.',
    );
  }
  let candidate = path.resolve(directory, file);
  if (!contained(directory, candidate))
    throw new BridgeError('invalid_selector', 'The path escapes the configured worktree.');
  // Historical source paths need not exist today. Check their nearest existing ancestor.
  while (true) {
    try {
      if (!contained(directory, await realpath(candidate))) {
        throw new BridgeError(
          'invalid_selector',
          'The path resolves outside the configured worktree.',
        );
      }
      return;
    } catch (error) {
      if (!['ENOENT', 'ENOTDIR'].includes(error.code)) throw error;
      const parent = path.dirname(candidate);
      if (parent === candidate) throw error;
      candidate = parent;
    }
  }
}

function diagnostic(text, environment) {
  let value = text.replace(/\x1b\[[0-?]*[ -/]*[@-~]/g, '');
  if (environment.SUPER_ACCESS_TOKEN)
    value = value.replaceAll(environment.SUPER_ACCESS_TOKEN, '[redacted]');
  return value.trim();
}

async function executable(configured, environment) {
  const name = configured ?? 'super';
  const explicit = path.isAbsolute(name) || name.includes('/') || name.includes('\\');
  const names =
    process.platform === 'win32' && !path.extname(name) ? [`${name}.exe`, name] : [name];
  const directories = (environment.PATH ?? '')
    .split(path.delimiter)
    .filter((entry) => path.isAbsolute(entry));
  if (!configured)
    directories.push(path.join(environment.CARGO_HOME ?? path.join(homedir(), '.cargo'), 'bin'));
  const candidates = explicit
    ? [path.resolve(name)]
    : directories.flatMap((directory) => names.map((entry) => path.join(directory, entry)));
  for (const candidate of candidates) {
    try {
      const resolved = await realpath(candidate);
      if (!(await stat(resolved)).isFile()) continue;
      await access(resolved, constants.X_OK);
      return resolved;
    } catch (error) {
      if (!['ENOENT', 'EACCES', 'ENOTDIR'].includes(error.code)) throw error;
    }
  }
  throw new BridgeError(
    'cli_not_found',
    `Install Super CLI ${MINIMUM_CLI_VERSION} or later, or set --super-bin to its executable. Run npx @schematic-tech/super@latest in your terminal to install it.`,
  );
}

export class CliBridge {
  constructor(options = {}) {
    this.options = {
      timeoutMs: 120000,
      maxOutputBytes: 2 * 1024 * 1024,
      graceMs: 3000,
      maxProcesses: 4,
      ...options,
    };
    this.environment = {
      ...(options.environment ?? process.env),
      SUPER_NO_DAEMON: '1',
      SUPER_NO_UPDATE_CHECK: '1',
      GIT_TERMINAL_PROMPT: '0',
      NO_COLOR: '1',
    };
    this.operations = new Set();
    this.mutating = false;
    this.closed = false;
  }

  async directory() {
    const directory = await realpath(this.options.directory ?? process.cwd());
    if (!(await stat(directory)).isDirectory())
      throw new BridgeError('invalid_directory', '--directory must identify a directory.');
    return directory;
  }

  async identity() {
    if (!this.identityPromise) {
      this.identityPromise = (async () => {
        const binary = await executable(this.options.binary, this.environment);
        const { stdout } = await capture(binary, ['--version'], {
          cwd: await this.directory(),
          env: this.environment,
          timeout: 5000,
          maxBuffer: 4096,
          windowsHide: true,
        });
        const match = /^super (\d+)\.(\d+)\.(\d+)\s*$/.exec(stdout);
        if (!match)
          throw new BridgeError(
            'unsupported_cli',
            'The configured executable did not report a stable Super CLI version.',
          );
        const [major, minor, patch] = match.slice(1).map(Number);
        if (major === 0 && (minor < 14 || (minor === 14 && patch < 3))) {
          throw new BridgeError(
            'unsupported_cli',
            `Super CLI ${MINIMUM_CLI_VERSION} or later is required.`,
          );
        }
        return { binary, version: `${major}.${minor}.${patch}` };
      })().catch((error) => {
        this.identityPromise = undefined;
        if (error instanceof BridgeError) throw error;
        throw new BridgeError(
          'cli_unavailable',
          'Could not read the Super CLI version. Check --super-bin and run super --version in your terminal.',
        );
      });
    }
    return this.identityPromise;
  }

  async worktree(directory, signal) {
    let root;
    try {
      const { stdout } = await capture('git', ['rev-parse', '--show-toplevel'], {
        cwd: directory,
        env: this.environment,
        signal,
        timeout: 5000,
        maxBuffer: 65536,
        windowsHide: true,
      });
      root = await realpath(stdout.trim());
    } catch {
      throw new BridgeError(
        'invalid_worktree',
        'Set --directory to a Git worktree root before using repository tools.',
      );
    }
    if (root !== directory)
      throw new BridgeError(
        'invalid_worktree',
        `Set --directory to the Git worktree root: ${root}`,
      );
  }

  run(command, args, options = {}) {
    if (!COMMANDS.has(command))
      return Promise.reject(new BridgeError('invalid_command', 'Unsupported Super command.'));
    if (this.closed)
      return Promise.reject(new BridgeError('closed', 'The MCP sidecar is shutting down.'));
    if (this.operations.size >= this.options.maxProcesses)
      return Promise.reject(
        new BridgeError('busy', 'Too many Super commands are running. Wait for one to finish.'),
      );
    if (options.mutatesWorktree && this.mutating)
      return Promise.reject(
        new BridgeError(
          'busy',
          'Another command is changing this worktree or its link. Wait for it to finish.',
        ),
      );
    if (options.mutatesWorktree) this.mutating = true;
    const controller = new AbortController();
    const abort = () => controller.abort();
    options.signal?.addEventListener('abort', abort, { once: true });
    if (options.signal?.aborted) abort();
    const operation = { controller };
    this.operations.add(operation);
    operation.done = (async () => {
      try {
        controller.signal.throwIfAborted();
        const directory = await this.directory();
        const { binary } = await this.identity();
        if (options.repository !== false) await this.worktree(directory, controller.signal);
        await validateSelector(directory, options.selector);
        controller.signal.throwIfAborted();
        return await this.execute(binary, directory, command, args, {
          ...options,
          signal: controller.signal,
        });
      } finally {
        options.signal?.removeEventListener('abort', abort);
        this.operations.delete(operation);
        if (options.mutatesWorktree) this.mutating = false;
      }
    })();
    return operation.done;
  }

  execute(binary, directory, command, args, options) {
    return new Promise((resolve) => {
      const child = spawn(binary, ['--json', command, ...args], {
        cwd: directory,
        env: this.environment,
        stdio: ['ignore', 'pipe', 'pipe'],
        shell: false,
        detached: process.platform !== 'win32',
        windowsHide: true,
      });
      const decoder = new StringDecoder('utf8');
      const events = [];
      // Super deliberately finishes an in-flight patch application on Ctrl+C.
      // Killing it after a fixed grace period would break that file-write guarantee.
      const finishPatch = command === 'fix' && args.includes('--yes');
      let pending = '',
        stderr = '',
        bytes = 0,
        stderrBytes = 0,
        failure,
        stopped = null,
        killTimer,
        closed = false;
      let progress = Promise.resolve();
      const forceStop = () => {
        if (!child.pid) return;
        try {
          if (process.platform === 'win32') child.kill('SIGKILL');
          else process.kill(-child.pid, 'SIGKILL');
        } catch (error) {
          if (error.code !== 'ESRCH') failure ??= 'Could not stop the CLI process.';
        }
      };
      const stop = (reason) => {
        if (stopped) return;
        stopped = reason;
        if (closed) return;
        // Let the CLI finish any in-flight file application and detach observation.
        child.kill('SIGINT');
        if (!finishPatch) killTimer = setTimeout(forceStop, this.options.graceMs);
      };
      const abort = () => stop(this.closed ? 'shutdown' : 'cancelled');
      options.signal.addEventListener('abort', abort, { once: true });
      if (options.signal.aborted) abort();
      const timeout = setTimeout(
        () => stop('timeout'),
        Math.min(options.timeoutMs ?? this.options.timeoutMs, this.options.timeoutMs),
      );
      const parse = (line) => {
        if (!line.trim() || failure) return;
        try {
          const event = parseEnvelope(line);
          if (event.type === 'error' && typeof event.data?.message === 'string') {
            event.data.message = diagnostic(event.data.message, this.environment);
          }
          events.push(event);
          if (events.length > 1000)
            throw new Error('Too many CLI events; narrow the selection or observation window.');
          const count = events.length;
          if (options.onEvent)
            progress = progress.then(() => options.onEvent(event, count)).catch(() => {});
        } catch {
          failure =
            'The CLI returned unsupported or invalid JSON, or too many events. Update Super or narrow the selection.';
          stop('invalid_output');
        }
      };
      child.stdout.on('data', (chunk) => {
        if (failure) return;
        bytes += chunk.length;
        if (bytes > this.options.maxOutputBytes) {
          failure =
            'CLI output exceeded the response limit. Select a smaller path, one check, or a shorter wait.';
          stop('output_limit');
          return;
        }
        pending += decoder.write(chunk);
        let index;
        while ((index = pending.indexOf('\n')) >= 0) {
          parse(pending.slice(0, index));
          pending = pending.slice(index + 1);
        }
      });
      child.stderr.on('data', (chunk) => {
        if (stderrBytes < MAX_DIAGNOSTIC_BYTES)
          stderr += chunk.subarray(0, MAX_DIAGNOSTIC_BYTES - stderrBytes).toString('utf8');
        stderrBytes += chunk.length;
      });
      child.once('error', (error) => {
        failure = `Could not execute Super CLI (${error.code ?? 'process error'}).`;
      });
      child.once('close', async (code, signal) => {
        closed = true;
        clearTimeout(timeout);
        clearTimeout(killTimer);
        options.signal.removeEventListener('abort', abort);
        pending += decoder.end();
        if (!failure) parse(pending);
        if (stopped && !finishPatch) forceStop();
        await progress;
        const boundedWait =
          options.observation && stopped === 'timeout' && events.length > 0 && !failure;
        const isError = Boolean(
          failure ||
          events.some((event) => event.type === 'error') ||
          (!boundedWait &&
            (stopped || code === null || code >= 2 || (code === 1 && command !== 'check'))) ||
          (!events.length && !boundedWait),
        );
        resolve({
          isError,
          output: {
            command,
            exit_code: code !== null && code >= 0 ? code : null,
            signal,
            stopped,
            integer_encoding: 'Unsafe JSON integers are preserved as decimal strings.',
            events,
            ...(failure ? { error: failure } : {}),
            ...(stderr ? { diagnostics: diagnostic(stderr, this.environment) } : {}),
            ...(stopped
              ? {
                  recovery:
                    'Stopping observation does not cancel remote checks or undo changes. Inspect status before retrying; use super_cancel to cancel remote checks.',
                }
              : {}),
          },
        });
      });
    });
  }

  async close() {
    this.closed = true;
    const operations = [...this.operations];
    for (const operation of operations) operation.controller.abort();
    await Promise.allSettled(operations.map((operation) => operation.done));
  }
}
