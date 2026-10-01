mod browser;
mod cancellation;
mod compact;
mod diff;
mod presentation;
mod progress;
mod prompt;
mod style;
mod usage;

use browser::Browser;
use presentation::{activity, status};
use style::{Ink, Line};

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    io::{IsTerminal, Write},
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use crossterm::{
    cursor::{Hide, MoveTo, Show},
    event::{Event, EventStream, KeyCode, KeyEventKind},
    execute, queue,
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures_util::StreamExt;
use serde::Serialize;
use super_types::{Check, CheckSubmission, CheckSubmissionResponse, CommitRef, FixProposal, Supertest, Workspace};
use tokio::{sync::mpsc, task::JoinSet};
use uuid::Uuid;

use crate::{api::PupClient, config::LocalRepository};

const TICKS: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

#[derive(Debug, Clone)]
pub struct Ui {
    pub json: bool,
    pub interactive: bool,
    pub details: bool,
    release_alert: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScopeStatusRow {
    pub supertest: Supertest,
    pub current: Option<Check>,
    pub previous: Option<Check>,
}

#[derive(Debug, Clone)]
pub struct Results {
    pub repository: String,
    pub repository_root: PathBuf,
    pub workspace_id: Uuid,
    pub head: Option<CommitRef>,
    pub temporary_commit_parents: HashMap<String, String>,
    pub run: Option<CheckSubmission>,
    pub selector: String,
    pub rows: Vec<ScopeStatusRow>,
    pub history: Vec<Check>,
    pub next_before: Option<u64>,
    pub history_loaded: bool,
    pub history_loading: bool,
    pub history_error: Option<String>,
}

impl Results {
    pub fn from_submission(
        local: &LocalRepository,
        head: Option<CommitRef>,
        response: CheckSubmissionResponse,
    ) -> Self {
        Self {
            selector: response.submission.selector.clone(),
            run: Some(response.submission),
            rows: response
                .checks
                .into_iter()
                .map(|check| ScopeStatusRow {
                    supertest: check.supertest.clone(),
                    current: Some(check),
                    previous: None,
                })
                .collect(),
            ..Self::new(local, head)
        }
    }

    pub fn focused(local: &LocalRepository, head: CommitRef, check: Check) -> Self {
        Self {
            selector: check.supertest.selector(),
            rows: vec![ScopeStatusRow {
                supertest: check.supertest.clone(),
                current: Some(check),
                previous: None,
            }],
            ..Self::new(local, Some(head))
        }
    }

    pub fn from_checks(local: &LocalRepository, head: CommitRef, selector: String, checks: Vec<Check>) -> Self {
        Self {
            selector,
            rows: checks
                .into_iter()
                .map(|check| ScopeStatusRow {
                    supertest: check.supertest.clone(),
                    current: Some(check),
                    previous: None,
                })
                .collect(),
            ..Self::new(local, Some(head))
        }
    }

    pub fn empty(local: &LocalRepository, head: CommitRef) -> Self {
        Self::new(local, Some(head))
    }

    fn new(local: &LocalRepository, head: Option<CommitRef>) -> Self {
        Self {
            repository: local.name.clone(),
            repository_root: local.root.clone(),
            workspace_id: local.workspace_id,
            head,
            // GitRepository::temporary_commit keys this cache by parent:tree. Retain
            // the provenance even after HEAD moves or a cached Git object is pruned.
            temporary_commit_parents: local
                .temporary_commits
                .iter()
                .filter_map(|(fingerprint, oid)| {
                    let (parent, _) = fingerprint.split_once(':')?;
                    Some((oid.clone(), parent.to_owned()))
                })
                .collect(),
            run: None,
            selector: ".".into(),
            rows: Vec::new(),
            history: Vec::new(),
            next_before: None,
            history_loaded: false,
            history_loading: false,
            history_error: None,
        }
    }

    fn json_target(&self) -> String {
        self.run
            .as_ref()
            .map(|run| format!("--run {}", run.id))
            .or_else(|| {
                (self.rows.len() == 1)
                    .then(|| {
                        self.rows
                            .first()?
                            .current
                            .as_ref()
                            .map(|check| format!("--check {}", check.number))
                    })
                    .flatten()
            })
            .unwrap_or_else(|| shell_quote(&self.selector))
    }

    /// Human hints use check numbers or selectors; exact run identities remain in JSON.
    fn status_command(&self) -> String {
        if let [row] = self.rows.as_slice()
            && let Some(check) = &row.current
        {
            return format!("super status --check {}", check.number);
        }
        if self.run.is_some() && self.selector == "." {
            return "super status".into();
        }
        let current = std::env::current_dir().and_then(|path| path.canonicalize());
        let selector = self.command_selector(current.as_deref().unwrap_or(&self.repository_root));
        format!("super status {}", shell_quote(&selector))
    }

    fn command_selector(&self, current: &Path) -> String {
        command_selector(&self.repository_root, current, &self.selector)
    }

    pub async fn load_history(&mut self, client: &PupClient, before: Option<u64>) -> Result<()> {
        let page = client.history(self.workspace_id, Some(&self.selector), before).await?;
        if before.is_none() {
            self.history.clear();
        }
        self.append_history(page)
    }

    fn append_history(&mut self, page: super_types::CheckHistoryPage) -> Result<()> {
        if self.history.len() + page.checks.len() > 10_000 {
            anyhow::bail!(
                "interactive history reached 10,000 records; use status --history --before <CHECK_NUMBER> to read further pages"
            );
        }
        for check in page.checks {
            if !self.history.iter().any(|item| item.number == check.number) {
                self.history.push(check);
            }
        }
        self.history
            .sort_by_key(|check| std::cmp::Reverse((check.created_at, check.number)));
        self.next_before = page.next_before;
        self.history_loaded = true;
        self.history_error = None;
        self.refresh_previous();
        Ok(())
    }

    fn refresh_previous(&mut self) {
        for row in &mut self.rows {
            row.previous = self
                .history
                .iter()
                .filter(|candidate| {
                    candidate.supertest.selector() == row.supertest.selector()
                        && row.current.as_ref().is_none_or(|check| {
                            (candidate.created_at, candidate.number) < (check.created_at, check.number)
                        })
                })
                .max_by_key(|check| (check.created_at, check.number))
                .cloned();
        }
    }

    pub fn finished(&self) -> bool {
        self.rows
            .iter()
            .filter_map(|row| row.current.as_ref())
            .all(|check| check.terminal)
    }

    fn fixes_pending(&self) -> bool {
        self.rows
            .iter()
            .filter_map(|row| row.current.as_ref())
            .any(presentation::fix_pending)
    }

    fn history_incomplete(&self) -> bool {
        !self.history_loaded || self.history_loading || self.next_before.is_some()
    }

    pub fn exit_code(&self) -> u8 {
        let checks: Vec<_> = self.rows.iter().filter_map(|row| row.current.as_ref()).collect();
        if checks.iter().any(|check| check.operational_error.is_some()) {
            2
        } else {
            u8::from(checks.iter().any(|check| check.problematic))
        }
    }

    pub(crate) fn update(&mut self, check: &Check) -> bool {
        if check.repository_id != self.workspace_id {
            return false;
        }
        let mut changed = false;
        for row in &mut self.rows {
            for slot in [&mut row.current, &mut row.previous] {
                if let Some(current) = slot
                    && current.number == check.number
                    && current.event_sequence <= check.event_sequence
                {
                    changed |= *current != *check;
                    *current = check.clone();
                }
            }
        }
        for current in &mut self.history {
            if current.number == check.number && current.event_sequence <= check.event_sequence {
                changed |= *current != *check;
                *current = check.clone();
            }
        }
        changed
    }

    fn attempts(&self, row: usize) -> Vec<&Check> {
        let Some(row) = self.rows.get(row) else {
            return Vec::new();
        };
        let mut attempts = BTreeMap::new();
        for check in self
            .history
            .iter()
            .filter(|check| check.supertest.selector() == row.supertest.selector())
        {
            attempts.insert((check.created_at, check.number), check);
        }
        if let Some(check) = &row.current {
            attempts.insert((check.created_at, check.number), check);
        }
        attempts.into_values().rev().collect()
    }

    fn summary(&self) -> String {
        presentation::summary(self).text()
    }

    fn json_data(&self, include_history: bool) -> serde_json::Value {
        serde_json::json!({
            "repository": self.repository, "workspace_id": self.workspace_id,
            "resume_command": format!("super status {} --watch", self.json_target()), "run": self.run, "source": self.head, "selector": self.selector,
            "rows": self.rows,
            "history": if include_history { Some(&self.history) } else { None },
            "next_before": self.next_before,
            "finished": self.finished(), "problem_count": self.rows.iter().filter(|row| row.current.as_ref().is_some_and(|check| check.problematic)).count(),
        })
    }

    fn recheck(&self, check: &Check) -> Line {
        let current = std::env::current_dir().and_then(|path| path.canonicalize());
        let selector = command_selector(
            &self.repository_root,
            current.as_deref().unwrap_or(&self.repository_root),
            &check.supertest.selector(),
        );
        Line::default()
            .muted("To check again: ")
            .push(format!("super check {}", shell_quote(&selector)), Ink::Accent, true)
            .soft_wrap()
    }
}

impl Ui {
    pub fn new(json: bool) -> Self {
        let interactive = !json
            && std::io::stdin().is_terminal()
            && std::io::stdout().is_terminal()
            && std::io::stderr().is_terminal()
            && std::env::var("TERM").as_deref() != Ok("dumb");
        let color = !json && std::env::var_os("NO_COLOR").is_none() && std::env::var("TERM").as_deref() != Ok("dumb");
        console::set_colors_enabled(color && std::io::stdout().is_terminal());
        console::set_colors_enabled_stderr(color && std::io::stderr().is_terminal());
        Self {
            json,
            interactive,
            details: false,
            release_alert: None,
        }
    }

    pub fn json_value(&self, kind: &str, data: &impl Serialize) {
        debug_assert!(self.json);
        println!(
            "{}",
            serde_json::json!({ "schema_version": 1, "type": kind, "data": data })
        );
    }

    fn check_update(&self, check: &Check, streaming: bool) {
        if self.json && streaming {
            self.json_value(
                "check_updated",
                &serde_json::json!({"check_number": check.number, "check": check}),
            );
        } else if !self.json {
            let line = Line::default()
                .bold(&check.supertest.name)
                .muted(" · ")
                .push(activity(check), status(check).ink, false)
                .muted(format!(" · Check #{}", check.number));
            eprintln!("{}", line.render(Self::width(), 2).trim_end());
            if !check.terminal
                && let Some(activity) = &check.presentation.live_line
            {
                eprintln!(
                    "{}",
                    presentation::activity_sentence(activity, None)
                        .render(Self::width(), 2)
                        .trim_end()
                );
            }
        }
    }

    pub fn error(&self, error: &anyhow::Error) {
        let exhausted = crate::api::is_exhausted(error);
        if self.json {
            self.json_value(
                "error",
                &serde_json::json!({"code": if exhausted { super_types::api::EXHAUSTED_ERROR_CODE } else { "operation_failed" }, "message": format!("{error:#}")}),
            );
        } else {
            Self::stderr_lines(&[Line::default()
                .push("! ", if exhausted { Ink::Warning } else { Ink::Fail }, true)
                .plain(format!("{error:#}"))]);
        }
    }

    pub fn success(&self, message: impl std::fmt::Display) {
        if self.json {
            self.json_value("success", &serde_json::json!({"message": message.to_string()}));
        } else {
            Self::print_lines(&[Line::default().push("✓ ", Ink::Pass, false).bold(message.to_string())]);
        }
    }

    pub fn notice(&self, message: impl std::fmt::Display) {
        let line = if self.interactive {
            Line::default().muted(message.to_string())
        } else {
            Line::new(message.to_string())
        };
        Self::stderr_lines(&[line]);
    }

    pub fn update_available(&self, current: &str, latest: &semver::Version) {
        if self.json {
            return;
        }
        // Keep the installer on one copyable line, even in a narrow terminal.
        let accent = Ink::Accent.style().for_stderr().bold();
        let muted = Ink::Muted.style().for_stderr();
        let _ = writeln!(
            std::io::stderr().lock(),
            "\n  {} {}\n  curl -fsSL https://get.schematic.tech/cli.sh | sh\n",
            accent.apply_to(format!("Super CLI {latest} is available")),
            muted.apply_to(format!("(installed: {current})")),
        );
    }

    pub fn release_alert(&mut self, message: &str) -> bool {
        let message = style::clean(message);
        let message = message.trim();
        if message.is_empty() {
            return false;
        }
        let heading = Ink::Warning.style().for_stderr().bold();
        // Let the terminal soft-wrap so copied URLs and commands stay intact.
        let _ = writeln!(
            std::io::stderr().lock(),
            "\n  {}\n  {}\n",
            heading.apply_to("Super CLI notice"),
            message.replace('\n', "\n  "),
        );
        self.release_alert = Some(message.to_owned());
        true
    }

    fn stderr_lines(lines: &[Line]) {
        let width = Self::width();
        for line in lines.iter().flat_map(|line| line.wrapped(width.saturating_sub(4))) {
            eprintln!("{}", line.render(width, 2).trim_end());
        }
    }

    pub fn workspace(&self, workspace: &Workspace, head: &CommitRef) {
        if self.json {
            self.json_value("linked", &serde_json::json!({"workspace": workspace, "source": head}));
        } else {
            Self::print_lines(&[
                Line::default()
                    .push("✓ ", Ink::Pass, false)
                    .bold(format!("Linked {}", workspace.name))
                    .muted(format!(
                        " · {} · {}",
                        head.branch.as_deref().unwrap_or("detached HEAD"),
                        head.short_oid()
                    )),
                Line::default().muted("Next: ").push("super check", Ink::Accent, true),
            ]);
        }
    }

    pub fn fix_preview(&self, repository: &str, check: &Check, fix: &FixProposal, prepared: &crate::git::PreparedFix) {
        if self.json {
            self.json_value(
                "fix_proposal",
                &serde_json::json!({
                    "check_number": check.number, "repository_id": check.repository_id, "proposal": fix,
                    "source_changed": prepared.source_changed,
                    "applies_cleanly": prepared.apply_error.is_none(),
                    "apply_error": prepared.apply_error,
                }),
            );
            return;
        }
        Self::print_lines(&diff::preview(repository, check, fix, Self::width(), self.details));
    }

    pub fn fix_source_warning(check_number: u64) {
        Self::stderr_lines(&[
            Line::default()
                .push("! ", Ink::Warning, true)
                .plain(format!("Your source has changed since check #{check_number}.")),
            Line::new("The patch applies cleanly, but may no longer fix the issue. Recheck after applying."),
        ]);
    }

    pub fn fix_applied(&self, fix: &FixProposal, check: &Check, source_changed: bool, repository_root: &Path) {
        if self.json {
            self.json_value(
                "success",
                &serde_json::json!({
                    "message": format!("Applied fix · {}", fix.summary),
                    "check_number": check.number,
                    "source_changed": source_changed,
                }),
            );
            return;
        }
        let files = fix.files.len();
        Self::print_lines(&[
            Line::default(),
            Line::default().push("✓ ", Ink::Pass, false).bold(format!(
                "Applied · {files} file{} changed",
                if files == 1 { "" } else { "s" }
            )),
            Line::default().muted("Changes are uncommitted."),
            Line::default(),
        ]);
        let current = std::env::current_dir().and_then(|path| path.canonicalize());
        let selector = command_selector(
            repository_root,
            current.as_deref().unwrap_or(repository_root),
            &check.supertest.selector(),
        );
        let argument = shell_quote(&selector);
        let command = Line::default()
            .muted("Next: ")
            .push(format!("super check {argument}"), Ink::Accent, true);
        if command.width() > Self::width().saturating_sub(4) {
            Self::print_lines(&[Line::default()
                .muted("Next: ")
                .push("super check \\", Ink::Accent, true)]);
            let argument = Line::new("  ").push(argument, Ink::Accent, true);
            // Let the terminal soft-wrap exceptionally long arguments. Inserting a hard
            // newline inside a quoted filename would change a copied command's meaning.
            println!(
                "{}",
                argument.render(Self::width().max(argument.width() + 4), 2).trim_end()
            );
        } else {
            Self::print_lines(&[command]);
        }
    }

    pub fn results(&self, results: &Results, include_history: bool) {
        if self.json {
            self.json_value("results", &results.json_data(include_history));
            return;
        }
        Self::print_lines(&presentation::snapshot(
            results,
            include_history,
            self.details,
            Self::width(),
        ));
    }

    pub fn cancellation(
        &self,
        results: &Results,
        entries: &[crate::cancel::Entry],
        interrupted: Option<&anyhow::Error>,
    ) {
        if self.json {
            let mut data = results.json_data(false);
            data["cancellation"] = serde_json::json!(entries);
            data["interrupted"] = serde_json::json!(interrupted.is_some());
            self.json_value("results", &data);
        } else {
            Self::print_lines(&cancellation::render(results, entries, Self::width()));
            if let Some(error) = interrupted {
                self.notice(error);
            }
        }
    }

    pub fn completed(&self, results: &Results) {
        if self.json {
            self.results(results, results.rows.len() == 1);
        } else {
            Self::print_lines(&presentation::snapshot(results, false, self.details, Self::width()));
        }
    }

    fn width() -> usize {
        usize::from(terminal::size().map_or(80, |size| size.0)).max(40)
    }

    fn print_lines(lines: &[Line]) {
        let width = Self::width();
        for line in lines {
            if line.soft_wrap {
                println!("{}", line.render(width.max(line.width() + 4), 2).trim_end());
            } else {
                for line in line.wrapped(width.saturating_sub(4)) {
                    println!("{}", line.render(width, 2).trim_end());
                }
            }
        }
    }

    fn watch_closed(&self, results: &Results, detached: bool) {
        if self.json {
            self.json_value(
                "closed",
                &serde_json::json!({"detached": detached, "target": results.json_target()}),
            );
        }
    }

    pub fn accepted(&self, results: &Results) {
        let count = results.rows.len();
        let mut lines = vec![
            Line::default()
                .bold(format!("Accepted {count} check{}", if count == 1 { "" } else { "s" }))
                .muted(" · ")
                .append(presentation::context(results)),
        ];
        if let [row] = results.rows.as_slice()
            && let Some(check) = &row.current
        {
            lines.push(Line::default().muted(format!("{} · #{}", check.supertest.name, check.number)));
        }
        lines.push(Line::default().muted("Checks continue remotely."));
        lines.push(self.follow_up(results, true));
        Self::print_lines(&lines);
    }

    fn follow_up(&self, results: &Results, watch: bool) -> Line {
        Line::default()
            .muted(if watch { "Watch: " } else { "Details: " })
            .push(
                format!(
                    "{}{}{}",
                    results.status_command(),
                    if watch { " --watch" } else { "" },
                    if self.details { " --details" } else { "" }
                ),
                Ink::Accent,
                true,
            )
            .soft_wrap()
    }

    pub fn closed(&self, results: &Results) {
        if self.json {
            return;
        }
        let mut lines = vec![
            Line::default()
                .bold(if results.finished() { "View closed" } else { "Detached" })
                .muted(" · ")
                .append(presentation::context(results)),
            presentation::summary(results),
        ];
        if !results.finished() {
            lines.push(Line::default().muted("Checks continue remotely."));
        }
        let notices = results
            .rows
            .iter()
            .filter_map(|row| {
                let check = row.current.as_ref()?;
                let notice = presentation::fix_notice(check, false)?;
                Some(if results.rows.len() == 1 {
                    notice
                } else {
                    Line::default()
                        .bold(&row.supertest.name)
                        .muted(" · ")
                        .append(notice)
                        .soft_wrap()
                })
            })
            .collect::<Vec<_>>();
        if !notices.is_empty() {
            lines.push(Line::default());
            lines.extend(notices);
            lines.push(Line::default());
        }
        lines.push(self.follow_up(results, !results.finished() || results.fixes_pending()));
        Self::print_lines(&lines);
    }
}

fn elapsed(start: DateTime<Utc>, end: DateTime<Utc>) -> String {
    let seconds = (end - start).num_seconds().max(0);
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m {}s", seconds / 60, seconds % 60)
    } else {
        format!("{}h {}m", seconds / 3600, seconds % 3600 / 60)
    }
}
fn age(value: DateTime<Utc>) -> String {
    let seconds = (Utc::now() - value).num_seconds().max(0);
    if seconds >= 86400 {
        format!("{}d ago", seconds / 86400)
    } else if seconds >= 3600 {
        format!("{}h ago", seconds / 3600)
    } else if seconds >= 60 {
        format!("{}m ago", seconds / 60)
    } else {
        format!("{seconds}s ago")
    }
}

struct TerminalGuard {
    output: std::io::Stderr,
    frame: TerminalFrame,
}
impl TerminalGuard {
    fn enter() -> Result<Self> {
        terminal::enable_raw_mode()?;
        let guard = Self {
            output: std::io::stderr(),
            frame: TerminalFrame::default(),
        };
        // Leave mouse input with the terminal so dragging selects text for copying.
        execute!(std::io::stderr(), EnterAlternateScreen, Hide)?;
        Ok(guard)
    }
    fn render(&mut self, lines: Vec<String>, size: (u16, u16)) -> Result<()> {
        self.frame.render(&mut self.output.lock(), lines, size)
    }
}
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(std::io::stderr(), Show, LeaveAlternateScreen);
        let _ = terminal::disable_raw_mode();
    }
}

#[derive(Default)]
struct TerminalFrame {
    lines: Vec<String>,
    size: (u16, u16),
}

impl TerminalFrame {
    fn render(&mut self, output: &mut impl Write, lines: Vec<String>, size: (u16, u16)) -> Result<()> {
        // Preserve selection on unchanged rows while spinners and elapsed times update.
        // Padded rows erase old content without clearing the screen between frames.
        // A resize can reflow existing content, so repaint every row when dimensions change.
        for (row, line) in (0..size.1).zip(&lines) {
            if size != self.size || self.lines.get(usize::from(row)) != Some(line) {
                queue!(output, MoveTo(0, row))?;
                write!(output, "{line}")?;
            }
        }
        output.flush()?;
        self.lines = lines;
        self.size = size;
        Ok(())
    }
}

enum StreamMessage {
    Check(Box<super_types::CheckEvent>),
    Connection(usize, bool),
    Error(anyhow::Error),
}

#[derive(Debug, Clone, Copy)]
pub enum Observation {
    Attached,
    AttachedStream,
    Watch,
}

impl Observation {
    fn finished(self, results: &Results) -> bool {
        results.finished()
            && (!matches!(self, Self::Watch)
                || results
                    .rows
                    .iter()
                    .filter_map(|row| row.current.as_ref())
                    .all(|check| !check.updates_pending))
    }
}

async fn stream_initial_results(
    ui: &Ui,
    results: &Results,
    include_history: bool,
    signal: std::pin::Pin<&mut impl std::future::Future<Output = std::io::Result<()>>>,
) -> Result<bool> {
    // Register before publishing results: a caller can interrupt immediately after
    // receiving them. The caller keeps this listener alive throughout observation.
    let interrupted = match futures_util::poll!(signal) {
        std::task::Poll::Ready(result) => {
            result?;
            true
        }
        std::task::Poll::Pending => false,
    };
    ui.results(results, include_history);
    Ok(interrupted)
}

// Keep the event loop and its termination/cleanup conditions together.
#[allow(clippy::too_many_lines)]
pub async fn observe(client: &PupClient, results: &mut Results, mode: Observation, ui: &Ui) -> Result<bool> {
    let watching = matches!(mode, Observation::Watch);
    let mut size = terminal::size().unwrap_or((80, 24));
    let interactive = ui.interactive && size.0 >= 60 && size.1 >= 20;
    // Human views stay open for review. JSON watches finish after the selected checks'
    // pending updates; ordinary noninteractive checks still finish at the verdict.
    let keep_open = interactive || (watching && !ui.json);
    let streaming = !matches!(mode, Observation::Attached);
    let interrupted_signal = tokio::signal::ctrl_c();
    tokio::pin!(interrupted_signal);
    if ui.json && streaming && stream_initial_results(ui, results, watching, interrupted_signal.as_mut()).await? {
        let detached = !results.finished();
        if watching {
            ui.watch_closed(results, detached);
        }
        return Ok(detached);
    }
    if mode.finished(results) && !keep_open {
        if watching {
            ui.watch_closed(results, false);
        }
        return Ok(false);
    }
    let mut terminal = interactive.then(TerminalGuard::enter).transpose()?;
    let mut input = interactive.then(EventStream::new);
    let mut browser = Browser::new(results);
    browser.release_alert.clone_from(&ui.release_alert);
    browser.configure(ui.details, watching && results.rows.len() == 1 && ui.details);
    let mut timer = tokio::time::interval(Duration::from_millis(120));
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let (sender, mut receiver) = mpsc::channel(64);
    let mut tasks = JoinSet::new();
    let mut watched = HashSet::new();
    let mut next_index = 0;
    spawn_watches(client, results, &sender, &mut tasks, &mut watched, &mut next_index)?;
    let mut pending_history = initial_history(client, results, interactive);
    let mut visible_updates = visible_updates(results);
    let mut ended = false;
    let mut disconnected = HashSet::new();
    if !interactive && !ui.json {
        ui.notice(results.summary());
    }
    let detached = loop {
        if let Some(terminal) = &mut terminal {
            size = terminal::size().unwrap_or(size);
            let (width, height) = size;
            terminal.render(
                browser.frame(results, width.into(), height.into(), !disconnected.is_empty()),
                size,
            )?;
        }
        if mode.finished(results) && !keep_open {
            break false;
        }
        tokio::select! {
            task = tasks.join_next(), if !tasks.is_empty() => {
                if let Some(Err(error)) = task {
                    return Err(error).context("check observer task failed; remote work was not canceled");
                }
            }
            signal = &mut interrupted_signal => { signal?; break !results.finished(); }
            event = async { input.as_mut().expect("interactive input exists").next().await }, if interactive => {
                match event {
                    Some(Ok(Event::Key(key))) if key.kind != KeyEventKind::Release => {
                        if browser.closes(key) {
                            break !results.finished();
                        }
                        if key.code == KeyCode::Char('n') {
                            if pending_history.is_none() && browser.history_action(results).is_some() {
                                results.history_loading = true;
                                pending_history = Some(history_request(client, results, results.next_before));
                            }
                        } else {
                            let (width, height) = terminal::size().unwrap_or(size);
                            browser.key(key.code, results, height.into(), width.into());
                        }
                    }
                    Some(Err(error)) => return Err(error.into()),
                    None => break !results.finished(),
                    _ => {}
                }
            }
            page = async { pending_history.as_mut().expect("history request exists").as_mut().await }, if pending_history.is_some() => {
                pending_history = None;
                results.history_loading = false;
                match page.and_then(|page| results.append_history(page)) {
                    Ok(()) => {
                        if let Err(error) = spawn_watches(client, results, &sender, &mut tasks, &mut watched, &mut next_index) {
                            results.history_error = Some(format!("{error:#}"));
                        }
                    },
                    Err(error) => results.history_error = Some(format!("{error:#}")),
                }
            }
            message = receiver.recv(), if !ended => match message {
                Some(StreamMessage::Check(event)) => {
                    let visible = (activity(&event.check), event.check.presentation.live_line.clone());
                    let visible_changed = visible_updates.get(&event.check.number) != Some(&visible);
                    if results.update(&event.check) && !interactive && (ui.json || visible_changed) {
                        ui.check_update(&event.check, streaming);
                    }
                    visible_updates.insert(event.check.number, visible);
                }
                Some(StreamMessage::Connection(index, connected)) => {
                    if connected { disconnected.remove(&index); } else { disconnected.insert(index); }
                    if ui.json && streaming { ui.json_value("connection", &serde_json::json!({"connected": disconnected.is_empty()})); }
                }
                Some(StreamMessage::Error(error)) => return Err(error).context("could not follow checks; remote work was not canceled"),
                None => ended = true,
            },
            _ = timer.tick(), if interactive => browser.tick = browser.tick.wrapping_add(1),
        }
    };
    tasks.shutdown().await;
    drop(terminal);
    if watching {
        ui.watch_closed(results, detached);
    }
    Ok(detached)
}

fn history_request(
    client: &PupClient,
    results: &Results,
    before: Option<u64>,
) -> futures_util::future::BoxFuture<'static, Result<super_types::CheckHistoryPage>> {
    let client = client.clone();
    let workspace = results.workspace_id;
    let selector = results.selector.clone();
    Box::pin(async move { client.history(workspace, Some(&selector), before).await })
}

fn command_selector(repository_root: &Path, current: &Path, selector: &str) -> String {
    let (path, name) = selector
        .rsplit_once("::")
        .map_or((selector, None), |(path, name)| (path, Some(name)));
    let selected = repository_root.join(path);
    let relative = current
        .ancestors()
        .enumerate()
        .find_map(|(depth, ancestor)| {
            selected.strip_prefix(ancestor).ok().map(|tail| {
                let mut relative = std::iter::repeat_n("..", depth).collect::<PathBuf>();
                relative.extend(tail.components());
                relative
            })
        })
        .unwrap_or(selected);
    let mut path = relative.to_string_lossy().into_owned();
    if path.is_empty() {
        path.push('.');
    } else if path.starts_with('-') {
        path.insert_str(0, "./");
    }
    name.map_or(path.clone(), |name| format!("{path}::{name}"))
}

fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._/:-".contains(&b))
    {
        value.to_owned()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

#[cfg(test)]
#[path = "output_tests.rs"]
mod tests;

fn spawn_watches(
    client: &PupClient,
    results: &Results,
    sender: &mpsc::Sender<StreamMessage>,
    tasks: &mut JoinSet<()>,
    watched: &mut HashSet<u64>,
    next_index: &mut usize,
) -> Result<()> {
    let mut check_numbers: Vec<_> = results
        .rows
        .iter()
        .filter_map(|row| row.current.as_ref())
        .chain(results.history.iter())
        .filter(|check| !check.terminal || check.updates_pending)
        .map(|check| check.number)
        .filter(|id| !watched.contains(id))
        .collect();
    check_numbers.sort_unstable();
    check_numbers.dedup();
    if watched.len() + check_numbers.len() > 1024 {
        anyhow::bail!("too many active checks to watch together; select a narrower scope");
    }
    watched.extend(check_numbers.iter().copied());
    for check_numbers in check_numbers.chunks(128) {
        let index = *next_index;
        *next_index += 1;
        let check_numbers = check_numbers.to_vec();
        let client = client.clone();
        let workspace = results.workspace_id;
        let sender = sender.clone();
        tasks.spawn(async move {
            let mut observed = BTreeMap::new();
            let mut delay = Duration::from_millis(250);
            loop {
                let stream = client.check_events(workspace, &check_numbers).await;
                let result = match stream {
                    Ok(mut stream) => {
                        let _ = sender.send(StreamMessage::Connection(index, true)).await;
                        let mut finished = HashSet::new();
                        loop {
                            match stream.next_event().await {
                                Ok(Some(event)) => {
                                    if !check_numbers.contains(&event.check.number)
                                        || event.check.repository_id != workspace
                                    {
                                        continue;
                                    }
                                    if observed.insert(event.check.number, event.sequence) != Some(event.sequence) {
                                        delay = Duration::from_millis(250);
                                    }
                                    if event.check.terminal && !event.check.updates_pending {
                                        finished.insert(event.check.number);
                                    }
                                    if sender.send(StreamMessage::Check(Box::new(event))).await.is_err() {
                                        return;
                                    }
                                    if finished.len() == check_numbers.len() {
                                        return;
                                    }
                                }
                                Ok(None) => break Ok(()),
                                Err(error) => break Err(error),
                            }
                        }
                    }
                    Err(error) => Err(error),
                };
                if let Err(error) = result
                    && !crate::api::is_retryable(&error)
                {
                    let _ = sender.send(StreamMessage::Error(error)).await;
                    return;
                }
                if sender.send(StreamMessage::Connection(index, false)).await.is_err() {
                    return;
                }
                tokio::time::sleep(delay).await;
                delay = delay.saturating_mul(2).min(Duration::from_secs(5));
            }
        });
    }
    Ok(())
}

fn initial_history(
    client: &PupClient,
    results: &mut Results,
    interactive: bool,
) -> Option<futures_util::future::BoxFuture<'static, Result<super_types::CheckHistoryPage>>> {
    if interactive && !results.history_loaded && !results.rows.is_empty() {
        results.history_loading = true;
        Some(history_request(client, results, None))
    } else {
        None
    }
}

fn visible_updates(results: &Results) -> BTreeMap<u64, (String, Option<String>)> {
    results
        .rows
        .iter()
        .filter_map(|row| row.current.as_ref())
        .chain(results.history.iter())
        .map(|check| (check.number, (activity(check), check.presentation.live_line.clone())))
        .collect()
}
