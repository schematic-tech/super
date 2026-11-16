#!/usr/bin/env node
import fs from 'node:fs';

const args = process.argv.slice(2);
const mode = process.env.FAKE_MODE ?? 'normal';
if (process.env.FAKE_LOG)
  fs.appendFileSync(
    process.env.FAKE_LOG,
    `${JSON.stringify({
      args,
      cwd: process.cwd(),
      pid: process.pid,
      noDaemon: process.env.SUPER_NO_DAEMON,
      noUpdates: process.env.SUPER_NO_UPDATE_CHECK,
      input: fs.readFileSync(0, 'utf8'),
    })}\n`,
  );
if (args[0] === '--version') {
  console.log(`super ${process.env.FAKE_VERSION ?? '0.14.3'}`);
  process.exit(0);
}
if (args[0] !== '--json') process.exit(9);
const command = args[1];
const emit = (type, data, schema_version = 1) =>
  console.log(JSON.stringify({ schema_version, type, data }));
const number = args.find((value) => value.startsWith('--check='))?.slice(8) ?? '23';
const run = { id: '997a25aa-4fe6-49dc-b1b4-6611d0ba980c' };
const current = {
  number: 23,
  terminal: true,
  problematic: mode === 'problem',
  operational_error: null,
  result: { outcome: mode === 'problem' ? 'fail' : 'pass' },
};
const report = { run, rows: [{ current }], finished: true };

if (mode === 'slow-apply') {
  process.on('SIGINT', () => {});
  setTimeout(() => {
    if (process.env.FAKE_STOP) fs.writeFileSync(process.env.FAKE_STOP, 'application completed');
    emit('success', { message: 'Applied fix', check_number: 23 });
  }, 400);
} else if (mode === 'invalid') {
  console.log('not JSON');
} else if (mode === 'unsupported') {
  emit('results', report, 2);
} else if (mode === 'large') {
  console.log('x'.repeat(3 * 1024 * 1024));
} else if (mode === 'integer') {
  console.log(
    '{"schema_version":1,"type":"results","data":{"check_number":9223372036854775807,"evidence":{"value":18446744073709551615}}}',
  );
} else if (mode === 'error') {
  console.error(`diagnostic ${process.env.SUPER_ACCESS_TOKEN ?? ''}`);
  emit('error', {
    code: 'operation_failed',
    message: `rejected ${process.env.SUPER_ACCESS_TOKEN ?? ''}`,
  });
  process.exitCode = 2;
} else if (args.includes('--watch') || mode === 'hold') {
  emit('results', {
    ...report,
    finished: false,
    rows: [{ current: { ...current, terminal: false } }],
  });
  const interval = setInterval(() => {}, 1000);
  process.on('SIGINT', () => {
    if (process.env.FAKE_IGNORE_INTERRUPT) return;
    if (process.env.FAKE_STOP) fs.writeFileSync(process.env.FAKE_STOP, 'interrupted');
    emit('closed', { detached: true, target: `--check ${number}` });
    clearInterval(interval);
    process.exit(0);
  });
  if (mode !== 'hold') {
    setTimeout(() => emit('check_updated', { check_number: 23, check: current }), 15);
    setTimeout(() => {
      emit('closed', { detached: false, target: `--check ${number}` });
      clearInterval(interval);
    }, 30);
  }
} else if (command === 'link') {
  emit('linked', { workspace: { id: run.id, name: 'example' }, source: { oid: 'a'.repeat(40) } });
} else if (command === 'unlink') {
  emit('success', { message: 'Unlinked example' });
} else if (command === 'usage') {
  emit('usage', {
    totalTokens: 7,
    totalChecks: 1,
    quota: { remaining: 100 },
    usageAvailable: true,
  });
} else if (command === 'fix') {
  if (args.includes('--dry-run'))
    emit('fix_proposal', {
      check_number: Number(number),
      proposal: { diff: '--- a/code.js\n+++ b/code.js\n@@ -1 +1 @@\n-old\n+new\n' },
      applies_cleanly: true,
      source_changed: false,
    });
  else if (args.includes('--yes'))
    emit('success', {
      message: 'Applied fix',
      check_number: Number(number),
      source_changed: false,
    });
  else process.exitCode = 2;
} else if (command === 'cancel') {
  emit('results', { ...report, cancellation: [{ check_number: 23, outcome: 'canceled' }] });
} else {
  emit('results', { ...report, finished: command !== 'check' || mode === 'problem' });
  if (mode === 'problem') process.exitCode = 1;
}
