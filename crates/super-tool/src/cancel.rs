//! Cancellation acknowledges a request separately from the eventual check outcome.
use std::time::Duration;

use futures_util::{StreamExt, stream};
use serde::Serialize;
use super_types::{Check, CheckOperationalError};

use crate::{api::PupClient, output::Results};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const CONFIRMATION_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    AlreadyFinished,
    AlreadyCanceled,
    Canceled,
    BackgroundStopped,
    Finished,
    Requested,
    Unconfirmed,
}

#[derive(Debug, Serialize)]
pub struct Entry {
    pub check_number: u64,
    pub outcome: Outcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub fn active(check: &Check) -> bool {
    !check.terminal || check.updates_pending
}

pub fn canceled(check: &Check) -> bool {
    check.operational_error == Some(CheckOperationalError::Canceled)
}

pub fn entries(results: &Results) -> Vec<Entry> {
    results
        .rows
        .iter()
        .filter_map(|row| row.current.as_ref())
        .map(|check| Entry {
            check_number: check.number,
            outcome: if active(check) {
                // Also truthful if interrupted before this request is sent.
                Outcome::Unconfirmed
            } else if canceled(check) {
                Outcome::AlreadyCanceled
            } else {
                Outcome::AlreadyFinished
            },
            error: None,
        })
        .collect()
}

fn confirmed(check: &Check) -> Outcome {
    if active(check) {
        Outcome::Requested
    } else if canceled(check) {
        if check.result.is_some() {
            Outcome::BackgroundStopped
        } else {
            Outcome::Canceled
        }
    } else {
        Outcome::Finished
    }
}

/// Mutate the report as requests finish, so interruption cannot hide partial success.
pub async fn execute(client: &PupClient, results: &mut Results, entries: &mut [Entry]) {
    let targets: Vec<_> = entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry.outcome == Outcome::Unconfirmed)
        .map(|(index, entry)| (index, entry.check_number))
        .collect();
    let workspace = results.workspace_id;
    let mut requests = stream::iter(targets)
        .map(|(index, number)| async move {
            (
                index,
                tokio::time::timeout(REQUEST_TIMEOUT, client.cancel(workspace, number)).await,
            )
        })
        .buffer_unordered(8);
    while let Some((index, response)) = requests.next().await {
        let entry = &mut entries[index];
        match response {
            Ok(Ok(check)) => {
                entry.outcome = confirmed(&check);
                results.update(&check);
            }
            Ok(Err(error)) => entry.error = Some(format!("{error:#}")),
            Err(_) => entry.error = Some("Timed out waiting for the cancellation response.".into()),
        }
    }

    let pending: Vec<_> = entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry.outcome == Outcome::Requested)
        .map(|(index, entry)| {
            (
                index,
                results
                    .rows
                    .iter()
                    .filter_map(|row| row.current.as_ref())
                    .find(|check| check.number == entry.check_number)
                    .expect("cancellation target exists")
                    .clone(),
            )
        })
        .collect();
    // One bounded confirmation period for the entire selection. A slow or disconnected
    // stream leaves the acknowledged request visible and supplies a status follow-up.
    let deadline = tokio::time::Instant::now() + CONFIRMATION_TIMEOUT;
    let mut confirmations = stream::iter(pending)
        .map(|(index, mut check)| async move {
            let result = tokio::time::timeout_at(deadline, client.wait_for_stop(&mut check)).await;
            // Keep every observed snapshot, including when confirmation times out.
            (index, check, result.unwrap_or(Ok(())))
        })
        .buffer_unordered(8);
    while let Some((index, check, result)) = confirmations.next().await {
        results.update(&check);
        entries[index].outcome = confirmed(&check);
        entries[index].error = result.err().map(|error| format!("{error:#}"));
    }
}
