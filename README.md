# Super CLI

Use the Super CLI to check software behavior.
Super finds supertests in C, C#, Java, JavaScript, Python, Rust, and VHDL, checks them,
and displays results and proposed fixes.

JavaScript and VHDL support is currently being rolled out. See
[language support](https://docs.schematic.tech/super/get-started/#language-support) for availability.

[Read the Super documentation](https://docs.schematic.tech/super/).

Repository links live in each worktree's Git metadata, with source history shared
across the clone's worktrees. Moving a checkout preserves its link; a separate clone
still needs `super link`. Credentials remain in your user configuration.

CLI 0.11.0 requires the updated Super service. Update the service before distributing
this client; older clients remain supported. Existing local links migrate on the
next modifying command.

## Install

```sh
curl -fsSL https://get.schematic.tech/cli.sh | sh
```
