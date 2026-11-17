import { createServer } from 'node:http';
import { randomBytes, timingSafeEqual } from 'node:crypto';
import { readFile, realpath, stat } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import open, { apps } from 'open';

const PUBLIC = fileURLToPath(new URL('../public/', import.meta.url));
const TYPES = {
  '.html': 'text/html; charset=utf-8',
  '.js': 'text/javascript; charset=utf-8',
  '.css': 'text/css; charset=utf-8',
  '.json': 'application/json',
  '.woff2': 'font/woff2',
  '.woff': 'font/woff',
  '.svg': 'image/svg+xml',
  '.txt': 'text/plain; charset=utf-8',
};
const decimal = (value) =>
  /^[1-9]\d{0,18}$/.test(value) && BigInt(value) <= 9223372036854775807n;

export async function launchBrowser(
  url,
  options,
  { platform = process.platform, launch = open } = {},
) {
  if (platform !== 'darwin') {
    try {
      // Chromium-family browsers and Firefox honor this on Windows and Linux.
      return await launch(url, {
        ...options,
        app: { name: apps.browser, arguments: ['--new-window'] },
      });
    } catch {
      // Unrecognized default browsers still get the normal operating-system opener.
    }
  }
  return launch(url, options);
}

export class Dashboard {
  constructor(
    bridge,
    { openBrowser = launchBrowser, publicDirectory = PUBLIC, cacheMs = 2000 } = {},
  ) {
    this.bridge = bridge;
    this.openBrowser = openBrowser;
    this.publicDirectory = publicDirectory;
    this.cacheMs = cacheMs;
    this.token = randomBytes(32).toString('hex');
    this.cache = new Map();
    this.closed = false;
    this.controller = new AbortController();
  }

  async start() {
    if (this.closed) throw new Error('The dashboard is shutting down.');
    this.starting ??= (async () => {
      await stat(path.join(this.publicDirectory, 'index.html'));
      this.directory = await this.bridge.directory();
      const server = createServer((req, res) => {
        this.handle(req, res).catch(() => {
          if (!res.headersSent)
            this.json(res, 503, {
              error: 'Could not read Super history. Retry in a moment.',
            });
          else res.destroy();
        });
      });
      this.server = server;
      server.requestTimeout = 10000;
      server.headersTimeout = 10000;
      await new Promise((resolve, reject) => {
        server.once('error', reject);
        server.listen(0, '127.0.0.1', resolve);
      });
      this.origin = `http://127.0.0.1:${server.address().port}`;
      this.url = `${this.origin}/#token=${this.token}`;
      if (this.closed) await this.closeServer();
      return this.url;
    })().catch((error) => {
      this.starting = undefined;
      throw error;
    });
    return this.starting;
  }

  async show({ openBrowser = true } = {}) {
    const url = await this.start();
    if (!openBrowser) return { url, browser_requested: false };
    try {
      // open dispatches through macOS, Windows/WSL or Linux's default browser.
      // Browser preferences can still choose a tab rather than a separate window.
      const child = await this.openBrowser(url, {
        newInstance: true,
        wait: false,
      });
      // Catch launch failures reported just after spawn, without waiting for the browser to close.
      await new Promise((resolve, reject) => {
        const timer = setTimeout(resolve, 300);
        child.once('error', (error) => {
          clearTimeout(timer);
          reject(error);
        });
        child.once('exit', (code) => {
          clearTimeout(timer);
          code && code !== 0 ? reject(new Error('Browser exited')) : resolve();
        });
      });
      return { url, browser_requested: true };
    } catch {
      return {
        url,
        browser_requested: false,
        message:
          'Open this link in a browser on the same machine. Automatic browser launch is unavailable.',
      };
    }
  }

  json(res, code, body) {
    res.writeHead(code, { 'Content-Type': 'application/json; charset=utf-8' });
    res.end(JSON.stringify(body));
  }

  async snapshot(kind, value) {
    const key = `${kind}:${value ?? ''}`;
    const cached = this.cache.get(key);
    if (cached && (cached.pending || Date.now() - cached.at < this.cacheMs))
      return cached.promise;
    // Keep memory bounded when paging or inspecting many individual checks.
    if (this.cache.size >= 32) this.cache.delete(this.cache.keys().next().value);
    const entry = { pending: true, at: Date.now() };
    entry.promise = (async () => {
      const args =
        kind === 'check'
          ? [`--check=${value}`]
          : ['--history', ...(value ? [`--before=${value}`] : []), '--', '.'];
      const response = await this.bridge.run('status', args, {
        signal: this.controller.signal,
      });
      if (response.isError) {
        const error = response.output.events.find((event) => event.type === 'error')?.data;
        return {
          error:
            error?.message ??
            response.output.error ??
            'Super could not read this worktree. Run super login and super link in your terminal.',
          code: error?.code,
        };
      }
      const report = response.output.events.findLast((event) => event.type === 'results')?.data;
      if (!report) return { error: 'Super returned no history. Try refreshing.' };
      return {
        directory: this.directory,
        updated_at: new Date().toISOString(),
        ...report,
      };
    })()
      .catch((error) => ({
        error:
          error.code === 'busy'
            ? 'Super is busy. History will retry shortly.'
            : 'Could not read Super history. Check the CLI installation and repository link.',
      }))
      .finally(() => {
        entry.pending = false;
        entry.at = Date.now();
      });
    this.cache.set(key, entry);
    return entry.promise;
  }

  async handle(req, res) {
    res.setHeader('Cache-Control', 'no-store');
    res.setHeader('X-Content-Type-Options', 'nosniff');
    res.setHeader('Referrer-Policy', 'no-referrer');
    res.setHeader('X-Frame-Options', 'DENY');
    res.setHeader(
      'Content-Security-Policy',
      "frame-ancestors 'none'; connect-src 'self'; object-src 'none'; base-uri 'none'",
    );
    if (
      !this.origin ||
      req.headers.host !== new URL(this.origin).host ||
      (req.headers.origin && req.headers.origin !== this.origin) ||
      req.headers['sec-fetch-site'] === 'cross-site'
    )
      return this.json(res, 403, { error: 'Local dashboard access only.' });
    if (req.method !== 'GET') return this.json(res, 405, { error: 'Read-only dashboard.' });
    const url = new URL(req.url, this.origin);
    if (url.pathname.startsWith('/api/')) {
      const supplied = Buffer.from(req.headers.authorization ?? '');
      const expected = Buffer.from(`Bearer ${this.token}`);
      if (supplied.length !== expected.length || !timingSafeEqual(supplied, expected))
        return this.json(res, 401, {
          error: 'Open the dashboard link from Super to reconnect.',
        });
      let kind, value;
      if (url.pathname === '/api/history') {
        kind = 'history';
        value = url.searchParams.get('before');
        if ([...url.searchParams.keys()].some((key) => key !== 'before'))
          return this.json(res, 400, { error: 'Invalid history request.' });
      } else if (url.pathname.startsWith('/api/check/')) {
        kind = 'check';
        value = url.pathname.slice('/api/check/'.length);
        if (!value || url.search)
          return this.json(res, 400, { error: 'Invalid check number.' });
      } else return this.json(res, 404, { error: 'Not found.' });
      if (value !== null && value !== undefined && !decimal(value))
        return this.json(res, 400, { error: 'Invalid check number.' });
      const report = await this.snapshot(kind, value);
      return this.json(res, report.error ? 503 : 200, report);
    }
    const relative =
      url.pathname === '/' ? 'index.html' : decodeURIComponent(url.pathname).slice(1);
    if (!relative || relative.split(/[\\/]/).some((part) => !part || part.startsWith('.')))
      return this.json(res, 404, { error: 'Not found.' });
    try {
      const root = await realpath(this.publicDirectory);
      const file = await realpath(path.join(root, relative));
      if (!file.startsWith(`${root}${path.sep}`))
        return this.json(res, 404, { error: 'Not found.' });
      const type = TYPES[path.extname(file)];
      if (!type) return this.json(res, 404, { error: 'Not found.' });
      const body = await readFile(file);
      res.writeHead(200, { 'Content-Type': type });
      res.end(body);
    } catch {
      this.json(res, 404, { error: 'Not found.' });
    }
  }

  async closeServer() {
    if (!this.server?.listening) return;
    await new Promise((resolve) => {
      this.server.close(resolve);
      this.server.closeAllConnections();
    });
  }

  async close() {
    this.closed = true;
    this.controller.abort();
    await this.starting?.catch(() => {});
    await this.closeServer();
    this.cache.clear();
  }
}
