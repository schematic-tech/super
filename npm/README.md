# Super CLI installer

Install Super on macOS or Linux (including WSL) with Node.js 22.14 or later:

```sh
npx @schematic-tech/super@latest
```

This package bundles the [Super installer](https://get.schematic.tech/cli.sh). It sets up Rust
if needed, builds the latest published `super-tool` crate, and offers to add the Super skill
to supported coding agents. Installation requires internet access and may take a few minutes.

Follow the installer’s instructions, including restarting your terminal if requested. Then run:

```sh
super --version
super login
```

Use `--no-skills` to skip agent setup, or `--yes` to add skills to detected agents without prompting:

```sh
npx @schematic-tech/super@latest --no-skills
npx --yes @schematic-tech/super@latest --yes
```

The first `--yes` in the second command accepts npm’s package download prompt; the second accepts
agent setup. `--help` shows installer options. `--version` prints the npm installer version;
`super --version` prints the installed CLI version. The installer and CLI are versioned independently.

The installed CLI uses your existing Super credentials and repository links. Installing the npm
package alone does not run the installer; invoke it with `npx`, or use `super-install` after a global
npm installation. Run the installed CLI with `super` or `sup`.

See [Get started](https://docs.schematic.tech/super/get-started/) for the next steps.
