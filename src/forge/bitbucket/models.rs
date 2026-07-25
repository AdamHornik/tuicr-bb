use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::forge::remote_comments::{RemoteCommentSide, RemoteReviewComment, RemoteReviewThread};
use crate::forge::traits::{
    ForgeRepository, PullRequestCommit, PullRequestDetails, PullRequestSummary,
};

/// A page of results from a paginated Bitbucket Cloud API 2.0 endpoint.
/// `next` is an absolute URL to the following page, absent on the last page.
#[derive(Debug, Deserialize)]
pub struct BbPage<T> {
    #[serde(default = "empty_vec")]
    pub values: Vec<T>,
    #[serde(default)]
    pub next: Option<String>,
}

/// Default for `BbPage::values` that does not impose a `T: Default` bound
/// (which `#[serde(default)]` on a generic field would).
fn empty_vec<T>() -> Vec<T> {
    Vec::new()
}

#[derive(Debug, Deserialize, Default)]
pub struct BbUser {
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub nickname: Option<String>,
    #[serde(default)]
    pub uuid: Option<String>,
}

impl BbUser {
    /// Prefer the account handle (`nickname`) and fall back to the human name.
    fn handle(&self) -> Option<String> {
        self.nickname
            .as_ref()
            .filter(|s| !s.is_empty())
            .cloned()
            .or_else(|| non_empty(&self.display_name))
    }
}

#[derive(Debug, Deserialize, Default)]
pub struct BbBranch {
    #[serde(default)]
    pub name: String,
}

#[derive(Debug, Deserialize, Default)]
pub struct BbCommitRef {
    #[serde(default)]
    pub hash: String,
}

#[derive(Debug, Deserialize, Default)]
pub struct BbEndpoint {
    #[serde(default)]
    pub branch: BbBranch,
    #[serde(default)]
    pub commit: BbCommitRef,
}

#[derive(Debug, Deserialize, Default)]
pub struct BbLinks {
    #[serde(default)]
    pub html: Option<BbLink>,
}

#[derive(Debug, Deserialize, Default)]
pub struct BbLink {
    #[serde(default)]
    pub href: String,
}

#[derive(Debug, Deserialize)]
pub struct BbPrSummary {
    pub id: u64,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub author: Option<BbUser>,
    #[serde(default)]
    pub source: BbEndpoint,
    #[serde(default)]
    pub destination: BbEndpoint,
    #[serde(default)]
    pub links: BbLinks,
    #[serde(default)]
    pub updated_on: Option<DateTime<Utc>>,
    #[serde(default)]
    pub draft: bool,
}

impl BbPrSummary {
    pub fn into_summary(self, repo: &ForgeRepository) -> PullRequestSummary {
        PullRequestSummary {
            repository: repo.clone(),
            number: self.id,
            title: self.title,
            author: self.author.and_then(|a| a.handle()),
            head_ref_name: self.source.branch.name,
            base_ref_name: self.destination.branch.name,
            updated_at: self.updated_on,
            url: html_href(&self.links),
            state: normalize_state(&self.state),
            is_draft: self.draft,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct BbPrDetails {
    pub id: u64,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub author: Option<BbUser>,
    #[serde(default)]
    pub source: BbEndpoint,
    #[serde(default)]
    pub destination: BbEndpoint,
    #[serde(default)]
    pub links: BbLinks,
    #[serde(default)]
    pub updated_on: Option<DateTime<Utc>>,
    #[serde(default)]
    pub draft: bool,
}

impl BbPrDetails {
    pub fn into_details(self, repo: &ForgeRepository) -> PullRequestDetails {
        let state = normalize_state(&self.state);
        // Bitbucket exposes no dedicated merge timestamp; a MERGED state means
        // the PR is read-only, so surface `updated_on` as the merge time to
        // drive the read-only "merged" reason. DECLINED/SUPERSEDED just close.
        let merged_at = (state == "MERGED").then_some(self.updated_on).flatten();
        let closed = state != "OPEN";
        PullRequestDetails {
            repository: repo.clone(),
            number: self.id,
            title: self.title,
            url: html_href(&self.links),
            state,
            is_draft: self.draft,
            author: self.author.and_then(|a| a.handle()),
            head_ref_name: self.source.branch.name,
            base_ref_name: self.destination.branch.name,
            head_sha: self.source.commit.hash,
            base_sha: self.destination.commit.hash,
            body: self.description,
            updated_at: self.updated_on,
            closed,
            merged_at,
            diff_start_sha: None,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct BbCommit {
    #[serde(default)]
    pub hash: String,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub date: Option<DateTime<Utc>>,
    #[serde(default)]
    pub author: Option<BbCommitAuthor>,
}

#[derive(Debug, Deserialize, Default)]
pub struct BbCommitAuthor {
    /// `"Name <email>"` form; used as a fallback when no linked account.
    #[serde(default)]
    pub raw: String,
    #[serde(default)]
    pub user: Option<BbUser>,
}

impl BbCommit {
    pub fn into_pull_request_commit(self) -> PullRequestCommit {
        let short_oid = self.hash.chars().take(7).collect();
        let summary = self.message.lines().next().unwrap_or("").to_string();
        let author = self
            .author
            .map(|a| {
                a.user
                    .and_then(|u| u.handle())
                    .unwrap_or_else(|| strip_email(&a.raw))
            })
            .unwrap_or_default();
        PullRequestCommit {
            oid: self.hash,
            short_oid,
            summary,
            author,
            timestamp: self.date,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct BbComment {
    pub id: u64,
    #[serde(default)]
    pub content: BbContent,
    #[serde(default)]
    pub user: Option<BbUser>,
    #[serde(default)]
    pub created_on: Option<DateTime<Utc>>,
    #[serde(default)]
    pub deleted: bool,
    #[serde(default)]
    pub parent: Option<BbCommentParent>,
    #[serde(default)]
    pub inline: Option<BbInline>,
    #[serde(default)]
    pub resolution: Option<BbResolution>,
    #[serde(default)]
    pub links: BbLinks,
}

#[derive(Debug, Deserialize, Default)]
pub struct BbContent {
    #[serde(default)]
    pub raw: String,
}

#[derive(Debug, Deserialize)]
pub struct BbCommentParent {
    pub id: u64,
}

#[derive(Debug, Deserialize, Default)]
pub struct BbInline {
    #[serde(default)]
    pub path: String,
    /// Line number on the old (left) side; set for deletions/context.
    #[serde(default)]
    pub from: Option<u32>,
    /// Line number on the new (right) side; set for additions/context.
    #[serde(default)]
    pub to: Option<u32>,
}

/// Present only when a comment thread has been marked resolved.
#[derive(Debug, Deserialize)]
pub struct BbResolution {
    #[serde(default)]
    pub resolved_on: Option<DateTime<Utc>>,
}

impl BbComment {
    fn author_handle(&self) -> Option<String> {
        self.user.as_ref().and_then(|u| u.handle())
    }

    fn into_review_comment(self) -> RemoteReviewComment {
        let in_reply_to = self.parent.map(|p| p.id.to_string());
        RemoteReviewComment {
            id: self.id.to_string(),
            author: self.user.and_then(|u| u.handle()),
            body: self.content.raw,
            created_at: self.created_on,
            in_reply_to,
            url: html_href(&self.links),
        }
    }
}

/// Group a flat list of Bitbucket PR comments into review threads.
///
/// Bitbucket returns comments as a flat list linked by `parent.id`. Each root
/// comment (no parent) becomes a thread; replies are attached to their root in
/// posted order. Inline roots carry a file/line anchor; general roots surface
/// as path-less threads (the PR-level discussion), mirroring GitLab.
pub fn group_comments_into_threads(comments: Vec<BbComment>) -> Vec<RemoteReviewThread> {
    use std::collections::HashMap;

    // Replies keyed by their root's id, preserving input (posted) order.
    let mut replies: HashMap<u64, Vec<BbComment>> = HashMap::new();
    let mut roots: Vec<BbComment> = Vec::new();
    for comment in comments {
        if comment.deleted {
            continue;
        }
        match comment.parent.as_ref().map(|p| p.id) {
            Some(parent_id) => replies.entry(parent_id).or_default().push(comment),
            None => roots.push(comment),
        }
    }

    let mut threads = Vec::new();
    for root in roots {
        // Anchor: inline `to` → right side; else inline `from` → left side.
        // Both absent (or no inline) leaves the thread path-less/line-less.
        let (path, line, side) = match &root.inline {
            Some(inline) => {
                if let Some(to) = inline.to {
                    (inline.path.clone(), Some(to), RemoteCommentSide::Right)
                } else if let Some(from) = inline.from {
                    (inline.path.clone(), Some(from), RemoteCommentSide::Left)
                } else {
                    (inline.path.clone(), None, RemoteCommentSide::Right)
                }
            }
            None => (String::new(), None, RemoteCommentSide::Right),
        };

        let root_id = root.id;
        let is_resolved = root.resolution.is_some();
        if root.content.raw.is_empty() {
            continue;
        }

        let mut thread_comments = vec![root.into_review_comment()];
        if let Some(children) = replies.remove(&root_id) {
            thread_comments.extend(
                children
                    .into_iter()
                    .filter(|c| !c.content.raw.is_empty())
                    .map(BbComment::into_review_comment),
            );
        }

        threads.push(RemoteReviewThread {
            id: root_id.to_string(),
            path,
            line,
            side,
            is_resolved,
            is_outdated: false,
            comments: thread_comments,
        });
    }
    threads
}

fn html_href(links: &BbLinks) -> String {
    links
        .html
        .as_ref()
        .map(|l| l.href.clone())
        .unwrap_or_default()
}

fn non_empty(value: &str) -> Option<String> {
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

/// Turn a `"Name <email>"` author string into just `"Name"`.
fn strip_email(raw: &str) -> String {
    raw.split(" <").next().unwrap_or(raw).trim().to_string()
}

/// Normalize Bitbucket PR states to tuicr's uppercase vocabulary.
/// Bitbucket uses OPEN / MERGED / DECLINED / SUPERSEDED.
fn normalize_state(state: &str) -> String {
    match state.to_ascii_uppercase().as_str() {
        "OPEN" => "OPEN".to_string(),
        "MERGED" => "MERGED".to_string(),
        "DECLINED" => "CLOSED".to_string(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> ForgeRepository {
        ForgeRepository::bitbucket("bitbucket.org", "myworkspace", "myrepo")
    }

    #[test]
    fn should_map_pr_summary_into_summary() {
        let json = r#"{
            "id": 42,
            "title": "Add feature",
            "state": "OPEN",
            "author": { "display_name": "Alice A", "nickname": "alice" },
            "source": { "branch": { "name": "feature" }, "commit": { "hash": "aaa" } },
            "destination": { "branch": { "name": "main" }, "commit": { "hash": "bbb" } },
            "links": { "html": { "href": "https://bitbucket.org/myworkspace/myrepo/pull-requests/42" } },
            "updated_on": "2024-01-01T00:00:00Z",
            "draft": false
        }"#;
        let summary: BbPrSummary = serde_json::from_str(json).unwrap();
        let mapped = summary.into_summary(&repo());
        assert_eq!(mapped.number, 42);
        assert_eq!(mapped.title, "Add feature");
        assert_eq!(mapped.author.as_deref(), Some("alice"));
        assert_eq!(mapped.head_ref_name, "feature");
        assert_eq!(mapped.base_ref_name, "main");
        assert_eq!(mapped.state, "OPEN");
        assert_eq!(
            mapped.url,
            "https://bitbucket.org/myworkspace/myrepo/pull-requests/42"
        );
    }

    #[test]
    fn should_map_pr_details_with_shas() {
        let json = r#"{
            "id": 7,
            "title": "Fix bug",
            "description": "body text",
            "state": "OPEN",
            "author": { "display_name": "Bob", "nickname": "bob" },
            "source": { "branch": { "name": "fix" }, "commit": { "hash": "headsha" } },
            "destination": { "branch": { "name": "main" }, "commit": { "hash": "basesha" } },
            "links": { "html": { "href": "https://bitbucket.org/w/r/pull-requests/7" } },
            "updated_on": "2024-01-02T00:00:00Z",
            "draft": true
        }"#;
        let details: BbPrDetails = serde_json::from_str(json).unwrap();
        let mapped = details.into_details(&repo());
        assert_eq!(mapped.number, 7);
        assert_eq!(mapped.head_sha, "headsha");
        assert_eq!(mapped.base_sha, "basesha");
        assert_eq!(mapped.body, "body text");
        assert!(mapped.is_draft);
        assert!(!mapped.closed);
        assert!(mapped.merged_at.is_none());
    }

    #[test]
    fn should_flag_merged_pr_as_read_only() {
        let json = r#"{
            "id": 8,
            "state": "MERGED",
            "source": { "commit": { "hash": "h" } },
            "destination": { "commit": { "hash": "b" } },
            "updated_on": "2024-01-03T00:00:00Z"
        }"#;
        let details: BbPrDetails = serde_json::from_str(json).unwrap();
        let mapped = details.into_details(&repo());
        assert_eq!(mapped.state, "MERGED");
        assert!(mapped.closed);
        assert!(mapped.merged_at.is_some());
        assert!(mapped.is_read_only());
        assert_eq!(mapped.read_only_reason(), Some("merged"));
    }

    #[test]
    fn should_map_commit_summary_and_short_oid() {
        let json = r#"{
            "hash": "abcdef1234567890",
            "message": "Subject line\n\nBody paragraph",
            "date": "2024-01-01T00:00:00Z",
            "author": { "raw": "Alice A <alice@example.com>", "user": { "nickname": "alice" } }
        }"#;
        let commit: BbCommit = serde_json::from_str(json).unwrap();
        let mapped = commit.into_pull_request_commit();
        assert_eq!(mapped.oid, "abcdef1234567890");
        assert_eq!(mapped.short_oid, "abcdef1");
        assert_eq!(mapped.summary, "Subject line");
        assert_eq!(mapped.author, "alice");
    }

    #[test]
    fn should_fall_back_to_raw_author_without_linked_user() {
        let json = r#"{
            "hash": "deadbeef",
            "message": "Msg",
            "author": { "raw": "Carol C <carol@example.com>" }
        }"#;
        let commit: BbCommit = serde_json::from_str(json).unwrap();
        assert_eq!(commit.into_pull_request_commit().author, "Carol C");
    }

    #[test]
    fn should_group_inline_comment_with_reply() {
        let json = r#"[
            {
                "id": 100,
                "content": { "raw": "root comment" },
                "user": { "nickname": "alice" },
                "inline": { "path": "src/lib.rs", "to": 10 },
                "resolution": null
            },
            {
                "id": 101,
                "content": { "raw": "a reply" },
                "user": { "nickname": "bob" },
                "parent": { "id": 100 }
            }
        ]"#;
        let comments: Vec<BbComment> = serde_json::from_str(json).unwrap();
        let threads = group_comments_into_threads(comments);
        assert_eq!(threads.len(), 1);
        let thread = &threads[0];
        assert_eq!(thread.path, "src/lib.rs");
        assert_eq!(thread.line, Some(10));
        assert_eq!(thread.side, RemoteCommentSide::Right);
        assert!(!thread.is_resolved);
        assert_eq!(thread.comments.len(), 2);
        assert_eq!(thread.comments[0].body, "root comment");
        assert_eq!(thread.comments[1].body, "a reply");
    }

    #[test]
    fn should_mark_left_side_and_resolved() {
        let json = r#"[
            {
                "id": 200,
                "content": { "raw": "deletion note" },
                "inline": { "path": "old.rs", "from": 5 },
                "resolution": { "resolved_on": "2024-01-01T00:00:00Z" }
            }
        ]"#;
        let comments: Vec<BbComment> = serde_json::from_str(json).unwrap();
        let threads = group_comments_into_threads(comments);
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0].side, RemoteCommentSide::Left);
        assert_eq!(threads[0].line, Some(5));
        assert!(threads[0].is_resolved);
    }

    #[test]
    fn should_skip_deleted_comments() {
        let json = r#"[
            {
                "id": 300,
                "content": { "raw": "" },
                "deleted": true,
                "inline": { "path": "x.rs", "to": 1 }
            }
        ]"#;
        let comments: Vec<BbComment> = serde_json::from_str(json).unwrap();
        assert!(group_comments_into_threads(comments).is_empty());
    }

    #[test]
    fn should_treat_general_comment_as_pathless_thread() {
        let json = r#"[
            { "id": 400, "content": { "raw": "overall LGTM" }, "user": { "nickname": "alice" } }
        ]"#;
        let comments: Vec<BbComment> = serde_json::from_str(json).unwrap();
        let threads = group_comments_into_threads(comments);
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0].path, "");
        assert_eq!(threads[0].line, None);
    }
}
