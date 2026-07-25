use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use serde::de::DeserializeOwned;

use crate::error::{Result, TuicrError};
use crate::forge::remote_comments::RemoteReviewThread;
use crate::forge::submit::{GhSide, SubmitEvent};
use crate::forge::traits::{
    CreateReviewRequest, ForgeBackend, ForgeFileLinesRequest, ForgeRepository,
    GhCreateReviewResponse, PagedPullRequests, PullRequestCommit, PullRequestDetails,
    PullRequestListQuery, PullRequestListScope, PullRequestTarget,
};
use crate::model::DiffLine;
use crate::vcs::slice_context_lines;

use super::models::{BbPage, BbPrDetails, BbPrSummary, BbUser, group_comments_into_threads};
use super::models::{BbComment, BbCommit};

/// Bitbucket Cloud REST API 2.0 base. Cloud-only; Server/Data Center uses a
/// different API and is out of scope for this backend.
const API_BASE: &str = "https://api.bitbucket.org/2.0";
const BITBUCKET_HOST: &str = "bitbucket.org";

// ---------------------------------------------------------------------------
// Authentication
// ---------------------------------------------------------------------------

/// Resolved Bitbucket credentials. Bitbucket Cloud accepts either a Bearer
/// token (workspace/repo/project access token, or an Atlassian API token used
/// as Bearer) or HTTP Basic auth (`username:app_password` / `email:api_token`).
#[derive(Debug, Clone, PartialEq, Eq)]
enum BitbucketAuth {
    Bearer(String),
    Basic { username: String, secret: String },
}

impl BitbucketAuth {
    /// Resolve credentials: environment variables first, config file fallback.
    fn resolve() -> Option<Self> {
        Self::from_env().or_else(Self::from_config)
    }

    fn from_env() -> Option<Self> {
        let username = env_nonempty("BITBUCKET_USERNAME");
        let secret = env_nonempty("BITBUCKET_APP_PASSWORD")
            .or_else(|| env_nonempty("BITBUCKET_API_TOKEN"));
        if let (Some(username), Some(secret)) = (username.clone(), secret) {
            return Some(Self::Basic { username, secret });
        }
        if let Some(token) = env_nonempty("BITBUCKET_TOKEN") {
            // A username alongside a token still implies Basic (email:token).
            return Some(match username {
                Some(username) => Self::Basic {
                    username,
                    secret: token,
                },
                None => Self::Bearer(token),
            });
        }
        None
    }

    fn from_config() -> Option<Self> {
        let (username, token) = crate::config::bitbucket_config_credentials();
        let token = token?;
        Some(match username {
            Some(username) if !username.is_empty() => Self::Basic {
                username,
                secret: token,
            },
            _ => Self::Bearer(token),
        })
    }

    fn header_value(&self) -> String {
        match self {
            Self::Bearer(token) => format!("Bearer {token}"),
            Self::Basic { username, secret } => {
                let encoded = BASE64.encode(format!("{username}:{secret}"));
                format!("Basic {encoded}")
            }
        }
    }
}

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn missing_auth_error() -> TuicrError {
    TuicrError::Forge(
        "Bitbucket integration requires credentials.\n\
         Set BITBUCKET_TOKEN (an access/API token), or BITBUCKET_USERNAME plus \
         BITBUCKET_APP_PASSWORD, or add a [forge.bitbucket] section to your tuicr config."
            .to_string(),
    )
}

// ---------------------------------------------------------------------------
// HTTP client abstraction
// ---------------------------------------------------------------------------

/// Transport seam for Bitbucket API calls, mirroring the CLI-runner traits of
/// the GitHub/GitLab backends so tests can inject a fake with no live HTTP.
pub trait BitbucketHttpClient {
    fn get(&self, url: &str) -> Result<String>;
    fn post(&self, url: &str, body: &str) -> Result<String>;
}

/// Real client backed by `ureq`. Attaches the resolved auth header to every
/// request and surfaces Bitbucket's JSON `error.message` on failures.
pub struct SystemBitbucketClient {
    agent: ureq::Agent,
    auth: Option<BitbucketAuth>,
}

impl SystemBitbucketClient {
    pub fn new() -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(30)))
            // Read the body on non-2xx ourselves so we can surface the API's
            // error message instead of a bare status code.
            .http_status_as_error(false)
            // Bitbucket's PR diff endpoint (`/pullrequests/{id}/diff`) answers
            // with a 302 redirect to a topic-diff URL on the same host. ureq's
            // default (`Never`) drops the Authorization header when following
            // any redirect, so the diff would arrive unauthenticated and
            // Bitbucket returns 404 for private repos — surfacing as a "404 on
            // open PR" even though credentials are valid. Preserve the header
            // on same-host redirects so the follow-up request stays authed.
            .redirect_auth_headers(ureq::config::RedirectAuthHeaders::SameHost)
            .build();
        Self {
            agent: config.into(),
            auth: BitbucketAuth::resolve(),
        }
    }

    fn auth_header(&self) -> Result<String> {
        self.auth
            .as_ref()
            .map(BitbucketAuth::header_value)
            .ok_or_else(missing_auth_error)
    }
}

impl Default for SystemBitbucketClient {
    fn default() -> Self {
        Self::new()
    }
}

fn format_api_error(status: u16, body: &str) -> String {
    let detail = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            v.get("error")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .map(str::to_string)
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            if body.is_empty() {
                format!("HTTP {status}")
            } else {
                body.chars().take(300).collect()
            }
        });
    match status {
        401 | 403 => format!(
            "Bitbucket authentication failed (HTTP {status}): {detail}\n\
             Check BITBUCKET_TOKEN (or BITBUCKET_USERNAME + BITBUCKET_APP_PASSWORD) and its scopes."
        ),
        _ => format!("Bitbucket API error (HTTP {status}): {detail}"),
    }
}

impl BitbucketHttpClient for SystemBitbucketClient {
    fn get(&self, url: &str) -> Result<String> {
        let auth = self.auth_header()?;
        let response = self
            .agent
            .get(url)
            .header("Authorization", auth.as_str())
            .call()
            .map_err(|e| TuicrError::Forge(format!("Bitbucket request failed: {e}")))?;
        let status = response.status();
        let body = response
            .into_body()
            .read_to_string()
            .map_err(|e| TuicrError::Forge(format!("Bitbucket: failed to read response: {e}")))?;
        if status.is_success() {
            Ok(body)
        } else {
            Err(TuicrError::Forge(format_api_error(status.as_u16(), &body)))
        }
    }

    fn post(&self, url: &str, body: &str) -> Result<String> {
        let auth = self.auth_header()?;
        let response = self
            .agent
            .post(url)
            .header("Authorization", auth.as_str())
            .header("Content-Type", "application/json")
            .send(body)
            .map_err(|e| TuicrError::Forge(format!("Bitbucket request failed: {e}")))?;
        let status = response.status();
        let body = response
            .into_body()
            .read_to_string()
            .map_err(|e| TuicrError::Forge(format!("Bitbucket: failed to read response: {e}")))?;
        if status.is_success() {
            Ok(body)
        } else {
            Err(TuicrError::Forge(format_api_error(status.as_u16(), &body)))
        }
    }
}

// ---------------------------------------------------------------------------
// Local checkout helpers (shared shape with the GitLab backend)
// ---------------------------------------------------------------------------

/// Read a git blob from a checkout at `repo_root` using `git show <sha>:<path>`.
fn read_blob_with_repo(repo_root: &Path, sha: &str, path: &Path) -> Option<String> {
    use crate::process::run_command_output;
    use std::ffi::OsStr;
    let spec = format!("{}:{}", sha, path.to_string_lossy());
    run_command_output(
        "git",
        Some(repo_root),
        ["cat-file", "-e", spec.as_str()]
            .iter()
            .map(|s| OsStr::new(*s)),
    )
    .ok()?;
    run_command_output(
        "git",
        Some(repo_root),
        ["show", spec.as_str()].iter().map(|s| OsStr::new(*s)),
    )
    .ok()
}

/// Return `Some(diff)` when both SHAs exist locally, via `git diff <start>..<end>`.
fn local_range_diff(repo_root: &Path, start_sha: &str, end_sha: &str) -> Option<String> {
    use crate::process::run_command_output;
    use std::ffi::OsStr;
    for sha in [start_sha, end_sha] {
        run_command_output(
            "git",
            Some(repo_root),
            ["cat-file", "-e", sha].iter().map(|s| OsStr::new(*s)),
        )
        .ok()?;
    }
    let range = format!("{start_sha}..{end_sha}");
    run_command_output(
        "git",
        Some(repo_root),
        ["diff", range.as_str()].iter().map(|s| OsStr::new(*s)),
    )
    .ok()
}

/// Percent-encode a query-parameter value (used for the `q` PR filter).
fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len() * 3);
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Percent-encode a repository file path for the `src` endpoint, preserving `/`.
fn encode_path(path: &str) -> String {
    path.split('/')
        .map(urlencode)
        .collect::<Vec<_>>()
        .join("/")
}

// ---------------------------------------------------------------------------
// Backend
// ---------------------------------------------------------------------------

pub struct BitbucketBackend<C = SystemBitbucketClient> {
    default_repository: Option<ForgeRepository>,
    client: C,
    local_checkout: Option<PathBuf>,
}

impl BitbucketBackend<SystemBitbucketClient> {
    pub fn new(default_repository: Option<ForgeRepository>) -> Self {
        Self {
            default_repository,
            client: SystemBitbucketClient::new(),
            local_checkout: None,
        }
    }

    pub fn with_local_checkout(mut self, checkout: Option<PathBuf>) -> Self {
        self.local_checkout = checkout;
        self
    }
}

impl<C> BitbucketBackend<C>
where
    C: BitbucketHttpClient,
{
    #[cfg(test)]
    pub fn with_client(default_repository: Option<ForgeRepository>, client: C) -> Self {
        Self {
            default_repository,
            client,
            local_checkout: None,
        }
    }

    fn resolve_repository(&self, target: &PullRequestTarget) -> Result<ForgeRepository> {
        target
            .repository
            .clone()
            .or_else(|| self.default_repository.clone())
            .ok_or_else(|| {
                TuicrError::Forge(format!(
                    "Bitbucket pull request target `{}` does not include a repository",
                    target.original
                ))
            })
    }

    fn repo_api_base(repo: &ForgeRepository) -> String {
        format!("{API_BASE}/repositories/{}/{}", repo.owner, repo.name)
    }

    /// Follow `next` cursors, collecting deserialized `values` up to `max` items.
    fn fetch_paginated<T: DeserializeOwned>(&self, first_url: &str, max: usize) -> Result<Vec<T>> {
        let mut out: Vec<T> = Vec::new();
        let mut next = Some(first_url.to_string());
        while let Some(url) = next {
            if out.len() >= max {
                break;
            }
            let body = self.client.get(&url)?;
            let page: BbPage<T> = serde_json::from_str(&body)?;
            out.extend(page.values);
            next = page.next;
        }
        Ok(out)
    }

    /// Best-effort lookup of the authenticated account's UUID, used to filter
    /// PRs by reviewer. Returns `None` when the call fails.
    fn current_user_uuid(&self, _repo: &ForgeRepository) -> Option<String> {
        let body = self.client.get(&format!("{API_BASE}/user")).ok()?;
        let user: BbUser = serde_json::from_str(&body).ok()?;
        user.uuid.filter(|s| !s.is_empty())
    }

    fn fetch_file(&self, request: &ForgeFileLinesRequest) -> Result<String> {
        if let Some(root) = self.local_checkout.as_deref()
            && let Some(content) = read_blob_with_repo(root, request.sha(), request.path.as_path())
        {
            return Ok(content);
        }
        let path = request.path.to_string_lossy().replace('\\', "/");
        let url = format!(
            "{}/src/{}/{}",
            Self::repo_api_base(&request.repository),
            request.sha(),
            encode_path(&path),
        );
        self.client.get(&url)
    }

    fn post_comment(
        &self,
        repo_base: &str,
        pr_number: u64,
        payload: &serde_json::Value,
    ) -> Result<String> {
        let url = format!("{repo_base}/pullrequests/{pr_number}/comments");
        let body = serde_json::to_string(payload)?;
        self.client.post(&url, &body)
    }
}

impl<C> ForgeBackend for BitbucketBackend<C>
where
    C: BitbucketHttpClient,
{
    fn list_pull_requests(&self, query: PullRequestListQuery) -> Result<PagedPullRequests> {
        let page_size = query.page_size.max(1);
        let want = query.already_loaded + page_size + 1;
        let repo_base = Self::repo_api_base(&query.repository);

        let first_url = match query.scope {
            PullRequestListScope::ReviewRequested => match self.current_user_uuid(&query.repository)
            {
                Some(uuid) => {
                    let q = format!("state=\"OPEN\" AND reviewers.uuid=\"{uuid}\"");
                    format!("{repo_base}/pullrequests?pagelen=50&q={}", urlencode(&q))
                }
                // No UUID (auth without account scope): fall back to all open.
                None => format!("{repo_base}/pullrequests?state=OPEN&pagelen=50"),
            },
            PullRequestListScope::Open => {
                format!("{repo_base}/pullrequests?state=OPEN&pagelen=50")
            }
        };

        let rows: Vec<BbPrSummary> = self.fetch_paginated(&first_url, want)?;
        let has_more = rows.len() > query.already_loaded + page_size;
        let pull_requests = rows
            .into_iter()
            .skip(query.already_loaded)
            .take(page_size)
            .map(|row| row.into_summary(&query.repository))
            .collect::<Vec<_>>();
        let total_loaded = query.already_loaded + pull_requests.len();
        Ok(PagedPullRequests {
            pull_requests,
            has_more,
            total_loaded,
        })
    }

    fn get_pull_request(&self, target: PullRequestTarget) -> Result<PullRequestDetails> {
        let repository = self.resolve_repository(&target)?;
        let url = format!(
            "{}/pullrequests/{}",
            Self::repo_api_base(&repository),
            target.number
        );
        let body = self.client.get(&url)?;
        let details: BbPrDetails = serde_json::from_str(&body)?;
        Ok(details.into_details(&repository))
    }

    fn get_pull_request_diff(&self, pr: &PullRequestDetails) -> Result<String> {
        // Bitbucket's diff endpoint already emits git-style `diff --git` headers,
        // so the diff is fed to the parser unmodified.
        let url = format!(
            "{}/pullrequests/{}/diff",
            Self::repo_api_base(&pr.repository),
            pr.number
        );
        self.client.get(&url)
    }

    fn local_checkout_path(&self) -> Option<PathBuf> {
        self.local_checkout.clone()
    }

    fn list_pull_request_commits(&self, pr: &PullRequestDetails) -> Result<Vec<PullRequestCommit>> {
        let url = format!(
            "{}/pullrequests/{}/commits?pagelen=100",
            Self::repo_api_base(&pr.repository),
            pr.number
        );
        let rows: Vec<BbCommit> = self.fetch_paginated(&url, 1000)?;
        // Bitbucket returns commits newest-first; the trait contract is oldest
        // -first (the App reverses to newest-first for display).
        let mut commits: Vec<PullRequestCommit> = rows
            .into_iter()
            .map(BbCommit::into_pull_request_commit)
            .collect();
        commits.reverse();
        Ok(commits)
    }

    fn get_pull_request_commit_range_diff(
        &self,
        _pr: &PullRequestDetails,
        start_sha: &str,
        end_sha: &str,
    ) -> Result<String> {
        if let Some(root) = self.local_checkout.as_deref()
            && let Some(diff) = local_range_diff(root, start_sha, end_sha)
        {
            return Ok(diff);
        }
        Err(TuicrError::UnsupportedOperation(
            "Commit range diff without local checkout not yet supported for Bitbucket".to_string(),
        ))
    }

    fn list_review_threads(&self, pr: &PullRequestDetails) -> Result<Vec<RemoteReviewThread>> {
        let url = format!(
            "{}/pullrequests/{}/comments?pagelen=100",
            Self::repo_api_base(&pr.repository),
            pr.number
        );
        let comments: Vec<BbComment> = self.fetch_paginated(&url, 5000)?;
        Ok(group_comments_into_threads(comments))
    }

    fn fetch_file_lines(&self, request: ForgeFileLinesRequest) -> Result<Vec<DiffLine>> {
        if request.start_line == 0 || request.start_line > request.end_line {
            return Ok(Vec::new());
        }
        let content = self.fetch_file(&request)?;
        Ok(slice_context_lines(
            &content,
            request.start_line,
            request.end_line,
        ))
    }

    fn file_line_count(&self, request: ForgeFileLinesRequest) -> Result<u32> {
        let content = self.fetch_file(&request)?;
        Ok(content.lines().count() as u32)
    }

    fn create_review(
        &self,
        pr: &PullRequestDetails,
        request: CreateReviewRequest<'_>,
    ) -> Result<GhCreateReviewResponse> {
        // Bitbucket has no pending/draft review primitive; drafts are unsupported.
        if request.event == SubmitEvent::Draft {
            return Err(TuicrError::UnsupportedOperation(
                "Draft (pending) reviews are not supported for Bitbucket".to_string(),
            ));
        }

        let repo_base = Self::repo_api_base(&pr.repository);
        let mut first_comment_id: Option<u64> = None;

        // Review-level body posts as a general (non-inline) PR comment.
        if !request.body.is_empty() {
            let payload = serde_json::json!({ "content": { "raw": request.body } });
            let output = self.post_comment(&repo_base, pr.number, &payload)?;
            record_comment_id(&mut first_comment_id, &output);
        }

        // Inline comments: `to` anchors the new (right) side, `from` the old
        // (left) side. Bitbucket inline comments are single-line; a multi-line
        // selection anchors to its end line.
        for comment in request.comments {
            let path = comment.path.to_string_lossy().replace('\\', "/");
            let mut inline = serde_json::json!({ "path": path });
            match comment.side {
                GhSide::Right => inline["to"] = serde_json::Value::Number(comment.line.into()),
                GhSide::Left => inline["from"] = serde_json::Value::Number(comment.line.into()),
            }
            let payload = serde_json::json!({
                "content": { "raw": comment.body },
                "inline": inline,
            });
            let output = self.post_comment(&repo_base, pr.number, &payload)?;
            record_comment_id(&mut first_comment_id, &output);
        }

        if request.event == SubmitEvent::Approve {
            let url = format!("{repo_base}/pullrequests/{}/approve", pr.number);
            self.client.post(&url, "")?;
        }

        if request.event == SubmitEvent::RequestChanges {
            let url = format!("{repo_base}/pullrequests/{}/request-changes", pr.number);
            self.client.post(&url, "")?;
        }

        let state = match request.event {
            SubmitEvent::Approve => "APPROVED",
            SubmitEvent::RequestChanges => "CHANGES_REQUESTED",
            _ => "COMMENTED",
        };
        Ok(GhCreateReviewResponse {
            id: first_comment_id.unwrap_or(0),
            html_url: pr.url.clone(),
            state: state.to_string(),
        })
    }
}

/// Capture the first posted comment's numeric id from a Bitbucket response.
fn record_comment_id(slot: &mut Option<u64>, response: &str) {
    if slot.is_none()
        && let Ok(value) = serde_json::from_str::<serde_json::Value>(response)
        && let Some(id) = value.get("id").and_then(|v| v.as_u64())
    {
        *slot = Some(id);
    }
}

// ---------------------------------------------------------------------------
// URL / target parsing
// ---------------------------------------------------------------------------

/// Parse a Bitbucket Cloud remote URL into a `ForgeRepository`.
///
/// Handles HTTPS (`https://bitbucket.org/ws/repo.git`), SCP-like
/// (`git@bitbucket.org:ws/repo.git`), and `ssh://` scheme URLs. Matches only
/// the `bitbucket.org` host (with `altssh.bitbucket.org` mapped back to it).
pub fn parse_bitbucket_remote_url(remote_url: &str) -> Option<ForgeRepository> {
    let trimmed = trim_url_suffix(remote_url.trim());
    if trimmed.is_empty() {
        return None;
    }

    if let Some((host, path)) = parse_scp_like_remote(trimmed) {
        if !is_bitbucket_host(&normalize_bb_host(host)) {
            return None;
        }
        return bb_repository_from_path(path);
    }

    let without_scheme = strip_scheme(trimmed).unwrap_or(trimmed);
    let without_user = without_scheme
        .rsplit_once('@')
        .map(|(_, rest)| rest)
        .unwrap_or(without_scheme);
    let (host, path) = without_user.split_once('/')?;
    if !is_bitbucket_host(&normalize_bb_host(host)) {
        return None;
    }
    bb_repository_from_path(path)
}

/// Parse a pull request target in Bitbucket form: numeric id, a PR URL
/// (`https://bitbucket.org/ws/repo/pull-requests/N`), or `ws/repo#N`.
pub fn parse_pull_request_target_bitbucket(input: &str) -> Result<PullRequestTarget> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return malformed_target(input);
    }
    if let Some(target) = parse_numeric_target(trimmed) {
        return Ok(target);
    }
    if let Some(target) = parse_bb_url_target(trimmed) {
        return Ok(target);
    }
    if let Some(target) = parse_bb_repo_hash_target(trimmed) {
        return Ok(target);
    }
    malformed_target(input)
}

fn parse_numeric_target(target: &str) -> Option<PullRequestTarget> {
    if !target.chars().all(|ch| ch.is_ascii_digit()) {
        return None;
    }
    let number = target.parse::<u64>().ok()?;
    if number == 0 {
        return None;
    }
    Some(PullRequestTarget::number(number, target))
}

fn parse_bb_url_target(target: &str) -> Option<PullRequestTarget> {
    let without_scheme = strip_scheme(target)?;
    let trimmed = trim_url_suffix(without_scheme);
    let parts: Vec<&str> = trimmed.split('/').filter(|p| !p.is_empty()).collect();
    // Expected: [host, workspace, repo, "pull-requests", <n>]
    if parts.len() < 5 {
        return None;
    }
    if !is_bitbucket_host(&normalize_bb_host(parts[0])) {
        return None;
    }
    if parts[3] != "pull-requests" {
        return None;
    }
    let number = parts[4].parse::<u64>().ok()?;
    if number == 0 {
        return None;
    }
    Some(PullRequestTarget::with_repository(
        ForgeRepository::bitbucket(BITBUCKET_HOST, parts[1], strip_git_suffix(parts[2])),
        number,
        target,
    ))
}

fn parse_bb_repo_hash_target(target: &str) -> Option<PullRequestTarget> {
    let (repo_part, number_part) = target.split_once('#')?;
    let number = number_part.parse::<u64>().ok()?;
    if number == 0 {
        return None;
    }
    let parts: Vec<&str> = repo_part.split('/').filter(|p| !p.is_empty()).collect();
    let repository = match parts.as_slice() {
        [workspace, name] => {
            ForgeRepository::bitbucket(BITBUCKET_HOST, *workspace, strip_git_suffix(name))
        }
        [host, workspace, name] if host.contains('.') => {
            if !is_bitbucket_host(&normalize_bb_host(host)) {
                return None;
            }
            ForgeRepository::bitbucket(BITBUCKET_HOST, *workspace, strip_git_suffix(name))
        }
        _ => return None,
    };
    Some(PullRequestTarget::with_repository(
        repository, number, target,
    ))
}

fn bb_repository_from_path(path: &str) -> Option<ForgeRepository> {
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    // Bitbucket Cloud repositories are always `workspace/repo` (no subgroups).
    if parts.len() < 2 {
        return None;
    }
    Some(ForgeRepository::bitbucket(
        BITBUCKET_HOST,
        parts[0],
        strip_git_suffix(trim_url_suffix(parts[1])),
    ))
}

fn is_bitbucket_host(host: &str) -> bool {
    host.eq_ignore_ascii_case(BITBUCKET_HOST)
}

/// Map Bitbucket's SSH-over-443 transport host back to its canonical host.
fn normalize_bb_host(host: &str) -> String {
    let host = host.split(':').next().unwrap_or(host);
    if host.eq_ignore_ascii_case("altssh.bitbucket.org") {
        BITBUCKET_HOST.to_string()
    } else {
        host.to_string()
    }
}

fn parse_scp_like_remote(remote_url: &str) -> Option<(&str, &str)> {
    if remote_url.contains("://") {
        return None;
    }
    let (host_part, path) = remote_url.split_once(':')?;
    if host_part.contains('/') || path.is_empty() {
        return None;
    }
    let host = host_part
        .rsplit_once('@')
        .map(|(_, host)| host)
        .unwrap_or(host_part);
    Some((host, path))
}

fn strip_scheme(value: &str) -> Option<&str> {
    value
        .strip_prefix("https://")
        .or_else(|| value.strip_prefix("http://"))
        .or_else(|| value.strip_prefix("ssh://"))
}

fn trim_url_suffix(value: &str) -> &str {
    value
        .split(['?', '#'])
        .next()
        .unwrap_or(value)
        .trim_end_matches('/')
}

fn strip_git_suffix(value: &str) -> &str {
    value.strip_suffix(".git").unwrap_or(value)
}

fn malformed_target(input: &str) -> Result<PullRequestTarget> {
    Err(TuicrError::Forge(format!(
        "`{input}` is not a recognized Bitbucket pull request target"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::traits::ForgeKind;
    use std::cell::RefCell;
    use std::collections::VecDeque;

    struct RecordingClient {
        responses: RefCell<VecDeque<Result<String>>>,
        calls: RefCell<Vec<(String, String, Option<String>)>>,
    }

    impl RecordingClient {
        fn new(responses: Vec<&str>) -> Self {
            Self {
                responses: RefCell::new(
                    responses.into_iter().map(|r| Ok(r.to_string())).collect(),
                ),
                calls: RefCell::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<(String, String, Option<String>)> {
            self.calls.borrow().clone()
        }
    }

    impl BitbucketHttpClient for RecordingClient {
        fn get(&self, url: &str) -> Result<String> {
            self.calls
                .borrow_mut()
                .push(("GET".to_string(), url.to_string(), None));
            self.responses
                .borrow_mut()
                .pop_front()
                .unwrap_or_else(|| Err(TuicrError::Forge("no response queued".to_string())))
        }

        fn post(&self, url: &str, body: &str) -> Result<String> {
            self.calls.borrow_mut().push((
                "POST".to_string(),
                url.to_string(),
                Some(body.to_string()),
            ));
            self.responses
                .borrow_mut()
                .pop_front()
                .unwrap_or_else(|| Err(TuicrError::Forge("no response queued".to_string())))
        }
    }

    fn repo() -> ForgeRepository {
        ForgeRepository::bitbucket("bitbucket.org", "myworkspace", "myrepo")
    }

    fn details() -> PullRequestDetails {
        PullRequestDetails {
            repository: repo(),
            number: 7,
            title: "t".to_string(),
            url: "https://bitbucket.org/myworkspace/myrepo/pull-requests/7".to_string(),
            state: "OPEN".to_string(),
            is_draft: false,
            author: None,
            head_ref_name: "feature".to_string(),
            base_ref_name: "main".to_string(),
            head_sha: "headsha".to_string(),
            base_sha: "basesha".to_string(),
            body: String::new(),
            updated_at: None,
            closed: false,
            merged_at: None,
            diff_start_sha: None,
        }
    }

    #[test]
    fn should_list_open_pull_requests() {
        let page = r#"{ "values": [
            { "id": 1, "title": "First", "state": "OPEN",
              "source": { "branch": { "name": "f1" } },
              "destination": { "branch": { "name": "main" } } }
        ] }"#;
        let client = RecordingClient::new(vec![page]);
        let backend = BitbucketBackend::with_client(Some(repo()), client);
        let query = PullRequestListQuery::first_page(repo(), 30);
        let result = backend.list_pull_requests(query).unwrap();
        assert_eq!(result.pull_requests.len(), 1);
        assert_eq!(result.pull_requests[0].number, 1);
        assert!(!result.has_more);
        let calls = backend.client.calls();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].1.contains("/repositories/myworkspace/myrepo/pullrequests"));
        assert!(calls[0].1.contains("state=OPEN"));
    }

    #[test]
    fn should_get_pull_request_details() {
        let json = r#"{
            "id": 7, "title": "Fix", "description": "b", "state": "OPEN",
            "source": { "branch": { "name": "fix" }, "commit": { "hash": "h" } },
            "destination": { "branch": { "name": "main" }, "commit": { "hash": "base" } }
        }"#;
        let client = RecordingClient::new(vec![json]);
        let backend = BitbucketBackend::with_client(Some(repo()), client);
        let target = PullRequestTarget::number(7, "7");
        let details = backend.get_pull_request(target).unwrap();
        assert_eq!(details.head_sha, "h");
        assert_eq!(details.base_sha, "base");
        let calls = backend.client.calls();
        assert!(calls[0].1.ends_with("/pullrequests/7"));
    }

    #[test]
    fn should_return_diff_unmodified() {
        let diff = "diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n-a\n+b\n";
        let client = RecordingClient::new(vec![diff]);
        let backend = BitbucketBackend::with_client(Some(repo()), client);
        let out = backend.get_pull_request_diff(&details()).unwrap();
        assert_eq!(out, diff);
        assert!(backend.client.calls()[0].1.ends_with("/pullrequests/7/diff"));
    }

    #[test]
    fn should_list_commits_oldest_first() {
        // Bitbucket returns newest-first; backend reverses to oldest-first.
        let page = r#"{ "values": [
            { "hash": "newsha", "message": "newest" },
            { "hash": "oldsha", "message": "oldest" }
        ] }"#;
        let client = RecordingClient::new(vec![page]);
        let backend = BitbucketBackend::with_client(Some(repo()), client);
        let commits = backend.list_pull_request_commits(&details()).unwrap();
        assert_eq!(commits[0].oid, "oldsha");
        assert_eq!(commits[1].oid, "newsha");
    }

    #[test]
    fn should_post_inline_comment_and_approve() {
        // Responses: inline comment POST, then approve POST.
        let client = RecordingClient::new(vec![r#"{ "id": 555 }"#, r#"{}"#]);
        let backend = BitbucketBackend::with_client(Some(repo()), client);
        let inline = crate::forge::submit::InlineComment {
            path: PathBuf::from("src/lib.rs"),
            line: 12,
            side: GhSide::Right,
            counterpart_line: None,
            start_line: None,
            start_side: None,
            old_path: None,
            body: "looks good".to_string(),
            comment_id: "c1".to_string(),
        };
        let comments = vec![inline];
        let request = CreateReviewRequest {
            event: SubmitEvent::Approve,
            commit_id: "headsha",
            body: "",
            comments: &comments,
        };
        let response = backend.create_review(&details(), request).unwrap();
        assert_eq!(response.id, 555);
        assert_eq!(response.state, "APPROVED");
        let calls = backend.client.calls();
        assert_eq!(calls.len(), 2);
        // First: inline comment with `to` anchor.
        assert!(calls[0].1.ends_with("/pullrequests/7/comments"));
        let body = calls[0].2.as_deref().unwrap();
        assert!(body.contains("\"to\":12"));
        assert!(body.contains("src/lib.rs"));
        // Second: approve.
        assert!(calls[1].1.ends_with("/pullrequests/7/approve"));
    }

    #[test]
    fn should_reject_draft_submit() {
        let client = RecordingClient::new(vec![]);
        let backend = BitbucketBackend::with_client(Some(repo()), client);
        let comments: Vec<crate::forge::submit::InlineComment> = vec![];
        let request = CreateReviewRequest {
            event: SubmitEvent::Draft,
            commit_id: "headsha",
            body: "x",
            comments: &comments,
        };
        let err = backend.create_review(&details(), request).unwrap_err();
        assert!(matches!(err, TuicrError::UnsupportedOperation(_)));
        assert!(backend.client.calls().is_empty());
    }

    #[test]
    fn should_error_commit_range_diff_without_checkout() {
        let client = RecordingClient::new(vec![]);
        let backend = BitbucketBackend::with_client(Some(repo()), client);
        let err = backend
            .get_pull_request_commit_range_diff(&details(), "a", "b")
            .unwrap_err();
        assert!(matches!(err, TuicrError::UnsupportedOperation(_)));
    }

    #[test]
    fn should_parse_https_remote_url() {
        let repo = parse_bitbucket_remote_url("https://bitbucket.org/myworkspace/myrepo.git").unwrap();
        assert_eq!(repo.kind, ForgeKind::Bitbucket);
        assert_eq!(repo.host, "bitbucket.org");
        assert_eq!(repo.owner, "myworkspace");
        assert_eq!(repo.name, "myrepo");
    }

    #[test]
    fn should_parse_scp_remote_url() {
        let repo = parse_bitbucket_remote_url("git@bitbucket.org:myworkspace/myrepo.git").unwrap();
        assert_eq!(repo.owner, "myworkspace");
        assert_eq!(repo.name, "myrepo");
    }

    #[test]
    fn should_map_altssh_host() {
        let repo = parse_bitbucket_remote_url("git@altssh.bitbucket.org:ws/repo.git").unwrap();
        assert_eq!(repo.host, "bitbucket.org");
        assert_eq!(repo.owner, "ws");
    }

    #[test]
    fn should_reject_non_bitbucket_remote() {
        assert!(parse_bitbucket_remote_url("https://github.com/owner/repo.git").is_none());
        assert!(parse_bitbucket_remote_url("git@gitlab.com:owner/repo.git").is_none());
    }

    #[test]
    fn should_parse_pull_request_url_target() {
        let target = parse_pull_request_target_bitbucket(
            "https://bitbucket.org/myworkspace/myrepo/pull-requests/42",
        )
        .unwrap();
        assert_eq!(target.number, 42);
        let repo = target.repository.unwrap();
        assert_eq!(repo.owner, "myworkspace");
        assert_eq!(repo.name, "myrepo");
        assert_eq!(repo.kind, ForgeKind::Bitbucket);
    }

    #[test]
    fn should_parse_repo_hash_target() {
        let target = parse_pull_request_target_bitbucket("myworkspace/myrepo#5").unwrap();
        assert_eq!(target.number, 5);
        assert_eq!(target.repository.unwrap().owner, "myworkspace");
    }

    #[test]
    fn should_reject_non_pr_url_target() {
        assert!(parse_pull_request_target_bitbucket("https://bitbucket.org/ws/repo/commits").is_err());
    }
}
