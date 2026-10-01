//! Types for Super API requests, results, source uploads, and events.

pub mod credential;
pub mod usage;

use std::collections::BTreeSet;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

pub const MAX_SOURCE_FILES: usize = 100_000;
pub const MAX_SOURCE_FILE_BYTES: u64 = 10 * 1024 * 1024;
pub const MAX_SUPERTESTS_PER_SUBMISSION: usize = 128;

pub mod api {
    pub const EXHAUSTED_ERROR_CODE: &str = "exhausted";
    pub const AUTH_WHOAMI: &str = "/v1/super/auth/whoami";
    pub const USAGE: &str = "/v1/super/usage";
    pub const WORKSPACES: &str = "/v1/super/workspaces";

    #[must_use]
    pub fn workspace(id: impl std::fmt::Display) -> String {
        format!("/v1/super/workspaces/{id}")
    }

    #[must_use]
    pub fn workspace_revisions(id: impl std::fmt::Display) -> String {
        format!("/v1/super/workspaces/{id}/revisions")
    }

    #[must_use]
    pub fn workspace_revision(id: impl std::fmt::Display) -> String {
        format!("/v1/super/workspace-revisions/{id}")
    }

    #[must_use]
    pub fn content_upload(id: impl std::fmt::Display, sha256: &str) -> String {
        format!("/v1/super/workspace-revisions/{id}/content/{sha256}/upload")
    }

    #[must_use]
    pub fn verify_content(id: impl std::fmt::Display, sha256: &str) -> String {
        format!("/v1/super/workspace-revisions/{id}/content/{sha256}/verify")
    }

    #[must_use]
    pub fn finalize_revision(id: impl std::fmt::Display) -> String {
        format!("/v1/super/workspace-revisions/{id}/finalize")
    }

    #[must_use]
    pub fn check_submissions(id: impl std::fmt::Display) -> String {
        format!("/v1/super/workspaces/{id}/check-submissions")
    }

    #[must_use]
    pub fn latest_check_submission(id: impl std::fmt::Display) -> String {
        format!("/v1/super/workspaces/{id}/check-submissions/latest")
    }

    #[must_use]
    pub fn check_submission(id: impl std::fmt::Display, run: impl std::fmt::Display) -> String {
        format!("/v1/super/workspaces/{id}/check-submissions/{run}")
    }

    #[must_use]
    pub fn check_history(id: impl std::fmt::Display) -> String {
        format!("/v1/super/workspaces/{id}/checks/history")
    }

    #[must_use]
    pub fn checks(id: impl std::fmt::Display) -> String {
        format!("/v1/super/workspaces/{id}/checks")
    }

    #[must_use]
    pub fn check_events(id: impl std::fmt::Display) -> String {
        format!("/v1/super/workspaces/{id}/check-events")
    }

    #[must_use]
    pub fn check(id: impl std::fmt::Display, check_id: u64) -> String {
        format!("/v1/super/workspaces/{id}/checks/{check_id}")
    }

    #[must_use]
    pub fn fix(id: impl std::fmt::Display, check_id: u64) -> String {
        format!("/v1/super/workspaces/{id}/checks/{check_id}/fix")
    }

    #[must_use]
    pub fn cancel_check(id: impl std::fmt::Display, check_id: u64) -> String {
        format!("/v1/super/workspaces/{id}/checks/{check_id}/cancel")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ApiError {
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ApiErrorEnvelope {
    pub error: ApiError,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    pub email: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CreateWorkspace {
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Workspace {
    pub id: Uuid,
    pub name: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RevisionFile {
    pub path: String,
    pub sha256: String,
    pub bytes: u64,
    #[serde(default)]
    pub executable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CreateWorkspaceRevision {
    #[serde(default)]
    pub parent_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reported_git_commit: Option<String>,
    pub tree_sha256: String,
    #[serde(default)]
    pub files: Vec<RevisionFile>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceRevisionState {
    Uploading,
    Materializing,
    Complete,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceRevision {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub parent_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reported_git_commit: Option<String>,
    pub tree_sha256: String,
    pub files: Vec<RevisionFile>,
    pub state: WorkspaceRevisionState,
    pub created_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceDiff {
    pub revision_id: Uuid,
    pub parent_id: Option<Uuid>,
    pub added: Vec<String>,
    pub changed: Vec<String>,
    pub removed: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MissingContent {
    pub sha256: String,
    pub bytes: u64,
    pub stored_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceRevisionAdmission {
    pub revision: WorkspaceRevision,
    pub diff: WorkspaceDiff,
    pub missing_content: Vec<MissingContent>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct UploadPlan {
    pub staged_upload_id: String,
    pub sha256: String,
    pub bytes: u64,
    pub initiation_url: String,
    pub method: String,
    pub headers: std::collections::BTreeMap<String, String>,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct UploadRequest {
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct VerifyUploadRequest {
    pub staged_upload_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CommitRef {
    pub oid: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default)]
    pub temporary: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_oid: Option<String>,
}

impl CommitRef {
    #[must_use]
    pub fn short_oid(&self) -> &str {
        &self.oid[..self.oid.len().min(7)]
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SourceManifest {
    pub tree_sha256: String,
    pub files: Vec<SourceFile>,
}

impl SourceManifest {
    /// Validates the exact complete-tree identity sent to Super API.
    ///
    /// # Errors
    ///
    /// Returns a stable diagnostic for a malformed path, digest, size, duplicate, or tree identity.
    pub fn validate(&self) -> Result<(), String> {
        if self.files.len() > MAX_SOURCE_FILES {
            return Err(format!(
                "a source manifest may contain at most {MAX_SOURCE_FILES} files"
            ));
        }
        let mut paths = BTreeSet::new();
        for file in &self.files {
            if !valid_source_path(&file.path) {
                return Err(format!("source path '{}' is not a normalized relative path", file.path));
            }
            if !paths.insert(&file.path) {
                return Err(format!("source path '{}' appears more than once", file.path));
            }
            if !is_lower_sha256(&file.sha256) {
                return Err(format!("source file '{}' has an invalid SHA-256 identity", file.path));
            }
            if file.bytes > MAX_SOURCE_FILE_BYTES {
                return Err(format!(
                    "source file '{}' exceeds the {} byte file-size limit",
                    file.path, MAX_SOURCE_FILE_BYTES
                ));
            }
        }
        if self.tree_sha256 != source_tree_sha256(&self.files)? {
            return Err("source tree SHA-256 does not match its complete file manifest".to_owned());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SourceFile {
    pub path: String,
    pub sha256: String,
    pub bytes: u64,
    pub executable: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SourceLanguage {
    C,
    Rust,
    Python,
    #[serde(rename = "csharp")]
    CSharp,
    #[serde(rename = "javascript")]
    JavaScript,
    Java,
    Vhdl,
}

impl std::fmt::Display for SourceLanguage {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::C => "c",
            Self::Rust => "rust",
            Self::Python => "python",
            Self::CSharp => "csharp",
            Self::JavaScript => "javascript",
            Self::Java => "java",
            Self::Vhdl => "vhdl",
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Supertest {
    pub path: String,
    pub name: String,
    pub language: SourceLanguage,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
}

impl Supertest {
    #[must_use]
    pub fn selector(&self) -> String {
        format!("{}::{}", self.path, self.name)
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FixProposalState {
    Proposed,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FixFileChange {
    Added,
    Modified,
    Deleted,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FixFile {
    pub path: String,
    pub change: FixFileChange,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FixProposal {
    pub id: Uuid,
    pub state: FixProposalState,
    pub base_revision_id: Uuid,
    pub base_tree_sha256: String,
    pub summary: String,
    pub diff: String,
    pub instructions: String,
    pub files: Vec<FixFile>,
    pub validation: Vec<String>,
    pub created_at: DateTime<Utc>,
}

impl FixProposal {
    pub const MAX_DIFF_BYTES: usize = 512 * 1024;
    pub const MAX_FILES: usize = 64;
    pub const MAX_VALIDATION_ITEMS: usize = 32;

    /// Revalidates the bounded patch before the public tool applies it locally.
    ///
    /// # Errors
    ///
    /// Returns a diagnostic when identity, text, paths, or declared patch scope is malformed.
    pub fn validate(&self) -> Result<(), String> {
        if !is_lower_hex_len(&self.base_tree_sha256, 64) {
            return Err("fix base tree identity is not a lowercase SHA-256 digest".into());
        }
        if self.summary.trim().is_empty() || self.summary.len() > 512 || self.summary.chars().any(char::is_control) {
            return Err("fix summary must be one nonempty printable line of at most 512 bytes".into());
        }
        if self.diff.trim().is_empty()
            || self.diff.len() > Self::MAX_DIFF_BYTES
            || self.diff.contains('\0')
            || self.diff.contains("GIT binary patch")
            || self.diff.contains("Submodule ")
        {
            return Err("fix patch must be a bounded text diff without binary or submodule records".into());
        }
        let diff_paths = fix_diff_paths(&self.diff)?;
        if self.files.is_empty() || self.files.len() > Self::MAX_FILES {
            return Err("fix must declare between one and 64 changed files".into());
        }
        let mut declared = BTreeSet::new();
        for file in &self.files {
            if !valid_source_path(&file.path) || file.path.chars().any(char::is_control) {
                return Err(format!("fix file path '{}' is not a safe relative path", file.path));
            }
            if !declared.insert(file.path.as_str()) || !diff_paths.contains(file.path.as_str()) {
                return Err("fix file declarations must be unique and match the unified diff exactly".into());
            }
        }
        if declared.len() != diff_paths.len() {
            return Err("fix file declarations must match every unified-diff path exactly".into());
        }
        if self.instructions.trim().is_empty()
            || self.instructions.len() > 16 * 1024
            || self
                .instructions
                .chars()
                .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
        {
            return Err("fix instructions must be printable text of at most 16 KiB".into());
        }
        if self.validation.len() > Self::MAX_VALIDATION_ITEMS
            || self.validation.iter().any(|item| {
                item.trim().is_empty()
                    || item.len() > 512
                    || item
                        .chars()
                        .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
            })
        {
            return Err("fix validation commands must contain at most 32 printable items of at most 512 bytes".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PresentationTone {
    Success,
    Warning,
    Error,
    Active,
    Muted,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct StatusPresentation {
    pub marker: String,
    pub label: String,
    pub tone: PresentationTone,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PresentationLine {
    pub text: String,
    pub emphasized: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HistoryPresentation {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome_group: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collapse_group: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CheckPresentation {
    pub status: StatusPresentation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live_line: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity_label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity_updated_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub details: Vec<PresentationLine>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_problem_label: Option<String>,
    pub history: HistoryPresentation,
}

/// Semantic result data; terminality and operational failures remain independent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CheckResult {
    pub outcome: CheckOutcome,
    pub assurance: CheckAssurance,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CheckOutcome {
    Pass,
    Fail,
    Conditional,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CheckAssurance {
    Certified,
    Uncertified,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
#[allow(clippy::struct_excessive_bools)] // Independent public result and delivery states.
pub struct Check {
    pub repository_id: Uuid,
    pub number: u64,
    pub supertest: Supertest,
    pub revision: CheckRevision,
    pub terminal: bool,
    pub problematic: bool,
    #[serde(default)]
    pub result: Option<CheckResult>,
    /// The check could not complete normally; distinct from a reported code problem.
    #[serde(default)]
    pub operational_error: Option<CheckOperationalError>,
    pub presentation: CheckPresentation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix: Option<FixProposal>,
    /// A proposal may still arrive; this does not indicate active preparation.
    #[serde(default)]
    pub fix_pending: bool,
    /// Further updates may arrive even when `terminal` is true.
    #[serde(default)]
    pub updates_pending: bool,
    #[serde(default)]
    pub event_sequence: u64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CheckRevision {
    pub id: Uuid,
    pub tree_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reported_git_commit: Option<String>,
}

impl CheckRevision {
    /// A compact display identity. The content tree remains authoritative; a reported Git OID is
    /// only preferred when present because it is more familiar to a local Git user.
    #[must_use]
    pub fn short_identity(&self) -> &str {
        let value = self.reported_git_commit.as_deref().unwrap_or(&self.tree_sha256);
        &value[..value.len().min(7)]
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CheckEvent {
    pub sequence: u64,
    pub check: Check,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CreateChecksRequest {
    pub revision_id: Uuid,
    pub selector: String,
    #[serde(default)]
    pub certify: bool,
    pub supertests: Vec<Supertest>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CheckSubmission {
    pub id: Uuid,
    pub repository_id: Uuid,
    pub selector: String,
    pub check_numbers: Vec<u64>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CheckSubmissionResponse {
    pub submission: CheckSubmission,
    pub checks: Vec<Check>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CheckHistoryPage {
    pub checks: Vec<Check>,
    pub next_before: Option<u64>,
}

#[must_use]
pub fn source_sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// Computes the canonical `pup-workspace-tree-v1` source identity accepted by Super API.
///
/// # Errors
///
/// Returns an error when a content digest or path cannot be encoded canonically.
pub fn source_tree_sha256(files: &[SourceFile]) -> Result<String, String> {
    let mut files = files.iter().collect::<Vec<_>>();
    files.sort_by(|left, right| left.path.cmp(&right.path));
    let mut digest = Sha256::new();
    digest.update(b"pup-workspace-tree-v1\0");
    for file in files {
        if !is_lower_sha256(&file.sha256) {
            return Err(format!("source file '{}' has an invalid SHA-256 identity", file.path));
        }
        let path = file.path.as_bytes();
        let path_length = u32::try_from(path.len()).map_err(|_| "source path is too long".to_owned())?;
        let mut content_digest = [0_u8; 32];
        hex::decode_to_slice(&file.sha256, &mut content_digest)
            .map_err(|_| format!("source file '{}' has an invalid SHA-256 identity", file.path))?;
        digest.update(path_length.to_be_bytes());
        digest.update(path);
        digest.update(content_digest);
        digest.update(file.bytes.to_be_bytes());
        digest.update([u8::from(file.executable)]);
    }
    Ok(hex::encode(digest.finalize()))
}

fn valid_source_path(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.ends_with('/')
        && !path
            .split('/')
            .any(|component| component.is_empty() || matches!(component, "." | ".."))
}

fn fix_diff_paths(diff: &str) -> Result<BTreeSet<&str>, String> {
    let mut paths = BTreeSet::new();
    for line in diff.lines() {
        let Some(rest) = line.strip_prefix("diff --git a/") else {
            continue;
        };
        let Some((left, right)) = rest.split_once(" b/") else {
            return Err("each fix diff header must name matching a/ and b/ paths".into());
        };
        if left.is_empty() || left != right || !valid_source_path(left) {
            return Err("fix diffs may not contain malformed paths or renames".into());
        }
        paths.insert(left);
    }
    if paths.is_empty() {
        return Err("fix patch must contain at least one unified-diff header".into());
    }
    Ok(paths)
}

fn is_lower_hex(value: &str) -> bool {
    value
        .bytes()
        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn is_lower_hex_len(value: &str, length: usize) -> bool {
    value.len() == length && is_lower_hex(value)
}

fn is_lower_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_routes_use_the_versioned_pup_namespace() {
        let id = Uuid::nil();
        assert_eq!(api::AUTH_WHOAMI, "/v1/super/auth/whoami");
        assert_eq!(api::WORKSPACES, "/v1/super/workspaces");
        for route in [
            api::workspace(id),
            api::workspace_revisions(id),
            api::workspace_revision(id),
            api::content_upload(id, "digest"),
            api::verify_content(id, "digest"),
            api::finalize_revision(id),
            api::check_submissions(id),
            api::latest_check_submission(id),
            api::checks(id),
            api::check_events(id),
            api::check(id, 1),
            api::fix(id, 1),
            api::cancel_check(id, 1),
        ] {
            assert!(route.starts_with("/v1/super/"), "unexpected route: {route}");
        }
    }

    #[test]
    fn public_status_text_is_deserialized_without_interpretation() {
        let presentation: CheckPresentation = serde_json::from_value(serde_json::json!({
            "status": {
                "marker": "!",
                "label": "server-owned wording",
                "tone": "warning"
            },
            "details": [{"text": "server-owned evidence", "emphasized": true}],
            "history": {"outcome_group": "opaque"}
        }))
        .expect("public presentation");
        assert_eq!(presentation.status.label, "server-owned wording");
        assert_eq!(presentation.details[0].text, "server-owned evidence");
    }

    #[test]
    fn public_check_admission_exposes_intent_but_no_internal_profile() {
        let request = CreateChecksRequest {
            revision_id: Uuid::nil(),
            selector: "tests/example.c::identity_holds".to_owned(),
            certify: false,
            supertests: Vec::new(),
        };
        let value = serde_json::to_value(request).unwrap();
        assert_eq!(value["certify"], false);
        assert!(value.get("profile").is_none());
        assert!(value.get("assurance").is_none());
    }

    #[test]
    fn workspace_revision_uses_optional_reported_git_provenance() {
        let request = CreateWorkspaceRevision {
            parent_id: None,
            reported_git_commit: Some("a".repeat(40)),
            tree_sha256: "b".repeat(64),
            files: Vec::new(),
        };
        let value = serde_json::to_value(request).unwrap();
        assert_eq!(value["reported_git_commit"], "a".repeat(40));
        assert!(value.get("git_commit").is_none());

        assert!(
            serde_json::from_value::<CreateWorkspaceRevision>(serde_json::json!({
                "git_commit": "a".repeat(40),
                "tree_sha256": "b".repeat(64),
                "files": []
            }))
            .is_err()
        );
    }

    #[test]
    fn check_revision_falls_back_to_the_authoritative_tree_for_display() {
        let revision = CheckRevision {
            id: Uuid::nil(),
            tree_sha256: "1234567890abcdef".repeat(4),
            reported_git_commit: None,
        };
        assert_eq!(revision.short_identity(), "1234567");
    }

    #[test]
    fn short_oid_handles_sha1_and_tiny_test_ids() {
        let commit = CommitRef {
            oid: "a81d7c2ff00".into(),
            branch: None,
            temporary: false,
            parent_oid: None,
        };
        assert_eq!(commit.short_oid(), "a81d7c2");
    }

    #[test]
    fn workspace_tree_identity_uses_bytewise_utf8_path_order() {
        let files = [
            SourceFile {
                path: "a/b".into(),
                sha256: "22".repeat(32),
                bytes: 2,
                executable: true,
            },
            SourceFile {
                path: "a-b".into(),
                sha256: "11".repeat(32),
                bytes: 1,
                executable: false,
            },
        ];
        assert_eq!(
            source_tree_sha256(&files).expect("bytewise UTF-8 order"),
            "1164facf7b1210ed7a8749455048ef9a0d0e39179fa0c85c57720c6395a5c83e"
        );
    }

    #[test]
    fn fix_proposals_validate_the_declared_patch_scope() {
        let proposal = FixProposal {
            id: Uuid::new_v4(),
            state: FixProposalState::Proposed,
            base_revision_id: Uuid::new_v4(),
            base_tree_sha256: "b".repeat(64),
            summary: "Guard the empty value".into(),
            diff: "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-old\n+new\n"
                .into(),
            instructions: "Apply and run the focused test.".into(),
            files: vec![FixFile {
                path: "src/lib.rs".into(),
                change: FixFileChange::Modified,
            }],
            validation: vec!["cargo test".into()],
            created_at: Utc::now(),
        };
        assert!(proposal.validate().is_ok());

        let mut mismatched = proposal.clone();
        mismatched.files[0].path = "src/other.rs".into();
        assert!(mismatched.validate().is_err());

        let mut traversal = proposal;
        traversal.diff = traversal.diff.replace("src/lib.rs", "../outside.rs");
        assert!(traversal.validate().is_err());
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CheckOperationalError {
    Blocked,
    Canceled,
    Error,
    MissingConclusion,
}
