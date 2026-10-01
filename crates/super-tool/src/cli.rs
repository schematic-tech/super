use std::path::PathBuf;

use clap::{Args, CommandFactory, Parser, Subcommand};

pub const CLAP_STYLING: clap::builder::styling::Styles = clap::builder::styling::Styles::styled()
    .header(clap::builder::styling::AnsiColor::BrightMagenta.on_default().bold())
    .usage(clap::builder::styling::AnsiColor::BrightMagenta.on_default().bold())
    .literal(clap::builder::styling::AnsiColor::BrightCyan.on_default())
    .placeholder(clap::builder::styling::AnsiColor::Cyan.on_default());

#[derive(Debug, Parser)]
#[command(name = "super", bin_name = "super", version, about = "Check behaviors your code must always preserve",
    styles = CLAP_STYLING, propagate_version = true)]
pub struct Cli {
    #[arg(long, env = "SUPER_API_URL", global = true, hide = true)]
    pub api_url: Option<String>,
    /// Emit versioned JSON. With check --stream or status --watch, emit newline-delimited events.
    #[arg(long, global = true)]
    pub json: bool,
    #[command(subcommand)]
    pub command: Command,
}

impl Cli {
    pub fn validate(self) -> Result<Self, clap::Error> {
        // Validate after global flags have propagated: a subcommand's `requires` check
        // does not see --json when it was supplied before `check` in this Clap version.
        if matches!(&self.command, Command::Check(args) if args.stream) && !self.json {
            return Err(Self::command().error(
                clap::error::ErrorKind::MissingRequiredArgument,
                "--stream requires --json",
            ));
        }
        Ok(self)
    }
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Authenticate super with your Schematic account.
    Login,
    /// Remove the locally stored credential.
    Logout,
    /// Link a Git worktree and synchronize its current commit.
    Link {
        #[arg(default_value = ".", value_hint = clap::ValueHint::DirPath)]
        path: PathBuf,
    },
    /// Stop watching a linked Git worktree.
    Unlink {
        #[arg(value_hint = clap::ValueHint::DirPath)]
        path: Option<PathBuf>,
    },
    /// Start checks and wait for their results.
    Check(CheckArgs),
    /// Inspect existing checks without starting work or syncing source.
    Status(StatusArgs),
    /// Show account quota, token usage, and recent supertest checks.
    Usage(UsageArgs),
    /// Cancel shared checks for all observers by path, run, or check number.
    Cancel(CancelArgs),
    /// Apply a proposed fix after confirmation, or preview it with --dry-run.
    Fix(FixArgs),
    #[command(hide = true)]
    Daemon,
    #[command(hide = true)]
    RefreshReleases { directory: PathBuf },
}

#[derive(Debug, Args)]
pub struct UsageArgs {
    /// Activity period in your local time zone. Quota always covers the rolling seven days.
    #[arg(long, value_parser = ["today", "7d", "30d"], default_value = "30d")]
    pub period: String,
    /// Filter activity by repository name or ID. Use . for the current linked repository.
    #[arg(long, value_name = "NAME|ID|.")]
    pub repo: Option<String>,
}

#[derive(Debug, Args)]
// These booleans represent independently named command-line switches.
#[allow(clippy::struct_excessive_bools)]
pub struct CheckArgs {
    /// File or directory relative to the current directory. Omit for the linked repository.
    /// Append `::supertest_name` to a file path to check one supertest.
    #[arg(value_name = "PATH")]
    pub selector: Option<String>,
    /// Recheck problematic supertests from the latest run.
    #[arg(long)]
    pub problems: bool,
    #[command(flatten)]
    pub source: CheckSourceArgs,
    /// Return on acceptance. Remote checks continue.
    #[arg(long)]
    pub detach: bool,
    /// Stream admission and live JSON events, then a final result. Requires --json.
    #[arg(long, conflicts_with = "detach")]
    pub stream: bool,
    /// Show full explanations, evidence, and reproduction instructions. JSON is always complete.
    #[arg(long)]
    pub details: bool,
}

#[derive(Debug, Args)]
pub struct CheckSourceArgs {
    /// Check an existing commit without including working-tree changes.
    #[arg(long, value_name = "GIT_REVISION", conflicts_with = "dirty")]
    pub commit: Option<String>,
    /// Include working-tree changes in a temporary commit.
    #[arg(long)]
    pub dirty: bool,
}

#[derive(Debug, Args)]
#[command(
    after_help = "No target selects the latest run. A path selects the latest check for each matching supertest, across commits."
)]
pub struct StatusArgs {
    #[command(flatten)]
    pub target: TargetArgs,
    /// Follow results until closed. With --json, exit once checks and pending updates finish.
    #[arg(long)]
    pub watch: bool,
    /// Include a page of attempt history for the selected supertests.
    #[arg(long)]
    pub history: bool,
    /// Show full explanations, evidence, and reproduction instructions. JSON is always complete.
    #[arg(long)]
    pub details: bool,
    /// Continue history using the `next_before` check number from JSON output.
    #[arg(long, requires = "history", value_name = "CHECK_NUMBER", value_parser = clap::value_parser!(u64).range(1..=i64::MAX as u64))]
    pub before: Option<u64>,
}

#[derive(Debug, Args)]
pub struct FixArgs {
    /// File or directory containing one supertest, or `file::supertest_name`.
    /// Omit for a single problematic check in the latest run.
    #[arg(value_name = "PATH", conflicts_with = "check")]
    pub target: Option<String>,
    /// Retrieve the proposal for an exact check number.
    #[arg(long, value_name = "NUMBER", value_parser = clap::value_parser!(u64).range(1..=i64::MAX as u64))]
    pub check: Option<u64>,
    /// Retrieve and preview the proposal without modifying source files.
    #[arg(long, conflicts_with = "yes")]
    pub dry_run: bool,
    /// Include the proposal's instructions and suggested validation.
    #[arg(long)]
    pub details: bool,
    /// Approve application without prompting (required for noninteractive application).
    #[arg(long)]
    pub yes: bool,
}

#[derive(Debug, Args)]
#[group(skip)]
pub struct TargetArgs {
    /// File or directory. Append `::supertest_name` to a file path to select one supertest.
    #[arg(value_name = "PATH", conflicts_with_all = ["run", "check"])]
    pub selector: Option<String>,
    /// An exact run UUID, regardless of the local current commit.
    #[arg(long, value_name = "UUID")]
    #[arg(conflicts_with = "check")]
    pub run: Option<uuid::Uuid>,
    /// An exact check number, regardless of the local current commit.
    #[arg(long, value_name = "NUMBER", value_parser = clap::value_parser!(u64).range(1..=i64::MAX as u64))]
    pub check: Option<u64>,
}

#[derive(Debug, Args)]
#[command(group(clap::ArgGroup::new("target").args(["selector", "run", "check"]).required(true)))]
#[command(
    after_help = "A path targets the latest check for each matching supertest. A path, --run, or --check is required."
)]
pub struct CancelArgs {
    #[command(flatten)]
    pub target: TargetArgs,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn public_command_surface_is_flat() {
        let command = Cli::command();
        command.clone().debug_assert();
        let names: Vec<_> = command
            .get_subcommands()
            .filter(|c| !c.is_hide_set())
            .map(clap::Command::get_name)
            .collect();
        assert_eq!(
            names,
            [
                "login", "logout", "link", "unlink", "check", "status", "usage", "cancel", "fix"
            ]
        );
        assert!(Cli::try_parse_from(["super", "repo", "link"]).is_err());
        assert!(Cli::try_parse_from(["super", "completion", "zsh"]).is_err());
        assert!(Cli::try_parse_from(["super", "cancel"]).is_err());
        let mut command = Cli::command();
        let status_help = command
            .find_subcommand_mut("status")
            .unwrap()
            .render_long_help()
            .to_string();
        assert!(status_help.contains("No target selects the latest run"));
        let cancel_help = command
            .find_subcommand_mut("cancel")
            .unwrap()
            .render_long_help()
            .to_string();
        assert!(cancel_help.contains("A path targets the latest check for each matching supertest"));
        assert!(!cancel_help.contains("Omit for the latest run"));

        for name in ["check", "status", "fix", "cancel"] {
            let help = command
                .find_subcommand_mut(name)
                .unwrap()
                .render_long_help()
                .to_string();
            assert!(help.contains("[PATH]") || help.contains("<PATH>"), "{name}: {help}");
            assert!(help.contains("::supertest_name"), "{name}: {help}");
            assert!(!help.to_lowercase().contains("selector"), "{name}: {help}");
        }
    }

    #[test]
    fn check_words_are_now_selectors_and_execution_is_attached_by_default() {
        let Command::Check(args) = Cli::try_parse_from(["super", "check", "status"]).unwrap().command else {
            panic!()
        };
        assert_eq!(args.selector.as_deref(), Some("status"));
        assert!(!args.detach);
        assert!(Cli::try_parse_from(["super", "check", "--dirty", "--commit", "HEAD"]).is_err());
        assert!(Cli::try_parse_from(["super", "check", "--prove"]).is_err());
        let help = Cli::command()
            .find_subcommand_mut("check")
            .unwrap()
            .render_long_help()
            .to_string();
        assert!(!help.contains("--prove"));
    }

    #[test]
    fn attached_streaming_is_opt_in_and_requires_json() {
        let Command::Check(args) = Cli::try_parse_from(["super", "check", "--json", "--stream"])
            .unwrap()
            .command
        else {
            panic!()
        };
        assert!(args.stream && !args.detach);
        assert!(
            Cli::try_parse_from(["super", "check", "--stream"])
                .and_then(Cli::validate)
                .is_err()
        );
        assert!(
            Cli::try_parse_from(["super", "--json", "check", "--stream"])
                .and_then(Cli::validate)
                .is_ok()
        );
        assert!(Cli::try_parse_from(["super", "check", "--json", "--stream", "--detach"]).is_err());
        assert!(Cli::try_parse_from(["super", "status", "--json", "--stream"]).is_err());
    }

    #[test]
    fn status_is_a_snapshot_unless_watch_is_explicit() {
        let Command::Status(args) = Cli::try_parse_from(["super", "status", "--check", "23", "--history"])
            .unwrap()
            .command
        else {
            panic!()
        };
        assert!(!args.watch);
        assert!(args.history);
        assert!(Cli::try_parse_from(["super", "status", "--wait"]).is_err());
        assert!(Cli::try_parse_from(["super", "status", "--browse"]).is_err());
        assert!(
            Cli::try_parse_from(["super", "status", "--watch", "--json"])
                .unwrap()
                .json
        );
    }

    #[test]
    fn fix_applies_by_default_and_preview_cannot_authorize_edits() {
        let Command::Fix(args) = Cli::try_parse_from(["super", "fix", "--check", "23"]).unwrap().command else {
            panic!()
        };
        assert!(!args.dry_run && !args.yes);
        assert!(Cli::try_parse_from(["super", "fix", "--dry-run"]).is_ok());
        assert!(Cli::try_parse_from(["super", "fix", "--dry-run", "--yes"]).is_err());
        assert!(Cli::try_parse_from(["super", "fix", "--agent", "codex"]).is_err());
        assert!(Cli::try_parse_from(["super", "fix", "--agent", "codex", "--yes"]).is_err());
        assert!(Cli::try_parse_from(["super", "fix", "--apply"]).is_err());
        assert!(Cli::try_parse_from(["super", "fix", "--no-wait"]).is_err());
    }
}
