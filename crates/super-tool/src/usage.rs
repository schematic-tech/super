use anyhow::{Context, Result, bail};
use chrono_tz::Tz;
use super_types::usage::{Direction, Period, Repository, UsageQuery, UsageSort};
use uuid::Uuid;

use crate::{
    cli::UsageArgs,
    config::{StateStore, find_repository},
    git::GitRepository,
    output::Ui,
};

pub async fn run(store: &StateStore, api_url: &str, args: &UsageArgs, ui: &Ui) -> Result<()> {
    let state = store.load()?;
    let client = crate::authenticated_client(api_url, &state)?;
    let timezone = local_timezone();
    let repository_id = match args.repo.as_deref() {
        Some(".") => {
            let git = GitRepository::discover(&std::env::current_dir()?)?;
            Some(find_repository(&state, &git.root)
                .context("this repository is not linked to Super\n  Run `super usage` for all repositories, or select a repository with `--repo NAME`.")?
                .workspace_id)
        }
        Some(value) => Uuid::parse_str(value).ok(),
        None => None,
    };
    let mut query = UsageQuery {
        period: match args.period.as_str() {
            "today" => Period::Today,
            "7d" => Period::Week,
            _ => Period::Month,
        },
        timezone: timezone.name().into(),
        repository_id,
        sort: UsageSort::StartedAt,
        direction: Direction::Descending,
        page: 1,
        page_size: None,
        snapshot: None,
    };
    let report = ui
        .progress("Loading usage")
        .run_with_interrupt(
            async {
                if let Some(name) = args.repo.as_deref().filter(|_| query.repository_id.is_none()) {
                    let repositories = client.repositories().await?;
                    query.repository_id = Some(repository_named(&repositories, name)?);
                }
                client.usage(&query).await
            },
            "Usage request interrupted. Run `super usage` to retry.",
        )
        .await?;
    if ui.json {
        ui.json_value("usage", &report);
    } else {
        Ui::usage_report(&report, &query, timezone);
    }
    Ok(())
}

fn local_timezone() -> Tz {
    // TZ also lets scripts request reproducible calendar boundaries and timestamps.
    std::env::var("TZ")
        .ok()
        .and_then(|name| name.parse().ok())
        .or_else(|| iana_time_zone::get_timezone().ok().and_then(|name| name.parse().ok()))
        .unwrap_or(chrono_tz::UTC)
}

fn repository_named(repositories: &[Repository], name: &str) -> Result<Uuid> {
    let matches: Vec<_> = repositories
        .iter()
        .filter(|repository| repository.name == name)
        .collect();
    match matches.as_slice() {
        [repository] => Ok(repository.id),
        [] => bail!(
            "no repository named `{name}` was found in your account\n  Run `super usage --json` to see repository names and IDs."
        ),
        _ => bail!(
            "more than one repository is named `{name}`\n  Select one with `--repo ID`: {}",
            matches
                .iter()
                .map(|repository| repository.id.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}
