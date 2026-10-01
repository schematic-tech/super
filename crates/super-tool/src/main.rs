mod api;
mod cancel;
mod cli;
mod config;
mod daemon;
mod discovery;
mod git;
mod output;
mod updates;
mod usage;

use std::{
    collections::{HashMap, HashSet},
    io::IsTerminal,
    path::Path,
    process::ExitCode,
};

use anyhow::{Context, Result, bail};
use clap::Parser;
use cli::{CheckArgs, Cli, Command, FixArgs, StatusArgs};
use config::{CredentialSource, LocalRepository, LocalRevision, LocalState, StateStore, find_repository};
use output::{Observation, Results, Ui};
use super_types::{Check, CommitRef, CreateChecksRequest, MAX_SUPERTESTS_PER_SUBMISSION};
use uuid::Uuid;

const DEFAULT_API_URL: &str = "https://api.schematic.tech";

#[tokio::main]
async fn main() -> ExitCode {
    let cli = match Cli::try_parse().and_then(Cli::validate) {
        Ok(cli) => cli,
        Err(error) => {
            let mut ui = Ui::new(std::env::args_os().any(|argument| argument == "--json"));
            updates::on_startup(&mut ui);
            if ui.json && error.use_stderr() {
                ui.json_value(
                    "error",
                    &serde_json::json!({"code": "invalid_arguments", "message": error.to_string()}),
                );
                return ExitCode::from(2);
            }
            error.exit();
        }
    };
    if let Command::RefreshReleases { directory } = &cli.command {
        // This worker has no user-facing output and must never start another worker.
        let _ = updates::refresh_detached(directory).await;
        return ExitCode::SUCCESS;
    }
    let mut ui = Ui::new(cli.json);
    if !matches!(cli.command, Command::Daemon) {
        updates::on_startup(&mut ui);
    }
    ui.details = match &cli.command {
        Command::Check(args) => args.details,
        Command::Status(args) => args.details,
        Command::Fix(args) => args.details,
        _ => false,
    };
    match run(cli, &ui).await {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            ui.error(&error);
            ExitCode::from(2)
        }
    }
}

async fn run(cli: Cli, ui: &Ui) -> Result<u8> {
    let store = StateStore::discover()?;
    let api_url = if matches!(
        cli.command,
        Command::Login | Command::Logout | Command::Unlink { .. } | Command::Daemon
    ) {
        String::new()
    } else {
        let base = resolved_api_url(&store, cli.api_url.as_deref())?;
        store.select_api(&base)?;
        base
    };
    match cli.command {
        Command::Login => login(&store, cli.api_url.as_deref(), ui).await?,
        Command::Logout => logout(&store, ui)?,
        Command::Link { path } => link_repository(&store, &api_url, &path, ui).await?,
        Command::Unlink { path } => unlink_repository(&store, path.as_deref(), ui)?,
        Command::Check(args) => return check_command(&store, &api_url, args, ui).await,
        Command::Status(args) => check_status(&store, &api_url, args, ui).await?,
        Command::Usage(args) => usage::run(&store, &api_url, &args, ui).await?,
        Command::Cancel(args) => return cancel_checks(&store, &api_url, &args.target, ui).await,
        Command::Fix(args) => {
            if !args.dry_run && !args.yes && !ui.interactive {
                bail!("noninteractive fix application requires --yes; use --dry-run to preview")
            }
            fix_command(&store, &api_url, args, ui).await?;
        }
        Command::Daemon => daemon::run(store).await?,
        Command::RefreshReleases { .. } => unreachable!("release refresh is handled before normal startup"),
    }
    Ok(0)
}

async fn fix_command(store: &StateStore, api_url: &str, args: FixArgs, ui: &Ui) -> Result<()> {
    let current = std::env::current_dir()?.canonicalize()?;
    let check_id = args.check;
    let (state, local, git) = if check_id.is_some() || args.target.is_none() {
        local_context(store, None)?
    } else {
        selector_context(store, &current, args.target.as_deref().expect("target exists"))?
    };
    let client = authenticated_client(api_url, &state)?;
    let selector = if check_id.is_none() {
        args.target
            .as_deref()
            .map(|target| discovery::canonicalize_selector(&local.root, &current, target))
            .transpose()?
    } else {
        None
    };
    let progress = ui.progress("Fetching fix proposal");
    let check = progress.run(async {
        let mut check = resolve_fix_check(&client, local.workspace_id, check_id, selector.as_deref()).await?;
        if check.fix.is_none() {
            match client.fix(local.workspace_id, check.number).await {
                Ok(fix) => check.fix = Some(fix),
                Err(error) if api::is_not_found(&error) => {},
                Err(error) => return Err(error).context("could not retrieve the Super fix proposal"),
            }
        }
        if check.fix.is_none() {
            if check.terminal && !check.fix_pending {
                bail!("check {} finished without a fix proposal\n  Fixes are advisory and are available only when Super can construct a safe patch.", check.number);
            }
            progress.sender().send_replace(api::SourceProgress::Message(
                if check.fix_pending { "Waiting for fix proposal" } else { "Waiting for check updates" }.into(),
            ));
            check = client.wait_for_fix(local.workspace_id, check.number, check).await?;
        }
        Ok(check)
    }).await?;

    let fix = check
        .fix
        .as_ref()
        .context("Super did not publish an applicable fix proposal")?;
    fix.validate().map_err(anyhow::Error::msg)?;
    if fix.base_revision_id != check.revision.id || fix.base_tree_sha256 != check.revision.tree_sha256 {
        bail!("Super returned a fix for a different source revision")
    }
    let prepare_git = git.clone();
    let prepare_fix = fix.clone();
    let prepared = ui
        .progress("Checking the patch against your working files")
        .run(async {
            tokio::task::spawn_blocking(move || {
                prepare_git.prepare_fix(&prepare_fix.base_tree_sha256, &prepare_fix.diff)
            })
            .await
            .context("fix preparation task failed")?
        })
        .await?;
    if args.dry_run || !ui.json {
        ui.fix_preview(&local.name, &check, fix, &prepared);
    }
    if prepared.source_changed && prepared.apply_error.is_none() {
        Ui::fix_source_warning(check.number);
    }
    if args.dry_run {
        if let Some(error) = &prepared.apply_error {
            ui.notice(format!(
                "Preview only: {error}\n  Recheck your current source before requesting another fix."
            ));
        } else {
            ui.notice(format!(
                "Preview only. Apply with `super fix --check {}`.",
                check.number
            ));
        }
        return Ok(());
    }
    if let Some(error) = &prepared.apply_error {
        bail!("{error}\n  Recheck the current source with `super check --dirty` before requesting another fix.")
    }
    apply_fix(&git, &args, fix, &check, ui, prepared).await
}

async fn apply_fix(
    git: &git::GitRepository,
    args: &FixArgs,
    fix: &super_types::FixProposal,
    check: &Check,
    ui: &Ui,
    prepared: git::PreparedFix,
) -> Result<()> {
    if !args.yes {
        if !std::io::stdin().is_terminal() {
            bail!("refusing to modify a noninteractive worktree without `--yes`")
        }
        if ui.confirm("Apply this fix to your working tree?", true).await? != Some(true) {
            ui.notice("No changes made.");
            return Ok(());
        }
    }
    let apply_git = git.clone();
    let diff = fix.diff.clone();
    let before = prepared.before.clone();
    // Git application is a short critical section. Do not drop a blocking mutation on Ctrl-C.
    let mut application = tokio::task::spawn_blocking(move || apply_git.apply_fix_diff(&before, &diff));
    tokio::select! {
        result = &mut application => result.context("fix application task failed")??,
        signal = tokio::signal::ctrl_c() => {
            signal?;
            application.await.context("fix application task failed after interruption")??;
            bail!("Fix application finished during interruption. The patch is applied but uncommitted; no further action was started.");
        }
    }
    verify_fix_scope(git, fix, &prepared.before)?;
    ui.fix_applied(fix, check, prepared.source_changed, &git.root);
    Ok(())
}

async fn resolve_fix_check(
    client: &api::PupClient,
    workspace_id: Uuid,
    check_id: Option<u64>,
    target: Option<&str>,
) -> Result<Check> {
    if let Some(check_id) = check_id {
        return client
            .check(workspace_id, check_id)
            .await
            .with_context(|| format!("could not retrieve check {check_id}"));
    }
    let checks = if let Some(selector) = target {
        client.latest_checks(workspace_id, selector).await?
    } else {
        client.latest_submission(workspace_id, None).await?.checks
    };
    if target.is_some() && checks.len() != 1 {
        bail!("fix requires one supertest; select path::name or use --check <NUMBER>");
    }
    let mut candidates: Vec<_> = checks
        .into_iter()
        .filter(|check| check.problematic || check.fix.is_some())
        .collect();
    if candidates.len() != 1 {
        bail!(
            "fix requires one problematic supertest; {} match. Select a supertest or use --check <NUMBER>.",
            candidates.len()
        );
    }
    Ok(candidates.remove(0))
}

fn verify_fix_scope(
    git: &git::GitRepository,
    fix: &super_types::FixProposal,
    before: &git::WorktreeSnapshot,
) -> Result<()> {
    let allowed: HashSet<_> = fix.files.iter().map(|file| file.path.as_str()).collect();
    let changed = git.edits_since(before)?;
    if changed.is_empty() {
        bail!("fix application finished without changing the proposed files\n  Review the proposal and retry")
    }
    if changed.iter().any(|path| !allowed.contains(path.as_str())) {
        let unexpected = changed
            .iter()
            .filter(|path| !allowed.contains(path.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        bail!(
            "files changed outside the proposal during fix application: {}\n  Review the worktree before continuing.",
            unexpected.join(", ")
        )
    }
    let missing = allowed
        .iter()
        .filter(|path| !changed.contains(**path))
        .copied()
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        bail!(
            "fix application did not change every proposed file: {}\n  Review the result before accepting it.",
            missing.join(", ")
        )
    }
    Ok(())
}

fn key_api_url(token: Option<&str>, explicit: Option<&str>, saved: Option<&str>) -> Result<String> {
    use super_types::credential::{ApiBase, RoutedKey};
    let routed = token
        .map(RoutedKey::parse)
        .transpose()
        .map_err(anyhow::Error::msg)?
        .flatten();
    if let Some(key) = routed {
        if let Some(explicit) = explicit {
            let explicit = ApiBase::parse(explicit).map_err(anyhow::Error::msg)?;
            if explicit != key.api_base {
                bail!("this key selects a different API; remove SUPER_API_URL or --api-url and retry")
            }
        }
        return Ok(key.api_base.as_str().to_owned());
    }
    Ok(ApiBase::parse(explicit.or(saved).unwrap_or(DEFAULT_API_URL))
        .map_err(anyhow::Error::msg)?
        .as_str()
        .to_owned())
}

fn resolved_api_url(store: &StateStore, explicit: Option<&str>) -> Result<String> {
    let state = store.load()?;
    let token = if let Ok(token) = std::env::var("SUPER_ACCESS_TOKEN") {
        Some(token)
    } else if state.credential.is_some() {
        store.load_api_key()?
    } else {
        None
    };
    key_api_url(
        token.as_deref(),
        explicit,
        state.credential_api_url.as_deref().or(state.api_url.as_deref()),
    )
}

async fn login(store: &StateStore, explicit_api: Option<&str>, ui: &Ui) -> Result<()> {
    let token = std::env::var("SUPER_ACCESS_TOKEN")
        .ok()
        .filter(|token| !token.trim().is_empty())
        .map_or_else(|| ui.api_key(), Ok)?;
    let previous = store.load()?;
    let api_url = key_api_url(
        Some(&token),
        explicit_api,
        previous.credential_api_url.as_deref().or(previous.api_url.as_deref()),
    )?;
    let client = api::PupClient::new(&api_url, Some(&token))?;
    let identity = client
        .whoami()
        .await
        .context("the API key was rejected; create a new key in Schematic Platform or contact support")?;
    store.save_api_key(&token)?;
    store.update(|state| {
        state.select_api(&api_url);
        state.credential_api_url = Some(api_url.clone());
        state.credential = Some(CredentialSource::PupApiKey);
        Ok(())
    })?;
    daemon::ensure_daemon(store)?;
    ui.success(format!("Logged in as {}", identity.email));
    Ok(())
}

fn logout(store: &StateStore, ui: &Ui) -> Result<()> {
    store.clear_api_key()?;
    store.update(|state| {
        state.credential = None;
        Ok(())
    })?;
    ui.success("Logged out");
    Ok(())
}

fn unlink_repository(store: &StateStore, path: Option<&Path>, ui: &Ui) -> Result<()> {
    let (_, local, _) = local_context(store, path)?;
    store.update(|state| {
        state
            .repositories
            .retain(|repository| repository.association_id != local.association_id);
        state
            .unlinked_repositories
            .retain(|repository| repository.root != local.root && repository.association_id != local.association_id);
        state.unlinked_repositories.push(local.clone());
        Ok(())
    })?;
    ui.success(format!("Unlinked · {}", local.name));
    Ok(())
}

async fn link_repository(store: &StateStore, api_url: &str, path: &Path, ui: &Ui) -> Result<()> {
    let state = store.load()?;
    let client = authenticated_client(api_url, &state)?;
    let git = git::GitRepository::discover(path)?;
    require_committed_source(&git)?;
    let head = git.head()?;
    let prior = state
        .repositories
        .iter()
        .chain(&state.unlinked_repositories)
        .find(|repository| repository.root == git.root)
        .cloned();
    let association_id = prior.as_ref().map_or_else(
        || Uuid::new_v4().to_string(),
        |repository| repository.association_id.clone(),
    );
    let known_hashes = prior
        .as_ref()
        .map(|repository| repository.source_hashes.clone())
        .unwrap_or_default();
    let progress = ui.link_progress(&git.name);
    let client = client.with_source_progress(progress.sender());
    let (workspace, revision, snapshot) = progress
        .run(async {
            let workspace = client
                .link_workspace(
                    &git.name,
                    &association_id,
                    prior.as_ref().map(|value| value.workspace_id),
                )
                .await?;
            let parent_id = head
                .parent_oid
                .as_ref()
                .and_then(|oid| prior.as_ref()?.revisions.get(oid).map(|revision| revision.id));
            let (revision, snapshot) = client
                .ensure_revision(
                    workspace.id,
                    &git,
                    &head,
                    api::RevisionCache {
                        parent_id,
                        source_hashes: &known_hashes,
                        revision: prior
                            .as_ref()
                            .and_then(|repository| repository.revisions.get(&head.oid)),
                    },
                )
                .await?;
            Ok((workspace, revision, snapshot))
        })
        .await?;
    if let Some(snapshot) = &snapshot {
        report_exclusions(ui, &snapshot.excluded);
    }
    store.update(|state| {
        let mut revisions = prior.as_ref().map(|value| value.revisions.clone()).unwrap_or_default();
        revisions.insert(
            head.oid.clone(),
            LocalRevision {
                id: revision.id,
                tree_sha256: revision.tree_sha256.clone(),
            },
        );
        let local = LocalRepository {
            root: git.root.clone(),
            common_git_dir: git.common_git_dir.clone(),
            association_id,
            workspace_id: workspace.id,
            name: git.name.clone(),
            _legacy_dirty_preference: None,
            last_seen_oid: Some(head.oid.clone()),
            temporary_commits: prior
                .as_ref()
                .map(|value| value.temporary_commits.clone())
                .unwrap_or_default(),
            source_hashes: snapshot.map_or(known_hashes, |snapshot| snapshot.source_hashes),
            revisions,
            pending_submissions: prior
                .as_ref()
                .map(|value| value.pending_submissions.clone())
                .unwrap_or_default(),
        };
        state
            .unlinked_repositories
            .retain(|repository| repository.root != git.root && repository.association_id != local.association_id);
        if let Some(index) = state.repositories.iter().position(|value| value.root == git.root) {
            state.repositories[index] = local;
        } else {
            state.repositories.push(local);
        }
        state.api_url = Some(api_url.trim_end_matches('/').into());
        Ok(())
    })?;
    daemon::ensure_daemon(store)?;
    ui.workspace(&workspace, &head);
    Ok(())
}

async fn check_command(store: &StateStore, api_url: &str, args: CheckArgs, ui: &Ui) -> Result<u8> {
    let options = StartCheckOptions {
        selector: args.selector.as_deref(),
        problems: args.problems,
        revision: args.source.commit.as_deref(),
        dirty: args.source.dirty,
        observation: (!args.detach).then_some(if args.stream {
            Observation::AttachedStream
        } else {
            Observation::Attached
        }),
    };
    start_checks(store, api_url, options, ui).await
}

#[derive(Debug, Clone, Copy)]
struct StartCheckOptions<'a> {
    selector: Option<&'a str>,
    problems: bool,
    revision: Option<&'a str>,
    dirty: bool,
    observation: Option<Observation>,
}

async fn start_checks(store: &StateStore, api_url: &str, options: StartCheckOptions<'_>, ui: &Ui) -> Result<u8> {
    let current = std::env::current_dir()?.canonicalize()?;
    let (state, local, git) = optional_selector_context(store, &current, options.selector)?;
    let client = authenticated_client(api_url, &state)?;
    let (discovery_directory, selector) = start_check_discovery_scope(&git.root, &current, options.selector);
    let problems = if options.problems {
        let selector = discovery::canonicalize_selector(&git.root, discovery_directory, selector)?;
        let problems = latest_run_problems(&client, local.workspace_id, &selector).await?;
        if problems.is_empty() {
            ui.success(if selector == "." {
                "No problems to recheck in the latest run.".to_owned()
            } else {
                format!("No problems to recheck in `{selector}` from the latest run.")
            });
            return Ok(0);
        }
        Some(problems)
    } else {
        None
    };
    let commit = check_commit(store, &local, &git, options.revision, options.dirty, ui).await?;
    let progress = ui.progress("Syncing source and finding supertests");
    let client = client.with_source_progress(progress.sender());
    let discovery_git = git.clone();
    let discovery_oid = commit.oid.clone();
    let discovery_directory = discovery_directory.to_owned();
    let selector = selector.to_owned();
    let explicit_commit = options.revision.is_some();
    let discovery = tokio::task::spawn_blocking(move || {
        discovery::discover(
            &discovery_git,
            &discovery_oid,
            &discovery_directory,
            &selector,
            explicit_commit,
        )
    });
    let parent_id = commit
        .parent_oid
        .as_ref()
        .and_then(|oid| local.revisions.get(oid).map(|revision| revision.id));
    let upload = client.ensure_revision(
        local.workspace_id,
        &git,
        &commit,
        api::RevisionCache {
            parent_id,
            source_hashes: &local.source_hashes,
            revision: local.revisions.get(&commit.oid),
        },
    );
    let ((revision, snapshot), mut discovered) = progress
        .run(async {
            tokio::try_join!(upload, async {
                discovery.await.context("supertest discovery task failed")?
            })
        })
        .await?;
    if let Some(snapshot) = &snapshot {
        report_exclusions(ui, &snapshot.excluded);
    }
    record_admitted_revision(
        store,
        &local.association_id,
        &commit,
        revision.id,
        revision.tree_sha256.clone(),
        snapshot.map(|snapshot| snapshot.source_hashes),
        options.revision.is_none(),
    )?;
    report_exclusions(ui, &discovered.excluded);
    if let Some(problems) = problems {
        retain_problem_supertests(&problems, &mut discovered, ui)?;
    }
    validate_submission_size(discovered.supertests.len())?;
    let request = create_check_request(revision.id, discovered);
    let (request_sha256, idempotency_key) = submission_idempotency(store, &local.association_id, &request)?;
    let response = submit_checks(&client, local.workspace_id, &request, &idempotency_key, ui).await?;
    complete_submission(store, &local.association_id, &request_sha256)?;
    daemon::ensure_daemon(store)?;
    let mut results = Results::from_submission(&local, Some(commit), response);
    observe_submission(&client, &mut results, options.observation, ui).await
}

async fn observe_submission(
    client: &api::PupClient,
    results: &mut Results,
    observation: Option<Observation>,
    ui: &Ui,
) -> Result<u8> {
    let interrupted = ui.json
        && matches!(observation, Some(Observation::Attached))
        && tokio::select! {
            result = results.load_history(client, None) => { result?; false },
            signal = tokio::signal::ctrl_c() => { signal?; true },
        };
    let detached = if let Some(observation) = observation.filter(|_| !interrupted) {
        match output::observe(client, results, observation, ui).await {
            Ok(detached) => detached,
            Err(error) => {
                ui.closed(results);
                return Err(error);
            }
        }
    } else {
        !results.finished()
    };
    if detached && !ui.json {
        if observation.is_none() {
            ui.accepted(results);
        } else {
            ui.closed(results);
        }
    } else {
        ui.completed(results);
    }

    Ok(if detached || observation.is_none() {
        0
    } else {
        results.exit_code()
    })
}

async fn submit_checks(
    client: &api::PupClient,
    workspace: Uuid,
    request: &CreateChecksRequest,
    idempotency_key: &str,
    ui: &Ui,
) -> Result<super_types::CheckSubmissionResponse> {
    let progress = ui.progress("Starting checks");
    progress.run_with_interrupt(
        client.create_checks(workspace, request, idempotency_key, || {
            progress.sender().send_replace(api::SourceProgress::Message(
                "Waiting for the previous check to finish canceling before starting a new attempt...".into()
            ));
        }),
        "Stopped waiting for check acceptance; any accepted remote work continues. Run super check again to recover this submission.",
    ).await
}

fn create_check_request(revision_id: Uuid, discovery: discovery::Discovery) -> CreateChecksRequest {
    CreateChecksRequest {
        revision_id,
        selector: discovery.canonical_selector,
        certify: false,
        supertests: discovery.supertests,
    }
}

fn report_exclusions(ui: &Ui, exclusions: &[String]) {
    for excluded in exclusions {
        ui.notice(excluded);
    }
}

fn validate_submission_size(supertest_count: usize) -> Result<()> {
    if supertest_count > MAX_SUPERTESTS_PER_SUBMISSION {
        bail!(
            "selection contains {supertest_count} supertests; one check accepts at most {MAX_SUPERTESTS_PER_SUBMISSION}\n  Narrow the selection to a directory, file, or file::supertest."
        )
    }
    Ok(())
}

fn record_admitted_revision(
    store: &StateStore,
    association_id: &str,
    commit: &CommitRef,
    revision_id: Uuid,
    tree_sha256: String,
    source_hashes: Option<HashMap<String, String>>,
    track_head: bool,
) -> Result<()> {
    store.update(|state| {
        if let Some(repository) = state
            .repositories
            .iter_mut()
            .find(|repository| repository.association_id == association_id)
        {
            if let Some(source_hashes) = source_hashes {
                repository.source_hashes = source_hashes;
            }
            repository.revisions.insert(
                commit.oid.clone(),
                LocalRevision {
                    id: revision_id,
                    tree_sha256,
                },
            );
            if track_head && !commit.temporary {
                repository.last_seen_oid = Some(commit.oid.clone());
            }
        }
        Ok(())
    })
}

fn submission_idempotency(
    store: &StateStore,
    association_id: &str,
    request: &CreateChecksRequest,
) -> Result<(String, String)> {
    const MAX_PENDING_SUBMISSIONS: usize = 16;
    const RETENTION_HOURS: i64 = 24;

    let request_sha256 = super_types::source_sha256(&serde_json::to_vec(request)?);
    let request_identity = request_sha256.clone();
    let idempotency_key = store.update(|state| {
        let repository = state
            .repositories
            .iter_mut()
            .find(|repository| repository.association_id == association_id)
            .context("linked repository disappeared from local state")?;
        let oldest = chrono::Utc::now() - chrono::Duration::hours(RETENTION_HOURS);
        repository
            .pending_submissions
            .retain(|pending| pending.created_at >= oldest);
        if let Some(pending) = repository
            .pending_submissions
            .iter()
            .find(|pending| pending.request_sha256 == request_identity)
        {
            return Ok(pending.idempotency_key.clone());
        }
        if repository.pending_submissions.len() >= MAX_PENDING_SUBMISSIONS {
            bail!("too many unresolved check submissions\n  Retry the earlier commands before starting another check.")
        }
        let idempotency_key = format!("pup-check:{}", Uuid::new_v4());
        repository.pending_submissions.push(config::PendingSubmission {
            request_sha256: request_identity,
            idempotency_key: idempotency_key.clone(),
            created_at: chrono::Utc::now(),
        });
        Ok(idempotency_key)
    })?;
    Ok((request_sha256, idempotency_key))
}

fn complete_submission(store: &StateStore, association_id: &str, request_sha256: &str) -> Result<()> {
    store.update(|state| {
        let repository = state
            .repositories
            .iter_mut()
            .find(|repository| repository.association_id == association_id)
            .context("linked repository disappeared from local state")?;
        repository
            .pending_submissions
            .retain(|pending| pending.request_sha256 != request_sha256);
        Ok(())
    })
}

fn start_check_discovery_scope<'a>(
    repository_root: &'a Path,
    current_directory: &'a Path,
    selector: Option<&'a str>,
) -> (&'a Path, &'a str) {
    selector.map_or((repository_root, "."), |selector| (current_directory, selector))
}

async fn check_commit(
    store: &StateStore,
    local: &LocalRepository,
    git: &git::GitRepository,
    revision: Option<&str>,
    dirty: bool,
    ui: &Ui,
) -> Result<CommitRef> {
    let commit = if let Some(revision) = revision {
        git.resolve_commit(revision)
            .with_context(|| format!("could not resolve Git revision `{revision}`"))?
    } else if git.is_dirty()? {
        let authorized = dirty || dirty_authorized(ui).await?;
        if !authorized {
            bail!(
                "the worktree has uncommitted changes\n  Pass `--dirty` to include them or `--commit <revision>` to select committed source."
            )
        }
        let temporary = git.temporary_commit(&local.temporary_commits)?;
        store.update(|state| {
            let repository = state
                .repositories
                .iter_mut()
                .find(|repository| repository.association_id == local.association_id)
                .context("linked repository disappeared from local state")?;
            repository
                .temporary_commits
                .insert(temporary.fingerprint.clone(), temporary.commit.oid.clone());
            Ok(())
        })?;
        temporary.commit
    } else {
        git.head()?
    };
    Ok(commit)
}

async fn latest_run_problems(
    client: &api::PupClient,
    workspace: Uuid,
    selector: &str,
) -> Result<HashSet<(String, String)>> {
    // Match plain `super status`: a path narrows this run, never selects an older run.
    let latest = match client.latest_submission(workspace, None).await {
        Ok(latest) => latest,
        Err(error) if api::is_not_found(&error) => {
            bail!("no previous run to recheck\n  Run `super check` without `--problems` first.")
        }
        Err(error) => return Err(error).context("could not retrieve the latest run"),
    };
    let matching: Vec<_> = latest
        .checks
        .into_iter()
        .filter(|check| discovery::selector_matches(&check.supertest, selector))
        .collect();
    if matching.is_empty() {
        bail!(
            "no supertests in the latest run match `{selector}`\n  Run this selection without `--problems` to check it."
        );
    }
    Ok(matching
        .into_iter()
        .filter(|check| check.problematic)
        .map(|check| (check.supertest.path, check.supertest.name))
        .collect())
}

fn retain_problem_supertests(
    problem_supertests: &HashSet<(String, String)>,
    discovered: &mut discovery::Discovery,
    ui: &Ui,
) -> Result<()> {
    discovered
        .supertests
        .retain(|supertest| problem_supertests.contains(&(supertest.path.clone(), supertest.name.clone())));
    if discovered.supertests.is_empty() {
        bail!(
            "the previously problematic supertests no longer exist in `{}`",
            discovered.canonical_selector
        )
    }
    ui.notice(format!(
        "Rechecking {} problematic {}.",
        discovered.supertests.len(),
        if discovered.supertests.len() == 1 {
            "supertest"
        } else {
            "supertests"
        }
    ));
    Ok(())
}

async fn dirty_authorized(ui: &Ui) -> Result<bool> {
    if !ui.interactive {
        bail!(
            "uncommitted changes require an explicit choice in a noninteractive terminal\n  Pass `--dirty` to include them or `--commit <revision>` to select committed source."
        )
    }
    ui.confirm(
        "Uncommitted changes found. Proceed with the check, including these changes?",
        true,
    )
    .await?
    .context("Check canceled.")
}

async fn check_status(store: &StateStore, api_url: &str, args: StatusArgs, ui: &Ui) -> Result<()> {
    let current = std::env::current_dir()?.canonicalize()?;
    let target = target_value(&args.target);
    let (state, local, git) = match target {
        Some(Target::Selector(selector)) => selector_context(store, &current, selector)?,
        _ => local_context(store, None)?,
    };
    let client = authenticated_client(api_url, &state)?;
    let head = git.head()?;
    let mut results = ui
        .progress("Loading checks")
        .run(async {
            Ok(match target {
                Some(Target::Check(number)) => {
                    let check = client.check(local.workspace_id, number).await?;
                    Results::focused(&local, head, check)
                }
                Some(Target::Run(id)) => {
                    Results::from_submission(&local, Some(head), client.submission(local.workspace_id, id).await?)
                }
                Some(Target::Selector(selector)) => {
                    let selector = discovery::canonicalize_selector(&local.root, &current, selector)?;
                    let checks = client.latest_checks(local.workspace_id, &selector).await?;
                    Results::from_checks(&local, head, selector, checks)
                }
                None => match client.latest_submission(local.workspace_id, None).await {
                    Ok(response) => Results::from_submission(&local, Some(head), response),
                    Err(error) if api::is_not_found(&error) => Results::empty(&local, head),
                    Err(error) => return Err(error),
                },
            })
        })
        .await?;
    let include_history = args.history
        || args.before.is_some()
        || matches!(target, Some(Target::Selector(_)))
        || (ui.json && !matches!(target, Some(Target::Check(_))));
    // All targets are resolved before observation; history is optional browser context.
    let watch_without_explicit_history = args.watch && ui.json && !args.history && args.before.is_none();
    if include_history && !watch_without_explicit_history && !(args.watch && ui.interactive) {
        ui.progress("Loading check history")
            .run(results.load_history(&client, args.before))
            .await?;
    } else if args.watch && args.before.is_some() {
        // An explicit history page is the requested data, rather than optional browser context.
        ui.progress("Loading check history")
            .run(results.load_history(&client, args.before))
            .await?;
    }
    if args.watch {
        match output::observe(&client, &mut results, Observation::Watch, ui).await {
            Ok(_) => ui.closed(&results),
            Err(error) => {
                ui.closed(&results);
                return Err(error);
            }
        }
    } else {
        ui.results(&results, args.history || (ui.json && results.rows.len() == 1));
    }
    Ok(())
}

async fn cancel_checks(store: &StateStore, api_url: &str, target: &cli::TargetArgs, ui: &Ui) -> Result<u8> {
    let current = std::env::current_dir()?.canonicalize()?;
    let parsed = target_value(target).context("cancel requires a path, --run, or --check")?;
    let (state, local, git) = match parsed {
        Target::Selector(selector) => selector_context(store, &current, selector)?,
        _ => local_context(store, None)?,
    };
    let client = authenticated_client(api_url, &state)?;
    let mut results = match parsed {
        Target::Check(number) => Results::focused(&local, git.head()?, client.check(local.workspace_id, number).await?),
        Target::Run(id) => Results::from_submission(
            &local,
            Some(git.head()?),
            client.submission(local.workspace_id, id).await?,
        ),
        Target::Selector(selector) => {
            let selector = discovery::canonicalize_selector(&local.root, &current, selector)?;
            let checks = client.latest_checks(local.workspace_id, &selector).await?;
            if checks.is_empty() {
                bail!("no checks match {selector}");
            }
            Results::from_checks(&local, git.head()?, selector, checks)
        }
    };
    let mut entries = cancel::entries(&results);
    let active = entries
        .iter()
        .filter(|entry| entry.outcome == cancel::Outcome::Unconfirmed)
        .count();
    let mut interrupted = None;
    if active > 0 {
        let label = if let [row] = results.rows.as_slice() {
            format!("Canceling {} · #{}", row.supertest.name, entries[0].check_number)
        } else {
            format!("Canceling {active} checks")
        };
        interrupted = ui
            .progress(label)
            .run_with_interrupt(
                async {
                    cancel::execute(&client, &mut results, &mut entries).await;
                    Ok(())
                },
                "Stopped waiting. Cancellation requests already sent may still take effect.",
            )
            .await
            .err();
    }
    ui.cancellation(&results, &entries, interrupted.as_ref());
    Ok(
        if interrupted.is_some()
            || entries
                .iter()
                .any(|entry| entry.error.is_some() || entry.outcome == cancel::Outcome::Unconfirmed)
        {
            2
        } else {
            0
        },
    )
}

fn optional_selector_context(
    store: &StateStore,
    current: &Path,
    selector: Option<&str>,
) -> Result<(LocalState, LocalRepository, git::GitRepository)> {
    selector.map_or_else(
        || local_context(store, None),
        |selector| selector_context(store, current, selector),
    )
}

fn selector_context(
    store: &StateStore,
    current: &Path,
    selector: &str,
) -> Result<(LocalState, LocalRepository, git::GitRepository)> {
    let target = discovery::selector_target(current, selector)?;
    // A selector can name a file or a path that exists only in the selected commit.
    let directory = target
        .ancestors()
        .find(|path| path.is_dir())
        .with_context(|| format!("could not resolve the directory containing `{selector}`"))?;
    local_context(store, Some(directory))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Target<'a> {
    Run(Uuid),
    Check(u64),
    Selector(&'a str),
}

fn target_value(args: &cli::TargetArgs) -> Option<Target<'_>> {
    args.run
        .map(Target::Run)
        .or_else(|| args.check.map(Target::Check))
        .or_else(|| args.selector.as_deref().map(Target::Selector))
}

fn authenticated_client(api_url: &str, state: &LocalState) -> Result<api::PupClient> {
    if state.api_url.as_deref().is_some_and(|saved| saved != api_url) {
        bail!("the active API environment changed during this command; retry the command")
    }
    if let Ok(token) = std::env::var("SUPER_ACCESS_TOKEN") {
        if token.trim().is_empty() || token.chars().any(char::is_control) {
            bail!("SUPER_ACCESS_TOKEN is empty or invalid")
        }
        return client_for_api_key(api_url, &token);
    }
    match state.credential {
        Some(CredentialSource::PupApiKey) => {
            let token = state_store_api_key(state)?;
            client_for_api_key(api_url, &token)
        }
        None => bail!("authentication is required\n  Run `super login`, then retry."),
    }
}

fn client_for_api_key(api_url: &str, token: &str) -> Result<api::PupClient> {
    if key_api_url(Some(token), None, Some(api_url))? != api_url {
        bail!("the saved API key changed environments during this command; retry the command")
    }
    api::PupClient::new(api_url, Some(token))
}

fn state_store_api_key(_state: &LocalState) -> Result<String> {
    // The key is deliberately not part of LocalState.  Callers use this helper only after loading
    // the state so the credential marker and private file cannot drift silently.
    let store = StateStore::discover()?;
    store
        .load_api_key()?
        .with_context(|| "Schematic API key is missing; run `super login` again")
}

fn local_context(store: &StateStore, path: Option<&Path>) -> Result<(LocalState, LocalRepository, git::GitRepository)> {
    let state = store.load()?;
    let directory = path.map_or_else(std::env::current_dir, |path| Ok(path.to_path_buf()))?;
    let git = git::GitRepository::discover(&directory)?;
    let local = find_repository(&state, &git.root).cloned().with_context(|| {
        format!(
            "repository is not linked\n  Run `super link .` from {}.",
            git.root.display()
        )
    })?;
    Ok((state, local, git))
}

fn require_committed_source(git: &git::GitRepository) -> Result<()> {
    if !git.has_commits()? {
        bail!(
            "Repository has no commits\n  super needs one local commit to establish source identity.\n  From {}, run `git add -A && git commit -m \"Initial commit\"`, then retry.\n  A Git remote is not required.",
            git.root.display()
        )
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_origin_overrides_saved_settings_but_rejects_conflicting_explicit_settings() {
        let key = super_types::credential::RoutedKey {
            id: Uuid::new_v4(),
            api_base: super_types::credential::ApiBase::parse("https://api.staging.schematic.tech").unwrap(),
        }
        .encode(&[42; 32]);
        assert_eq!(
            key_api_url(Some(&key), None, Some(DEFAULT_API_URL)).unwrap(),
            "https://api.staging.schematic.tech"
        );
        assert!(key_api_url(Some(&key), Some(DEFAULT_API_URL), None).is_err());
        assert_eq!(
            key_api_url(Some("pup_live_legacy"), None, Some("https://legacy.example.test")).unwrap(),
            "https://legacy.example.test"
        );
        assert!(key_api_url(Some("pup_v2.invalid"), None, None).is_err());
    }

    #[test]
    fn bare_check_uses_repository_scope_while_explicit_dot_uses_current_directory() {
        let repository = Path::new("/repository");
        let nested = Path::new("/repository/supertests/strings");

        assert_eq!(start_check_discovery_scope(repository, nested, None), (repository, "."));
        assert_eq!(
            start_check_discovery_scope(repository, nested, Some(".")),
            (nested, ".")
        );
    }
}
