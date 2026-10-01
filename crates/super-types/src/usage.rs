//! Account usage reports shared by the platform and CLI.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
pub struct Repository {
    pub id: Uuid,
    pub name: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Pass,
    Fail,
    Conditional,
    Checking,
    Queued,
    Blocked,
    Canceled,
    Error,
}

impl CheckStatus {
    #[must_use]
    pub fn from_check(check: &crate::Check) -> Self {
        if let Some(result) = &check.result {
            return match result.outcome {
                crate::CheckOutcome::Pass => CheckStatus::Pass,
                crate::CheckOutcome::Fail => CheckStatus::Fail,
                crate::CheckOutcome::Conditional => CheckStatus::Conditional,
            };
        }
        if let Some(error) = check.operational_error {
            return match error {
                crate::CheckOperationalError::Blocked => CheckStatus::Blocked,
                crate::CheckOperationalError::Canceled => CheckStatus::Canceled,
                crate::CheckOperationalError::Error | crate::CheckOperationalError::MissingConclusion => {
                    CheckStatus::Error
                }
            };
        }
        if check.terminal {
            return CheckStatus::Error;
        }
        match check.presentation.status.label.as_str() {
            "queued" | "pending" => CheckStatus::Queued,
            _ => CheckStatus::Checking,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct UsageCheck {
    pub id: String,
    #[cfg_attr(feature = "typescript", ts(type = "number"))]
    pub number: u64,
    pub repository_id: Uuid,
    pub name: String,
    pub path: String,
    pub revision: String,
    pub started_at: DateTime<Utc>,
    pub status: CheckStatus,
    #[cfg_attr(feature = "typescript", ts(type = "number | null"))]
    pub period_tokens: Option<u64>,
    pub usage_pending: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct UsageBucket {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    #[cfg_attr(feature = "typescript", ts(type = "number | null"))]
    pub tokens: Option<u64>,
    pub current: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
pub enum Period {
    #[serde(rename = "today")]
    Today,
    #[serde(rename = "7d")]
    Week,
    #[default]
    #[serde(rename = "30d")]
    Month,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub enum UsageSort {
    #[default]
    StartedAt,
    Tokens,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Ascending,
    #[default]
    Descending,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UsageQuery {
    #[serde(default)]
    pub period: Period,
    pub timezone: String,
    pub repository_id: Option<Uuid>,
    #[serde(default)]
    pub sort: UsageSort,
    #[serde(default)]
    pub direction: Direction,
    #[serde(default = "first_page")]
    pub page: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_size: Option<u32>,
    pub snapshot: Option<Uuid>,
}
const fn first_page() -> u32 {
    1
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct UsageReport {
    pub snapshot: Uuid,
    pub as_of: DateTime<Utc>,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub repositories: Vec<Repository>,
    pub checks: Vec<UsageCheck>,
    pub buckets: Vec<UsageBucket>,
    #[cfg_attr(feature = "typescript", ts(type = "number | null"))]
    pub total_tokens: Option<u64>,
    pub total_checks: u32,
    pub page: u32,
    pub page_size: u32,
    pub usage_available: bool,
    pub pending: bool,
    /// Account-wide allowance; independent of this report's filters and pagination snapshot.
    pub quota: Option<TokenQuota>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct TokenQuota {
    pub as_of: DateTime<Utc>,
    pub window_start: DateTime<Utc>,
    #[cfg_attr(feature = "typescript", ts(type = "number | null"))]
    pub limit: Option<u64>,
    #[cfg_attr(feature = "typescript", ts(type = "number"))]
    pub used: u64,
    #[cfg_attr(feature = "typescript", ts(type = "number | null"))]
    pub remaining: Option<u64>,
    pub exhausted: bool,
    pub next_available_at: Option<DateTime<Utc>>,
}
