#!/usr/bin/env node
import { serveStdio } from '@modelcontextprotocol/server/stdio';
import { Dashboard } from '../lib/dashboard.mjs';
import { CliBridge } from '../lib/cli.mjs';
import { clientConfiguration, configuration, HELP, PACKAGE } from '../lib/config.mjs';
import { createServer } from '../lib/server.mjs';

try {
  const options = configuration();
  if (options.help) {
    process.stdout.write(HELP);
  } else if (options.version) {
    process.stdout.write(`${PACKAGE.name} ${PACKAGE.version}\n`);
  } else {
    const bridge = new CliBridge(options);
    const directory = await bridge.directory();
    if (options.printConfig) {
      process.stdout.write(
        `${JSON.stringify(clientConfiguration(options, directory), null, 2)}\n`,
      );
    } else {
      const dashboard = new Dashboard(bridge);
      const handle = options.dashboard
        ? undefined
        : serveStdio(() => createServer(bridge, { ...options, dashboard }), {
            onerror: () => console.error('Super MCP encountered a protocol error.'),
          });
      let closing;
      const close = () => {
        closing ??= Promise.allSettled([dashboard.close(), bridge.close(), handle?.close()]);
        return closing;
      };
      process.once('SIGINT', close);
      process.once('SIGTERM', close);
      if (options.dashboard) {
        const shown = await dashboard.show();
        console.error(`Super history: ${shown.url}`);
        if (shown.message) console.error(shown.message);
      } else process.stdin.once('end', close);
    }
  }
} catch (error) {
  console.error(`Super MCP: ${error.message}`);
  process.exitCode = 1;
}
