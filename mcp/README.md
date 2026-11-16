# Super MCP sidecar

Connect a coding agent to Super CLI through the Model Context Protocol (MCP).
The sidecar serves tools, resources, and prompts over local stdio and supports
both current and 2025 MCP clients.

## Set up

Use Node.js 22.14 or later and Super CLI 0.14.3 or later. Install the CLI and sign in
from your terminal if needed:

```sh
npx @schematic-tech/super@latest
super login
super --version
```

Start an MCP server for a Git worktree:

```sh
npx --yes @schematic-tech/super-mcp@latest --directory /absolute/path/to/project
```

Configure one server per Git worktree, with `--directory` pointing to its root.
The server uses the CLI's saved login and repository links. Linking uploads the
current committed source; the `super_link` tool can do this when authorized.
Keep login and API-key entry in your terminal, outside MCP.

For a client that uses `mcpServers`, add:

```json
{
  "mcpServers": {
    "super": {
      "command": "npx",
      "args": [
        "--yes",
        "@schematic-tech/super-mcp@latest",
        "--directory",
        "/absolute/path/to/project"
      ]
    }
  }
}
```

Other hosts may use a different configuration format. Use the same command and
arguments in their MCP settings. To generate JSON pinned to the installed
sidecar version:

```sh
npx --yes @schematic-tech/super-mcp@latest \
  --directory /absolute/path/to/project --print-config
```

Use `--super-bin /absolute/path/to/super` if the CLI cannot be found. The sidecar
also checks Cargo's bin directory. Set `SUPER_CONFIG_DIR` in the MCP process's
environment when using a separate CLI profile. Existing CLI environment settings
apply, but the sidecar does not expose credentials as tool arguments or resources.

The server does not install Rust, open a browser, or change client configuration
when it starts. MCP protocol messages are the only output on stdout.

## Tools

| Tool                | Behavior                                                                             |
| ------------------- | ------------------------------------------------------------------------------------ |
| `super_info`        | Show the selected directory, CLI version, and setup instructions.                    |
| `super_status`      | Read existing results by path, run ID, or check number, including paginated history. |
| `super_usage`       | Read account quota, credits, usage, and recent checks.                               |
| `super_wait`        | Follow an exact selection for a bounded period and report progress.                  |
| `super_check`       | Upload source and start checks, returning on acceptance.                             |
| `super_cancel`      | Explicitly cancel shared checks for all observers.                                   |
| `super_preview_fix` | Retrieve a proposal and check its applicability without editing files.               |
| `super_apply_fix`   | Apply an approved proposal for an exact check number.                                |
| `super_link`        | Link this worktree and upload its current committed source.                          |
| `super_unlink`      | Remove this worktree's local link without deleting remote history.                   |

`super_check` checks committed `HEAD` by default. Set `include_uncommitted: true`
to check working changes, or supply a particular `commit`. These options are
mutually exclusive. Accepted checks can still be running; follow the returned
run ID with `super_status` or `super_wait`.

For path-based operations, use relative paths within the configured worktree.
Append `::supertest_name` to select one declaration. Run IDs and check numbers
remain scoped to that linked worktree. Supply large check numbers as decimal
strings to avoid losing precision.

Tools return the CLI's schema-version-1 JSON events as structured content and text.
Unsafe JSON integers are preserved as exact decimal strings. Result fields,
evidence, and patch text retain their meaning. A failed property check is a valid
tool result; operational failures use MCP's `isError` flag. Inspect the check's
actual outcome, operational errors, and pending updates, not just the process exit
code or `finished` field.

## Observation and file changes

`super_wait` defaults to 30 seconds. `--timeout-seconds` caps CLI command duration
before interruption, defaulting to 120 seconds and allowing up to 600. A bounded
wait returns its partial events. Canceling a request or closing the MCP connection
stops observation, not remote checks. Use `super_cancel` to cancel remote work.

Source upload and check acceptance may already have happened when an operation
times out. Inspect status before retrying. No operation is automatically retried
by the sidecar.

Preview a fix and obtain the user's approval before calling `super_apply_fix`.
The CLI verifies applicability and applies the patch without committing or
starting another check. An in-flight patch application is allowed to finish on
interruption, following the CLI's file-write behavior. Inspect the worktree before
retrying an interrupted application.

Use `--read-only` to expose only inspection tools. This removes check submission,
link changes, remote cancellation, and fix application from the server's tool list.
The sidecar's CLI subprocesses do not start background observers or update checks.

## Resources and prompts

Resources provide configuration (`super://configuration`), latest results
(`super://status`), account usage (`super://usage`), individual checks
(`super://checks/23`), and runs (`super://runs/<run-id>`). Reading them never starts
a check or applies a fix.

The `check-code` prompt guides source selection, submission, and result inspection.
The `review-check` prompt guides evidence and fix review without applying changes.
Use the [Super skill](https://docs.schematic.tech/super/ai/) for authoring guidance
and [Automation and JSON](https://docs.schematic.tech/super/reference/automation/)
for the result contract.

## Develop

From this directory:

```sh
npm ci --ignore-scripts
npm test
node bin/super-mcp.mjs --directory /absolute/path/to/project
```

Tests exercise the real MCP SDK over stdio, including older protocol negotiation,
cancellation, process cleanup, path containment, read-only mode, and installation
of the packed package. The Rust CLI and MCP sidecar are versioned independently.
