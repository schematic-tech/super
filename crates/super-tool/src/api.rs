use std::{fmt, sync::Arc, time::Duration};

use anyhow::{Context, Result, bail};
use futures_util::{StreamExt, TryStreamExt, stream};
use reqwest::{Client, Method, RequestBuilder, Response, StatusCode, header};
use serde::{Serialize, de::DeserializeOwned};
use super_types::{
    ApiError, ApiErrorEnvelope, Check, CheckEvent, CheckSubmissionResponse, CommitRef, CreateChecksRequest,
    CreateWorkspace, CreateWorkspaceRevision, FixProposal, Identity, MissingContent, RevisionFile, UploadPlan,
    UploadRequest, VerifyUploadRequest, Workspace, WorkspaceRevision, WorkspaceRevisionAdmission,
    WorkspaceRevisionState, api, source_sha256,
};
use uuid::Uuid;

use crate::{
    config::LocalRevision,
    git::{GitRepository, SourceSnapshot},
};

const SOURCE_UPLOAD_CONCURRENCY: usize = 8;
const MAX_SSE_FRAME_BYTES: usize = 1024 * 1024;
const API_REQUEST_TIMEOUT: Duration = Duration::from_hours(1);
const SSE_IDLE_TIMEOUT: Duration = Duration::from_hours(1);
const SOURCE_UPLOAD_TIMEOUT: Duration = Duration::from_hours(1);

#[derive(Debug)]
struct ServiceError {
    status: StatusCode,
    body: Option<ApiError>,
}

impl fmt::Display for ServiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(body) = &self.body {
            formatter.write_str(&body.message)?;
            if let Some(help) = &body.help {
                write!(formatter, "\n  {help}")?;
            }
            return Ok(());
        }
        write!(formatter, "the Super API returned {}", self.status)
    }
}

impl std::error::Error for ServiceError {}

pub fn is_not_found(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<ServiceError>()
        .is_some_and(|error| error.status == StatusCode::NOT_FOUND)
}

pub fn is_exhausted(error: &anyhow::Error) -> bool {
    error.downcast_ref::<ServiceError>().is_some_and(|error| {
        error.status == StatusCode::TOO_MANY_REQUESTS
            && error
                .body
                .as_ref()
                .is_some_and(|body| body.code == api::EXHAUSTED_ERROR_CODE)
    })
}

fn check_cancellation_pending(error: &anyhow::Error) -> bool {
    error.downcast_ref::<ServiceError>().is_some_and(|error| {
        error.status == StatusCode::CONFLICT
            && error
                .body
                .as_ref()
                .is_some_and(|body| body.code == "check_cancellation_pending")
    })
}

pub fn is_retryable(error: &anyhow::Error) -> bool {
    if is_exhausted(error) {
        return false;
    }
    if error.downcast_ref::<ServiceError>().is_some_and(|error| {
        error.status == StatusCode::REQUEST_TIMEOUT
            || error.status == StatusCode::TOO_MANY_REQUESTS
            || error.status.is_server_error()
    }) {
        return true;
    }
    error
        .chain()
        .any(|cause| cause.is::<reqwest::Error>() || cause.is::<tokio::time::error::Elapsed>())
}

#[derive(Clone)]
pub struct PupClient {
    base: String,
    credential: Credential,
    http: Client,
    progress: Option<tokio::sync::watch::Sender<SourceProgress>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceProgress {
    Message(String),
    Preparing,
    Comparing,
    Uploading { completed: u64, total: u64 },
    Finalizing,
}

#[derive(Clone)]
enum Credential {
    Anonymous,
    Bearer(Arc<str>),
}

pub struct CheckEventStream {
    response: Response,
    buffer: Vec<u8>,
}

pub struct RevisionCache<'a> {
    pub parent_id: Option<Uuid>,
    pub source_hashes: &'a std::collections::HashMap<String, String>,
    pub revision: Option<&'a LocalRevision>,
}

impl CheckEventStream {
    pub async fn next_event(&mut self) -> Result<Option<CheckEvent>> {
        loop {
            while let Some((frame_end, delimiter_bytes)) = sse_frame_boundary(&self.buffer) {
                let frame = self.buffer[..frame_end].to_vec();
                self.buffer.drain(..frame_end + delimiter_bytes);
                if let Some(event) = parse_check_event_frame(&frame)? {
                    return Ok(Some(event));
                }
            }
            let chunk = tokio::time::timeout(SSE_IDLE_TIMEOUT, self.response.chunk()).await;
            let Some(bytes) = (match chunk {
                Ok(Ok(chunk)) => chunk,
                Ok(Err(_)) | Err(_) => return Ok(None),
            }) else {
                return Ok(None);
            };
            self.buffer.extend_from_slice(&bytes);
            if sse_frame_boundary(&self.buffer).is_none() && self.buffer.len() > MAX_SSE_FRAME_BYTES {
                bail!("the Super event stream emitted a frame larger than 1 MiB")
            }
        }
    }
}

impl PupClient {
    pub fn new(base: &str, token: Option<&str>) -> Result<Self> {
        let routed = token
            .map(super_types::credential::RoutedKey::parse)
            .transpose()
            .map_err(anyhow::Error::msg)?
            .flatten();
        let base = routed.as_ref().map_or(base, |key| key.api_base.as_str());
        let credential = token.map_or(Credential::Anonymous, |token| Credential::Bearer(Arc::from(token)));
        Self::with_credential(base, credential)
    }

    fn with_credential(base: &str, credential: Credential) -> Result<Self> {
        let base = super_types::credential::ApiBase::parse(base).map_err(anyhow::Error::msg)?;
        let base = base.as_str();
        Ok(Self {
            base: base.into(),
            credential,
            http: Client::builder()
                .user_agent(concat!("super/", env!("CARGO_PKG_VERSION")))
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(10))
                .build()?,
            progress: None,
        })
    }

    #[cfg(test)]
    fn validate_api_origin(value: &str) -> Result<()> {
        super_types::credential::ApiBase::parse(value)
            .map(|_| ())
            .map_err(anyhow::Error::msg)
    }

    pub async fn whoami(&self) -> Result<Identity> {
        self.get(api::AUTH_WHOAMI).await
    }

    pub async fn repositories(&self) -> Result<Vec<super_types::usage::Repository>> {
        self.get(api::WORKSPACES).await
    }

    pub async fn usage(&self, query: &super_types::usage::UsageQuery) -> Result<super_types::usage::UsageReport> {
        let request = self
            .http
            .get(self.url(api::USAGE))
            .query(query)
            .timeout(Duration::from_secs(25));
        let response = self
            .authorize(request)
            .send()
            .await
            .context("could not load token usage; retry `super usage`")?;
        if response.status() == StatusCode::UNAUTHORIZED {
            bail!("your Super credential could not be verified\n  Run `super login`, then retry `super usage`.")
        }
        let response = if response.status().is_success() {
            response
        } else {
            self.service_error(response).await?
        };
        response
            .json()
            .await
            .context("the Super API returned an invalid usage report")
    }

    pub fn with_source_progress(&self, progress: tokio::sync::watch::Sender<SourceProgress>) -> Self {
        let mut client = self.clone();
        client.progress = Some(progress);
        client
    }

    fn report_source(&self, progress: SourceProgress) {
        if let Some(sender) = &self.progress {
            sender.send_replace(progress);
        }
    }

    pub async fn link_workspace(&self, name: &str, association_id: &str, previous: Option<Uuid>) -> Result<Workspace> {
        if let Some(id) = previous {
            return match self.workspace(id).await {
                Err(error) if is_not_found(&error) => Err(error).context(
                    "the previously linked workspace is unavailable\n  Check your account and API endpoint. If they are correct, contact Schematic support to recover the existing link.",
                ),
                result => result,
            };
        }
        self.json_with_idempotency(
            Method::POST,
            api::WORKSPACES,
            &CreateWorkspace { name: name.to_owned() },
            &format!("pup-link:{association_id}"),
        )
        .await
    }

    pub async fn workspace_revision(&self, id: Uuid) -> Result<WorkspaceRevision> {
        self.get(&api::workspace_revision(id)).await
    }

    async fn workspace(&self, id: Uuid) -> Result<Workspace> {
        let workspace: Workspace = self.get(&api::workspace(id)).await?;
        if workspace.id != id {
            bail!("the Super API returned a different workspace than requested");
        }
        Ok(workspace)
    }

    pub async fn ensure_revision(
        &self,
        workspace_id: Uuid,
        repository: &GitRepository,
        commit: &CommitRef,
        cache: RevisionCache<'_>,
    ) -> Result<(WorkspaceRevision, Option<SourceSnapshot>)> {
        if let Some(cached) = cache.revision {
            self.report_source(SourceProgress::Comparing);
            match self.workspace_revision(cached.id).await {
                Ok(revision)
                    if revision.workspace_id == workspace_id
                        && revision.tree_sha256 == cached.tree_sha256
                        && revision.state == WorkspaceRevisionState::Complete =>
                {
                    return Ok((revision, None));
                }
                Ok(revision) if revision.workspace_id != workspace_id => {
                    bail!(
                        "the saved source revision for commit {} belongs to a different workspace\n  Contact Schematic support to recover the existing link.",
                        commit.short_oid()
                    )
                }
                Ok(revision) if revision.tree_sha256 != cached.tree_sha256 => {
                    bail!(
                        "saved source metadata for commit {} does not match the recorded source tree\n  Contact Schematic support to reconcile the saved source metadata.",
                        commit.short_oid()
                    )
                }
                Ok(_) => {}
                Err(error) if is_not_found(&error) => {}
                Err(error) => return Err(error),
            }
        }
        let (revision, snapshot) = self
            .upload_revision(workspace_id, repository, commit, cache.parent_id, cache.source_hashes)
            .await?;
        Ok((revision, Some(snapshot)))
    }

    pub async fn upload_revision(
        &self,
        workspace_id: Uuid,
        repository: &GitRepository,
        commit: &CommitRef,
        parent_id: Option<Uuid>,
        known_hashes: &std::collections::HashMap<String, String>,
    ) -> Result<(WorkspaceRevision, SourceSnapshot)> {
        self.report_source(SourceProgress::Preparing);
        let repository_for_snapshot = repository.clone();
        let oid = commit.oid.clone();
        let known_hashes = known_hashes.clone();
        let snapshot =
            tokio::task::spawn_blocking(move || repository_for_snapshot.source_snapshot(&oid, &known_hashes))
                .await
                .context("source preparation task failed")??;
        self.report_source(SourceProgress::Comparing);
        let admission: WorkspaceRevisionAdmission = self
            .json(
                Method::POST,
                &api::workspace_revisions(workspace_id),
                &CreateWorkspaceRevision {
                    parent_id,
                    reported_git_commit: Some(commit.oid.clone()),
                    tree_sha256: snapshot.manifest.tree_sha256.clone(),
                    files: snapshot
                        .manifest
                        .files
                        .iter()
                        .map(|file| RevisionFile {
                            path: file.path.clone(),
                            sha256: file.sha256.clone(),
                            bytes: file.bytes,
                            executable: file.executable,
                        })
                        .collect(),
                },
            )
            .await?;
        validate_revision_identity(
            &admission.revision,
            workspace_id,
            &snapshot.manifest.tree_sha256,
            commit,
        )?;

        let uploads = admission
            .missing_content
            .iter()
            .map(|missing| {
                let path = snapshot
                    .content_path(&missing.sha256)
                    .with_context(|| format!("Super API requested unadvertised content {}", missing.sha256))?;
                Ok((missing.clone(), path.to_owned()))
            })
            .collect::<Result<Vec<_>>>()?;
        let total = uploads.iter().try_fold(0_u64, |total, (missing, _)| {
            total
                .checked_add(missing.bytes)
                .context("source sync size exceeds the supported range")
        })?;
        if total > 0 {
            self.report_source(SourceProgress::Uploading { completed: 0, total });
        }
        stream::iter(uploads)
            .map(|(missing, path)| {
                let client = self.clone();
                let repository = repository.clone();
                let oid = commit.oid.clone();
                async move {
                    let bytes = missing.bytes;
                    client
                        .upload_missing_content(admission.revision.id, repository, oid, path, missing)
                        .await?;
                    Ok::<_, anyhow::Error>(bytes)
                }
            })
            .buffer_unordered(SOURCE_UPLOAD_CONCURRENCY)
            .try_fold(0_u64, |completed, bytes| async move {
                let completed = completed + bytes;
                self.report_source(SourceProgress::Uploading { completed, total });
                Ok(completed)
            })
            .await?;

        self.report_source(SourceProgress::Finalizing);
        let revision: WorkspaceRevision = self
            .post_without_body(&api::finalize_revision(admission.revision.id))
            .await?;
        validate_revision_identity(&revision, workspace_id, &snapshot.manifest.tree_sha256, commit)?;
        Ok((revision, snapshot))
    }

    async fn upload_missing_content(
        &self,
        revision_id: Uuid,
        repository: GitRepository,
        oid: String,
        path: String,
        missing: MissingContent,
    ) -> Result<()> {
        let path_for_read = path.clone();
        let content = tokio::task::spawn_blocking(move || repository.file_bytes_at_commit(&oid, &path_for_read))
            .await
            .context("source content task failed")??;
        if u64::try_from(content.len()).unwrap_or(u64::MAX) != missing.bytes
            || source_sha256(&content) != missing.sha256
        {
            bail!("Git content for `{path}` changed after its source manifest was computed")
        }
        let plan: UploadPlan = self
            .json(
                Method::POST,
                &api::content_upload(revision_id, &missing.sha256),
                &UploadRequest { bytes: missing.bytes },
            )
            .await?;
        if plan.sha256 != missing.sha256 || plan.bytes != missing.bytes || plan.staged_upload_id.is_empty() {
            bail!("the Super API returned a sync reservation for different source content");
        }
        let upload = self.direct_resumable_upload(&plan, content).await;
        let verification = self.verify_uploaded_content(revision_id, &plan).await;
        match (upload, verification) {
            (_, Ok(_)) => Ok(()),
            (Ok(()), Err(error)) => Err(error),
            (Err(upload), Err(verification)) => Err(anyhow::Error::new(upload).context(format!(
                "Super API could not verify the synced source content: {verification:#}"
            ))),
        }
    }

    async fn verify_uploaded_content(&self, revision_id: Uuid, plan: &UploadPlan) -> Result<MissingContent> {
        let verified: MissingContent = self
            .json(
                Method::POST,
                &api::verify_content(revision_id, &plan.sha256),
                &VerifyUploadRequest {
                    staged_upload_id: plan.staged_upload_id.clone(),
                },
            )
            .await?;
        if verified.sha256 != plan.sha256 || verified.bytes != plan.bytes || verified.stored_bytes != plan.bytes {
            bail!("the Super API verified different source content than the sync reservation");
        }
        Ok(verified)
    }

    async fn direct_resumable_upload(&self, plan: &UploadPlan, content: Vec<u8>) -> Result<(), DirectUploadError> {
        let method = Method::from_bytes(plan.method.as_bytes())
            .map_err(|error| DirectUploadError::Protocol(error.to_string()))?;
        let mut initiation = self.http.request(method, &plan.initiation_url);
        for (name, value) in &plan.headers {
            initiation = initiation.header(name, value);
        }
        let response = tokio::time::timeout(SOURCE_UPLOAD_TIMEOUT, initiation.send())
            .await
            .map_err(|_| DirectUploadError::Protocol("starting source sync timed out".to_owned()))?
            .map_err(|error| DirectUploadError::transport(&error))?;
        if !response.status().is_success() {
            return Err(DirectUploadError::Status(response.status()));
        }
        let session = response
            .headers()
            .get(header::LOCATION)
            .ok_or_else(|| {
                DirectUploadError::Protocol("source storage omitted the resumable session location".to_owned())
            })?
            .to_str()
            .map_err(|error| DirectUploadError::Protocol(error.to_string()))?
            .to_owned();
        let content_length = content.len();
        let content_range = if content_length == 0 {
            "bytes */0".to_owned()
        } else {
            format!("bytes 0-{}/{}", content_length - 1, content_length)
        };
        let upload = self
            .http
            .put(session)
            .header(header::CONTENT_TYPE, "application/octet-stream")
            .header(header::CONTENT_LENGTH, content_length)
            .header(header::CONTENT_RANGE, content_range)
            .body(content)
            .send();
        let response = tokio::time::timeout(SOURCE_UPLOAD_TIMEOUT, upload)
            .await
            .map_err(|_| DirectUploadError::Protocol("syncing source content timed out".to_owned()))?
            .map_err(|error| DirectUploadError::transport(&error))?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(DirectUploadError::Status(response.status()))
        }
    }

    pub async fn create_checks(
        &self,
        workspace_id: Uuid,
        request: &CreateChecksRequest,
        idempotency_key: &str,
        mut on_cancellation_wait: impl FnMut(),
    ) -> Result<CheckSubmissionResponse> {
        // A stopping predecessor has not admitted this submission. Keep the same key throughout
        // the bounded wait, including recovery after interruption or a lost acceptance response.
        let response = tokio::time::timeout(API_REQUEST_TIMEOUT, async {
            let mut waiting = false;
            loop {
                match self
                    .json_with_idempotency(
                        Method::POST,
                        &api::check_submissions(workspace_id),
                        request,
                        idempotency_key,
                    )
                    .await
                {
                    Err(error) if check_cancellation_pending(&error) => {
                        if !waiting {
                            on_cancellation_wait();
                            waiting = true;
                        }
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                    result => break result,
                }
            }
        })
        .await
        .context("waiting for check acceptance timed out; run super check again to recover the same submission")??;
        validate_submission(&response, workspace_id)?;
        Ok(response)
    }

    pub async fn latest_submission(
        &self,
        workspace_id: Uuid,
        selector: Option<&str>,
    ) -> Result<CheckSubmissionResponse> {
        let mut request = self.http.get(self.url(&api::latest_check_submission(workspace_id)));
        if let Some(selector) = selector {
            request = request.query(&[("selector", selector)]);
        }
        let response = self.send(request).await?;
        validate_submission(&response, workspace_id)?;
        Ok(response)
    }

    pub async fn submission(&self, workspace_id: Uuid, id: Uuid) -> Result<CheckSubmissionResponse> {
        let response: CheckSubmissionResponse = self.get(&api::check_submission(workspace_id, id)).await?;
        validate_submission(&response, workspace_id)?;
        if response.submission.id != id {
            bail!("server returned a different run UUID");
        }
        Ok(response)
    }

    pub async fn history(
        &self,
        workspace_id: Uuid,
        selector: Option<&str>,
        before: Option<u64>,
    ) -> Result<super_types::CheckHistoryPage> {
        let mut request = self
            .http
            .get(self.url(&api::check_history(workspace_id)))
            .query(&[("limit", 500)]);
        if let Some(selector) = selector {
            request = request.query(&[("selector", selector)]);
        }
        if let Some(before) = before {
            request = request.query(&[("before", before)]);
        }
        let page: super_types::CheckHistoryPage = self.send(request).await?;
        validate_history_page(&page, workspace_id, before)?;
        Ok(page)
    }

    pub async fn check(&self, workspace_id: Uuid, check_id: u64) -> Result<Check> {
        let check: Check = self.get(&api::check(workspace_id, check_id)).await?;
        if check.repository_id != workspace_id || check.number != check_id || !valid_check_number(check.number) {
            bail!("server returned a different check identity");
        }
        Ok(check)
    }

    /// History is ordered by check number, newest first. Resolve each declaration once across
    /// all pages; a selector can span checks submitted separately or inside broader runs.
    pub async fn latest_checks(&self, workspace_id: Uuid, selector: &str) -> Result<Vec<Check>> {
        let mut latest = std::collections::BTreeMap::new();
        let mut before = None;
        loop {
            let page = self.history(workspace_id, Some(selector), before).await?;
            for check in page.checks {
                latest.entry(check.supertest.selector()).or_insert(check);
            }
            if latest.len() > super_types::MAX_SUPERTESTS_PER_SUBMISSION {
                bail!("more than 128 supertests match; select a narrower path");
            }
            before = page.next_before;
            if before.is_none() || (selector.contains("::") && !latest.is_empty()) {
                return Ok(latest.into_values().collect());
            }
        }
    }

    pub async fn fix(&self, workspace_id: Uuid, check_id: u64) -> Result<FixProposal> {
        let fix: FixProposal = self.get(&api::fix(workspace_id, check_id)).await?;
        fix.validate().map_err(anyhow::Error::msg)?;
        Ok(fix)
    }

    /// Waits on the server's ordered event stream until a fix proposal arrives or the
    /// fix generation finishes. A verdict can arrive earlier. Reconnects after an idle
    /// stream without polling.
    pub async fn wait_for_fix(&self, workspace_id: Uuid, check_id: u64, mut current: Check) -> Result<Check> {
        if current.fix.is_some() || (current.terminal && !current.fix_pending) {
            return Ok(current);
        }
        tokio::time::timeout(Duration::from_mins(30), async {
            let mut reconnect_delay = Duration::from_millis(250);
            loop {
                let mut events = self.check_events(workspace_id, &[check_id]).await?;
                while let Some(event) = events.next_event().await? {
                    if event.check.number == check_id && event.check.repository_id == workspace_id {
                        if event.sequence > current.event_sequence {
                            reconnect_delay = Duration::from_millis(250);
                        }
                        if event.sequence >= current.event_sequence {
                            current = event.check;
                        }
                        if current.fix.is_some() || (current.terminal && !current.fix_pending) {
                            return Ok(current);
                        }
                    }
                }
                tokio::time::sleep(reconnect_delay).await;
                reconnect_delay = (reconnect_delay * 2).min(Duration::from_secs(5));
            }
        })
        .await
        .context("timed out waiting for a fix; retry `super fix` to resume from the current server state")?
    }

    pub async fn check_events(&self, workspace_id: Uuid, check_numbers: &[u64]) -> Result<CheckEventStream> {
        if check_numbers.is_empty() {
            bail!("at least one check is required for an event stream")
        }
        let check_numbers = check_numbers.iter().map(u64::to_string).collect::<Vec<_>>().join(",");
        let request = self
            .http
            .get(self.url(&api::check_events(workspace_id)))
            .query(&[("references", check_numbers)]);
        Ok(CheckEventStream {
            response: self.checked_response(request).await?,
            buffer: Vec::new(),
        })
    }

    pub async fn cancel(&self, workspace_id: Uuid, check_id: u64) -> Result<Check> {
        let check: Check = self
            .post_without_body(&api::cancel_check(workspace_id, check_id))
            .await?;
        if check.repository_id != workspace_id || check.number != check_id || !valid_check_number(check.number) {
            bail!("server returned a different cancellation target");
        }
        Ok(check)
    }

    /// Observe the public stop condition, including work continuing after a result.
    /// The caller bounds this wait and retains the acknowledged cancellation request.
    pub async fn wait_for_stop(&self, current: &mut Check) -> Result<()> {
        let mut delay = Duration::from_millis(250);
        while crate::cancel::active(current) {
            let observed = async {
                let mut events = self.check_events(current.repository_id, &[current.number]).await?;
                while let Some(event) = events.next_event().await? {
                    if event.check.repository_id == current.repository_id
                        && event.check.number == current.number
                        && event.sequence >= current.event_sequence
                    {
                        if event.sequence > current.event_sequence {
                            delay = Duration::from_millis(250);
                        }
                        *current = event.check;
                        if !crate::cancel::active(current) {
                            break;
                        }
                    }
                }
                Ok::<_, anyhow::Error>(())
            }
            .await;
            if let Err(error) = observed
                && !is_retryable(&error)
            {
                return Err(error);
            }
            if crate::cancel::active(current) {
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(Duration::from_secs(1));
            }
        }
        Ok(())
    }

    async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        self.send(self.http.get(self.url(path))).await
    }

    async fn post_without_body<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        self.send(self.empty_post(path)).await
    }

    fn empty_post(&self, path: &str) -> RequestBuilder {
        self.http.post(self.url(path)).header(header::CONTENT_LENGTH, 0)
    }

    async fn json<B: Serialize + ?Sized, T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: &B,
    ) -> Result<T> {
        self.send(self.http.request(method, self.url(path)).json(body)).await
    }

    async fn json_with_idempotency<B: Serialize + ?Sized, T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: &B,
        idempotency_key: &str,
    ) -> Result<T> {
        self.send(
            self.http
                .request(method, self.url(path))
                .json(body)
                .header("idempotency-key", idempotency_key),
        )
        .await
    }

    async fn send<T: DeserializeOwned>(&self, request: RequestBuilder) -> Result<T> {
        self.checked_response(request)
            .await?
            .json()
            .await
            .map_err(|_| anyhow::anyhow!("the Super API returned invalid data"))
    }

    async fn checked_response(&self, request: RequestBuilder) -> Result<Response> {
        let response = tokio::time::timeout(API_REQUEST_TIMEOUT, self.authorize(request).send())
            .await
            .context("the Super API request timed out")?
            .with_context(|| format!("could not reach the Super API at {}", self.base))?;
        if response.status().is_success() {
            return Ok(response);
        }
        self.service_error(response).await
    }

    async fn service_error(&self, response: Response) -> Result<Response> {
        let status = response.status();
        let body = response
            .json::<ApiErrorEnvelope>()
            .await
            .ok()
            .map(|envelope| envelope.error);
        Err(ServiceError { status, body }.into())
    }

    fn authorize(&self, request: RequestBuilder) -> RequestBuilder {
        match &self.credential {
            Credential::Anonymous => request,
            Credential::Bearer(token) => request.bearer_auth(token),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }
}

fn validate_revision_identity(
    revision: &WorkspaceRevision,
    workspace_id: Uuid,
    tree_sha256: &str,
    commit: &CommitRef,
) -> Result<()> {
    if revision.workspace_id != workspace_id || revision.tree_sha256 != tree_sha256 {
        bail!("the Super API returned a revision for a different workspace source tree")
    }
    if revision.reported_git_commit.as_deref() != Some(commit.oid.as_str()) {
        bail!("the Super API changed the reported Git provenance for the admitted source tree")
    }
    Ok(())
}

#[derive(Debug)]
enum DirectUploadError {
    Status(StatusCode),
    Protocol(String),
    Transport(&'static str),
}

impl DirectUploadError {
    fn transport(error: &reqwest::Error) -> Self {
        if error.is_timeout() {
            Self::Transport("the source sync connection timed out")
        } else if error.is_connect() {
            Self::Transport("could not connect to source storage")
        } else {
            Self::Transport("the source sync connection failed")
        }
    }
}

impl fmt::Display for DirectUploadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Status(status) => {
                write!(formatter, "source storage rejected source sync with {status}")
            }
            Self::Protocol(message) => formatter.write_str(message),
            Self::Transport(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for DirectUploadError {}

fn sse_frame_boundary(bytes: &[u8]) -> Option<(usize, usize)> {
    let line_feed = bytes
        .windows(2)
        .position(|window| window == b"\n\n")
        .map(|index| (index, 2));
    let carriage_return = bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| (index, 4));
    match (line_feed, carriage_return) {
        (Some(left), Some(right)) => Some(if left.0 < right.0 { left } else { right }),
        (Some(boundary), None) | (None, Some(boundary)) => Some(boundary),
        (None, None) => None,
    }
}

fn parse_check_event_frame(frame: &[u8]) -> Result<Option<CheckEvent>> {
    let frame = std::str::from_utf8(frame).context("the Super event stream returned non-UTF-8 data")?;
    let mut event_kind = None;
    let mut data = Vec::new();
    for line in frame.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(value) = line.strip_prefix("event:") {
            event_kind = Some(value.trim());
        } else if let Some(value) = line.strip_prefix("data:") {
            data.push(value.strip_prefix(' ').unwrap_or(value));
        }
    }
    if data.is_empty() || event_kind.is_some_and(|kind| kind != "check") {
        return Ok(None);
    }
    let event = serde_json::from_str::<CheckEvent>(&data.join("\n"))
        .map_err(|_| anyhow::anyhow!("the Super event stream returned invalid check data"))?;
    Ok(Some(event))
}

fn valid_check_number(number: u64) -> bool {
    number > 0 && i64::try_from(number).is_ok()
}

fn validate_history_page(page: &super_types::CheckHistoryPage, workspace: Uuid, before: Option<u64>) -> Result<()> {
    if page.checks.len() > 500
        || before.is_some_and(|cursor| !valid_check_number(cursor))
        || page.checks.iter().any(|check| {
            check.repository_id != workspace
                || !valid_check_number(check.number)
                || before.is_some_and(|cursor| check.number >= cursor)
                || page.next_before.is_some_and(|cursor| check.number < cursor)
        })
        || page.checks.windows(2).any(|pair| pair[0].number <= pair[1].number)
        || page.next_before.is_some_and(|cursor| !valid_check_number(cursor))
    {
        bail!("server returned an invalid check history page");
    }
    if before
        .zip(page.next_before)
        .is_some_and(|(previous, next)| next >= previous)
    {
        bail!("history cursor did not advance");
    }
    Ok(())
}

fn validate_submission(response: &CheckSubmissionResponse, workspace: Uuid) -> Result<()> {
    let submission = &response.submission;
    let ids: std::collections::BTreeSet<_> = response.checks.iter().map(|check| check.number).collect();
    if submission.repository_id != workspace
        || ids.len() != response.checks.len()
        || ids.len() != submission.check_numbers.len()
        || ids != submission.check_numbers.iter().copied().collect()
        || response
            .checks
            .iter()
            .any(|check| check.repository_id != workspace || !valid_check_number(check.number))
    {
        bail!("server returned inconsistent run/check identities");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use super_types::{
        CheckPresentation, CheckRevision, HistoryPresentation, PresentationTone, SourceLanguage, StatusPresentation,
        Supertest,
    };

    use super::*;

    fn check() -> Check {
        let now = Utc::now();
        Check {
            repository_id: Uuid::nil(),
            number: 3,
            supertest: Supertest {
                path: "tests/example.py".to_owned(),
                name: "property_holds".to_owned(),
                language: SourceLanguage::Python,
                line: Some(4),
            },
            revision: CheckRevision {
                id: Uuid::nil(),
                tree_sha256: "2".repeat(64),
                reported_git_commit: Some("1".repeat(40)),
            },
            terminal: false,
            problematic: false,
            result: None,
            operational_error: None,
            presentation: CheckPresentation {
                status: StatusPresentation {
                    marker: "●".to_owned(),
                    label: "checking".to_owned(),
                    tone: PresentationTone::Active,
                },
                live_line: None,
                activity_label: None,
                activity_updated_at: None,
                details: Vec::new(),
                active_problem_label: None,
                history: HistoryPresentation::default(),
            },
            fix: None,
            fix_pending: false,
            updates_pending: false,
            event_sequence: 1,
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn shared_number_can_belong_to_multiple_runs_without_changing_check_identity() {
        let check = check();
        let mut response = CheckSubmissionResponse {
            submission: super_types::CheckSubmission {
                id: Uuid::new_v4(),
                repository_id: check.repository_id,
                selector: ".".into(),
                check_numbers: vec![check.number],
                created_at: check.created_at,
            },
            checks: vec![check.clone()],
        };
        validate_submission(&response, check.repository_id).unwrap();
        response.submission.id = Uuid::new_v4();
        validate_submission(&response, check.repository_id).unwrap();
        response.checks[0].repository_id = Uuid::new_v4();
        assert!(validate_submission(&response, check.repository_id).is_err());
        response.checks[0] = check;
        response.submission.check_numbers.push(3);
        assert!(validate_submission(&response, Uuid::nil()).is_err());
    }

    #[test]
    fn numeric_history_pages_preserve_workspace_order_and_exclusive_cursors() {
        let mut earlier = check();
        earlier.number = 2;
        let page = super_types::CheckHistoryPage {
            checks: vec![check(), earlier],
            next_before: Some(2),
        };
        validate_history_page(&page, Uuid::nil(), Some(4)).unwrap();
        assert!(validate_history_page(&page, Uuid::new_v4(), Some(4)).is_err());
        assert!(validate_history_page(&page, Uuid::nil(), Some(3)).is_err());
        assert!(validate_history_page(&page, Uuid::nil(), Some(0)).is_err());
        let mut invalid = page.clone();
        invalid.checks.reverse();
        assert!(validate_history_page(&invalid, Uuid::nil(), None).is_err());
        invalid.checks = vec![check(), check()];
        assert!(validate_history_page(&invalid, Uuid::nil(), None).is_err());
        for number in [0, i64::MAX as u64 + 1] {
            invalid.checks = vec![check()];
            invalid.checks[0].number = number;
            assert!(validate_history_page(&invalid, Uuid::nil(), None).is_err());
        }
        invalid = page.clone();
        invalid.next_before = Some(3);
        assert!(validate_history_page(&invalid, Uuid::nil(), None).is_err());
        invalid.checks = vec![check(); 501];
        assert!(validate_history_page(&invalid, Uuid::nil(), None).is_err());
    }

    #[test]
    fn empty_filtered_history_pages_can_continue_but_cannot_loop() {
        let mut page = super_types::CheckHistoryPage {
            checks: vec![],
            next_before: Some(2),
        };
        validate_history_page(&page, Uuid::nil(), Some(3)).unwrap();
        for next in [0, 3, 4, i64::MAX as u64 + 1] {
            page.next_before = Some(next);
            assert!(validate_history_page(&page, Uuid::nil(), Some(3)).is_err());
        }
        page.next_before = None;
        validate_history_page(&page, Uuid::nil(), Some(3)).unwrap();
    }

    #[test]
    fn sse_parser_ignores_keepalives_and_decodes_public_event_envelopes() {
        assert!(parse_check_event_frame(b": keepalive").unwrap().is_none());
        let event = CheckEvent {
            sequence: 1,
            check: check(),
        };
        let frame = format!(
            "id: 3:now\nevent: check\ndata: {}",
            serde_json::to_string(&event).unwrap()
        );
        let event = parse_check_event_frame(frame.as_bytes()).unwrap().unwrap();
        assert_eq!(event.check.number, 3);
        assert_eq!(event.check.presentation.status.label, "checking");
    }

    #[test]
    fn malformed_events_do_not_echo_unexpected_fields_or_values() {
        for unexpected_field in [false, true] {
            let mut event = serde_json::to_value(CheckEvent {
                sequence: 1,
                check: check(),
            })
            .unwrap();
            if unexpected_field {
                event["unpublished_field"] = serde_json::json!("unpublished_value");
            } else {
                event["check"]["operational_error"] = serde_json::json!("unpublished_value");
            }
            let frame = format!("event: check\ndata: {event}");
            let error = parse_check_event_frame(frame.as_bytes()).unwrap_err();
            assert_eq!(
                format!("{error:#}"),
                "the Super event stream returned invalid check data"
            );
        }
    }

    #[test]
    fn empty_posts_advertise_their_zero_length() {
        let client = PupClient::new("https://pup.example", None).unwrap();
        let request = client.empty_post("/verify").build().unwrap();
        assert_eq!(request.headers()[header::CONTENT_LENGTH], "0");
    }

    #[tokio::test]
    async fn source_verification_sends_the_exact_upload_reservation() {
        use std::io::{BufRead, BufReader, Read, Write};

        for (sha, bytes, stored, valid) in [
            ("a", 3, 3, true),
            ("b", 3, 3, false),
            ("a", 4, 3, false),
            ("a", 3, 2, false),
        ] {
            let returned = serde_json::json!({"sha256": sha.repeat(64), "bytes": bytes, "stored_bytes": stored});
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let client = PupClient::new(&format!("http://{}", listener.local_addr().unwrap()), None).unwrap();
            let revision_id = Uuid::new_v4();
            let plan: UploadPlan = serde_json::from_value(serde_json::json!({
                "staged_upload_id": "reservation-specific-to-this-upload",
                "sha256": "a".repeat(64), "bytes": 3,
                "initiation_url": "https://storage.example/upload", "method": "POST", "headers": {},
                "expires_at": "2026-09-08T00:00:00Z"
            }))
            .unwrap();
            let expected_path = api::verify_content(revision_id, &plan.sha256);
            let server = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                assert_eq!(line, format!("POST {expected_path} HTTP/1.1\r\n"));
                let mut length = None;
                let mut json = false;
                loop {
                    line.clear();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    let header = line.to_ascii_lowercase();
                    if let Some(value) = header.strip_prefix("content-length:") {
                        length = Some(value.trim().parse::<usize>().unwrap());
                    }
                    json |= header == "content-type: application/json\r\n";
                }
                assert!(json);
                let length = length.unwrap();
                assert!(length < 1024);
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                let request: VerifyUploadRequest = serde_json::from_slice(&body).unwrap();
                assert_eq!(request.staged_upload_id, "reservation-specific-to-this-upload");
                let body = returned.to_string();
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            });
            let result = client.verify_uploaded_content(revision_id, &plan).await;
            server.join().unwrap();
            if valid {
                assert_eq!(result.unwrap().sha256, plan.sha256);
            } else {
                assert!(result.unwrap_err().to_string().contains("different source content"));
            }
        }
    }

    fn check_stream_fixture(checks: Vec<Check>) -> (PupClient, std::thread::JoinHandle<()>) {
        use std::fmt::Write as _;
        use std::io::{BufRead, BufReader, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let client = PupClient::new(&format!("http://{}", listener.local_addr().unwrap()), None).unwrap();
        let mut body = String::new();
        for check in checks {
            let sequence = check.event_sequence;
            write!(
                body,
                "event: check\ndata: {}\n\n",
                serde_json::to_string(&CheckEvent { sequence, check }).unwrap()
            )
            .unwrap();
        }
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            loop {
                let mut line = String::new();
                assert!(reader.read_line(&mut line).unwrap() > 0);
                if line == "\r\n" {
                    break;
                }
            }
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        });
        (client, server)
    }

    #[tokio::test]
    async fn multiplexed_events_do_not_share_a_global_version_cursor() {
        let checks = [(1, 5), (2, 4), (2, 6)].map(|(number, sequence)| {
            let mut check = check();
            check.number = number;
            check.event_sequence = sequence;
            check
        });
        let (client, server) = check_stream_fixture(checks.into());
        let mut events = client.check_events(Uuid::nil(), &[1, 2]).await.unwrap();
        for expected in [(1, 5), (2, 4), (2, 6)] {
            let event = events.next_event().await.unwrap().unwrap();
            assert_eq!((event.check.number, event.sequence), expected);
        }
        assert!(events.next_event().await.unwrap().is_none());
        server.join().unwrap();
    }

    #[tokio::test]
    async fn waiting_for_a_fix_finishes_when_unavailable_even_with_explanation_pending() {
        let mut initial = check();
        initial.terminal = true;
        initial.fix_pending = true;
        initial.updates_pending = true;
        let mut finished = initial.clone();
        finished.fix_pending = false;
        finished.event_sequence += 1;
        let (client, server) = check_stream_fixture(vec![initial.clone(), finished.clone()]);
        let result = client
            .wait_for_fix(initial.repository_id, initial.number, initial)
            .await
            .unwrap();
        assert!(result.terminal && result.updates_pending && !result.fix_pending);
        assert!(result.fix.is_none());
        assert_eq!(result.event_sequence, finished.event_sequence);
        server.join().unwrap();
    }

    #[tokio::test]
    async fn waiting_for_a_fix_continues_after_verdict_and_finishes_before_explanation() {
        let mut initial = check();
        initial.terminal = true;
        initial.fix_pending = true;
        initial.updates_pending = true;
        let mut finished = initial.clone();
        finished.fix_pending = false;
        finished.event_sequence += 1;
        finished.fix = Some(serde_json::from_value(serde_json::json!({
            "id": Uuid::nil(), "state": "proposed", "base_revision_id": Uuid::nil(),
            "base_tree_sha256": "a".repeat(64), "summary": "Return zero.",
            "diff": "diff --git a/example.c b/example.c\n--- a/example.c\n+++ b/example.c\n@@ -1 +1 @@\n-return 1;\n+return 0;\n",
            "instructions": "Review the patch.", "files": [{"path": "example.c", "change": "modified"}],
            "validation": [], "created_at": "2026-08-31T00:00:00Z",
        })).unwrap());
        let (client, server) = check_stream_fixture(vec![initial.clone(), finished.clone()]);
        let result = client
            .wait_for_fix(initial.repository_id, initial.number, initial)
            .await
            .unwrap();
        assert_eq!(result.fix, finished.fix);
        assert!(result.terminal && result.updates_pending && !result.fix_pending);
        server.join().unwrap();
    }

    #[test]
    fn quota_exhaustion_is_terminal_but_transient_rate_limits_still_retry() {
        for (code, exhausted, retryable) in [("exhausted", true, false), ("rate_limited", false, true)] {
            let error: anyhow::Error = ServiceError {
                status: StatusCode::TOO_MANY_REQUESTS,
                body: Some(ApiError {
                    code: code.into(),
                    message: "request rejected".into(),
                    help: None,
                }),
            }
            .into();
            assert_eq!(is_exhausted(&error), exhausted);
            assert_eq!(is_retryable(&error), retryable);
        }
    }

    #[test]
    fn only_explicit_pending_cancellation_is_retried_during_admission() {
        for (status, code, retry) in [
            (StatusCode::CONFLICT, "check_cancellation_pending", true),
            (StatusCode::CONFLICT, "state_conflict", false),
            (StatusCode::SERVICE_UNAVAILABLE, "check_cancellation_pending", false),
        ] {
            let error: anyhow::Error = ServiceError {
                status,
                body: Some(ApiError {
                    code: code.into(),
                    message: "a matching check is stopping".into(),
                    help: None,
                }),
            }
            .into();
            assert_eq!(check_cancellation_pending(&error), retry);
        }
    }

    #[test]
    fn api_origins_require_tls_outside_loopback_and_cannot_carry_a_path() {
        assert!(PupClient::validate_api_origin("https://pup.example").is_ok());
        assert!(PupClient::validate_api_origin("http://127.0.0.1:8080").is_ok());
        assert!(PupClient::validate_api_origin("http://pup.example").is_err());
        assert!(PupClient::validate_api_origin("https://pup.example/api").is_err());
        assert!(PupClient::validate_api_origin("https://user:pass@pup.example").is_err());
    }
}
