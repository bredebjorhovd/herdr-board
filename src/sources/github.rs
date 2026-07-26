//! GitHub source: read-only in v0 (impl spec §4.2). Linear stays the system of
//! record; GitHub contributes issues and, more importantly, the pull requests
//! that flip a task to `review`.

use crate::db::UpsertTask;
use crate::model::{Source, UpstreamState};
use anyhow::{Result, anyhow, bail};
use serde_json::Value;

pub const API: &str = "https://api.github.com";

/// The REST surface we use. A seam, so tests never touch the network.
pub trait Rest {
    fn get(&self, path: &str) -> Result<Value>;
    /// POST with a JSON body (comments).
    fn post(&self, path: &str, body: &Value) -> Result<Value>;
    /// PATCH with a JSON body (closing an issue).
    fn patch(&self, path: &str, body: &Value) -> Result<Value>;
    /// PUT with a JSON body (merging a pull request).
    fn put(&self, path: &str, body: &Value) -> Result<Value>;
}

pub struct HttpRest {
    client: reqwest::blocking::Client,
    token: Option<String>,
    base: String,
}

impl HttpRest {
    pub fn new(token: Option<String>) -> Result<HttpRest> {
        Ok(HttpRest {
            client: reqwest::blocking::Client::builder()
                .timeout(std::time::Duration::from_secs(20))
                .user_agent("herdr-board/0.1")
                .build()?,
            token,
            base: API.to_string(),
        })
    }
}

impl HttpRest {
    fn request(&self, method: reqwest::Method, path: &str, body: Option<&Value>) -> Result<Value> {
        let mut req = self
            .client
            .request(method, format!("{}{}", self.base, path))
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28");
        if let Some(t) = &self.token {
            req = req.header("Authorization", format!("Bearer {t}"));
        }
        if let Some(b) = body {
            req = req.json(b);
        }
        let resp = req.send()?;
        let status = resp.status();
        if status.as_u16() == 403 || status.as_u16() == 429 {
            bail!("github rate limited ({status})");
        }
        if status.as_u16() == 401 {
            bail!("github rejected the token (401) — check GITHUB_TOKEN scopes");
        }
        if !status.is_success() {
            bail!("github HTTP {status} for {path}");
        }
        // A 204 has no body; treat that as success rather than a parse error.
        Ok(resp.json().unwrap_or(Value::Null))
    }
}

impl Rest for HttpRest {
    fn get(&self, path: &str) -> Result<Value> {
        self.request(reqwest::Method::GET, path, None)
    }

    fn post(&self, path: &str, body: &Value) -> Result<Value> {
        self.request(reqwest::Method::POST, path, Some(body))
    }

    fn patch(&self, path: &str, body: &Value) -> Result<Value> {
        self.request(reqwest::Method::PATCH, path, Some(body))
    }

    fn put(&self, path: &str, body: &Value) -> Result<Value> {
        self.request(reqwest::Method::PUT, path, Some(body))
    }
}

#[derive(Debug, Clone)]
pub struct GithubIssue {
    pub repo: String,
    pub number: i64,
    pub node_id: String,
    pub title: String,
    pub body: Option<String>,
    pub url: String,
    pub labels: Vec<String>,
    pub closed: bool,
    pub updated_at: String,
}

impl GithubIssue {
    pub fn task_id(&self) -> String {
        format!("gh:{}#{}", self.repo, self.number)
    }

    /// Display identifier, per the design spec's 8-cell id column.
    pub fn identifier(&self) -> String {
        format!("gh#{}", self.number)
    }

    pub fn to_upsert(&self) -> UpsertTask {
        UpsertTask {
            id: self.task_id(),
            source: Source::Github,
            source_id: self.node_id.clone(),
            identifier: self.identifier(),
            title: self.title.clone(),
            body: self.body.clone(),
            url: self.url.clone(),
            labels: self.labels.clone(),
            source_state: Some(if self.closed { "closed" } else { "open" }.into()),
            linear_team: None,
            linear_project: None,
            upstream: if self.closed {
                UpstreamState::Terminal
            } else {
                UpstreamState::Unstarted
            },
            updated_at: self.updated_at.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct PullRequest {
    pub repo: String,
    pub number: i64,
    pub title: String,
    pub body: Option<String>,
    pub url: String,
    pub head_ref: String,
    pub open: bool,
    pub draft: bool,
    pub updated_at: String,
}

impl PullRequest {
    /// `!` rather than `#` so a PR and an issue of the same number in one repo
    /// stay distinct rows.
    pub fn task_id(&self) -> String {
        format!("gh:{}!{}", self.repo, self.number)
    }

    /// Fits the design's 8-cell id column.
    pub fn identifier(&self) -> String {
        format!("gh!{}", self.number)
    }

    /// An open pull request is work waiting on a human, which is exactly what
    /// the board calls `review`; a closed or merged one is `done`.
    pub fn to_upsert(&self) -> UpsertTask {
        UpsertTask {
            id: self.task_id(),
            source: Source::Github,
            source_id: self.url.clone(),
            identifier: self.identifier(),
            title: self.title.clone(),
            body: self.body.clone(),
            url: self.url.clone(),
            labels: Vec::new(),
            source_state: Some(
                if !self.open {
                    "closed"
                } else if self.draft {
                    "draft"
                } else {
                    "open"
                }
                .into(),
            ),
            linear_team: None,
            linear_project: None,
            upstream: if self.open {
                UpstreamState::Unstarted
            } else {
                UpstreamState::Terminal
            },
            updated_at: self.updated_at.clone(),
        }
    }
}

pub struct Github<T: Rest> {
    pub rest: T,
}

impl<T: Rest> Github<T> {
    pub fn new(rest: T) -> Github<T> {
        Github { rest }
    }

    /// Issues for a repo. GitHub's issues endpoint also returns pull requests;
    /// those carry a `pull_request` key and are filtered out here.
    pub fn issues(&self, repo: &str, labels: &[String]) -> Result<Vec<GithubIssue>> {
        let mut path = format!("/repos/{repo}/issues?state=all&per_page=100");
        if !labels.is_empty() {
            path.push_str(&format!("&labels={}", labels.join(",")));
        }
        let v = self.rest.get(&path)?;
        let arr = v
            .as_array()
            .ok_or_else(|| anyhow!("github issues: expected an array"))?;
        Ok(arr
            .iter()
            .filter(|n| n.get("pull_request").is_none())
            .filter_map(|n| parse_issue(repo, n))
            .collect())
    }

    /// Leave the same trail as Linear gets.
    pub fn comment(&self, repo: &str, number: i64, body: &str) -> Result<()> {
        self.rest.post(
            &format!("/repos/{repo}/issues/{number}/comments"),
            &serde_json::json!({ "body": body }),
        )?;
        Ok(())
    }

    /// Merge a pull request.
    ///
    /// Only ever from an explicit keypress with a confirmation — this is the
    /// one action the board takes that cannot be undone from the board.
    pub fn merge_pr(&self, repo: &str, number: i64) -> Result<()> {
        let r = self.rest.put(
            &format!("/repos/{repo}/pulls/{number}/merge"),
            &serde_json::json!({ "merge_method": "merge" }),
        )?;
        // GitHub answers 200 with `merged: false` when it declines — a dirty
        // mergeable_state, a required check, a review still outstanding.
        if r.get("merged").and_then(Value::as_bool) == Some(false) {
            bail!(
                "github refused the merge: {}",
                r.get("message").and_then(Value::as_str).unwrap_or("no reason given")
            );
        }
        Ok(())
    }

    /// Close on done. Without this, `d mark done` moves the row and the next
    /// poll recomputes `open` upstream and moves it straight back — a key that
    /// undoes itself.
    pub fn close_issue(&self, repo: &str, number: i64) -> Result<()> {
        self.rest.patch(
            &format!("/repos/{repo}/issues/{number}"),
            &serde_json::json!({ "state": "closed" }),
        )?;
        Ok(())
    }

    pub fn pulls(&self, repo: &str) -> Result<Vec<PullRequest>> {
        let v = self
            .rest
            .get(&format!("/repos/{repo}/pulls?state=all&per_page=100"))?;
        let arr = v
            .as_array()
            .ok_or_else(|| anyhow!("github pulls: expected an array"))?;
        Ok(arr.iter().filter_map(|n| parse_pull(repo, n)).collect())
    }
}

fn parse_issue(repo: &str, n: &Value) -> Option<GithubIssue> {
    Some(GithubIssue {
        repo: repo.to_string(),
        number: n.get("number")?.as_i64()?,
        node_id: n
            .get("node_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        title: n
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        body: n.get("body").and_then(Value::as_str).map(str::to_string),
        url: n
            .get("html_url")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        labels: n
            .get("labels")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|l| l.get("name").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        closed: n.get("state").and_then(Value::as_str) == Some("closed"),
        updated_at: n
            .get("updated_at")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    })
}

fn parse_pull(repo: &str, n: &Value) -> Option<PullRequest> {
    Some(PullRequest {
        repo: repo.to_string(),
        number: n.get("number")?.as_i64()?,
        title: n
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        body: n.get("body").and_then(Value::as_str).map(str::to_string),
        draft: n.get("draft").and_then(Value::as_bool).unwrap_or(false),
        updated_at: n
            .get("updated_at")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        url: n
            .get("html_url")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        head_ref: n
            .get("head")
            .and_then(|h| h.get("ref"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        open: n.get("state").and_then(Value::as_str) == Some("open"),
    })
}

/// Does this PR belong to that attempt? Branch match is the primary link
/// (impl spec §4.2); a PR URL recorded on the Linear issue is the other.
pub fn pr_matches_branch(pr: &PullRequest, branch: &str) -> bool {
    !branch.is_empty() && pr.head_ref == branch
}

/// Replays recorded responses keyed by path prefix. Used in tests.
#[cfg(test)]
pub struct FixtureRest {
    pub routes: Vec<(String, Value)>,
    pub asked: std::cell::RefCell<Vec<String>>,
    pub wrote: std::cell::RefCell<Vec<(String, String, Value)>>,
}

#[cfg(test)]
impl FixtureRest {
    pub fn new(routes: Vec<(String, Value)>) -> FixtureRest {
        FixtureRest {
            routes,
            asked: std::cell::RefCell::new(Vec::new()),
            wrote: std::cell::RefCell::new(Vec::new()),
        }
    }
}

#[cfg(test)]
impl Rest for FixtureRest {
    fn get(&self, path: &str) -> Result<Value> {
        self.asked.borrow_mut().push(path.to_string());
        self.routes
            .iter()
            .find(|(p, _)| path.starts_with(p.as_str()))
            .map(|(_, v)| v.clone())
            .ok_or_else(|| anyhow!("no fixture for {path}"))
    }

    fn post(&self, path: &str, body: &Value) -> Result<Value> {
        self.wrote.borrow_mut().push(("POST".into(), path.into(), body.clone()));
        Ok(Value::Null)
    }

    fn patch(&self, path: &str, body: &Value) -> Result<Value> {
        self.wrote.borrow_mut().push(("PATCH".into(), path.into(), body.clone()));
        Ok(Value::Null)
    }

    fn put(&self, path: &str, body: &Value) -> Result<Value> {
        self.wrote.borrow_mut().push(("PUT".into(), path.into(), body.clone()));
        Ok(serde_json::json!({ "merged": true }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn pull_requests_are_filtered_out_of_issues() {
        let g = Github::new(FixtureRest::new(vec![(
            "/repos/o/r/issues".into(),
            json!([
                { "number": 87, "node_id": "n1", "title": "Bug", "html_url": "u",
                  "state": "open", "updated_at": "t", "labels": [{"name":"herd"}] },
                { "number": 88, "node_id": "n2", "title": "A PR", "html_url": "u2",
                  "state": "open", "updated_at": "t", "pull_request": { "url": "x" } }
            ]),
        )]));
        let issues = g.issues("o/r", &[]).unwrap();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].number, 87);
        assert_eq!(issues[0].task_id(), "gh:o/r#87");
        assert_eq!(issues[0].identifier(), "gh#87");
    }

    #[test]
    fn closed_issues_map_to_terminal_upstream() {
        let g = Github::new(FixtureRest::new(vec![(
            "/repos/o/r/issues".into(),
            json!([{ "number": 87, "node_id": "n1", "title": "Bug", "html_url": "u",
                     "state": "closed", "updated_at": "t", "labels": [] }]),
        )]));
        let up = g.issues("o/r", &[]).unwrap()[0].to_upsert();
        assert_eq!(up.upstream, UpstreamState::Terminal);
        assert_eq!(up.source_state.as_deref(), Some("closed"));
    }

    #[test]
    fn label_filter_reaches_the_query_string() {
        let g = Github::new(FixtureRest::new(vec![("/repos".into(), json!([]))]));
        g.issues("o/r", &["herd".into(), "bug".into()]).unwrap();
        assert!(g.rest.asked.borrow()[0].contains("labels=herd,bug"));
    }

    #[test]
    fn an_open_pull_request_becomes_a_review_row() {
        let g = Github::new(FixtureRest::new(vec![(
            "/repos/o/r/pulls".into(),
            json!([{ "number": 508, "title": "Fix the gate", "html_url": "u",
                     "state": "open", "updated_at": "t", "draft": false,
                     "head": { "ref": "fix/rls" } }]),
        )]));
        let pr = &g.pulls("o/r").unwrap()[0];
        assert_eq!(pr.task_id(), "gh:o/r!508");
        assert_eq!(pr.identifier(), "gh!508");
        let up = pr.to_upsert();
        // Open: not terminal, so derivation with pr_open reaches `review`.
        assert_eq!(up.upstream, UpstreamState::Unstarted);
        assert_eq!(up.source_state.as_deref(), Some("open"));
    }

    #[test]
    fn a_merged_pull_request_is_done() {
        let g = Github::new(FixtureRest::new(vec![(
            "/repos/o/r/pulls".into(),
            json!([{ "number": 508, "title": "t", "html_url": "u",
                     "state": "closed", "updated_at": "t",
                     "head": { "ref": "fix/rls" } }]),
        )]));
        assert_eq!(
            g.pulls("o/r").unwrap()[0].to_upsert().upstream,
            UpstreamState::Terminal
        );
    }

    #[test]
    fn a_pull_request_id_cannot_collide_with_an_issue_id() {
        let g = Github::new(FixtureRest::new(vec![(
            "/repos/o/r/pulls".into(),
            json!([{ "number": 87, "title": "t", "html_url": "u",
                     "state": "open", "updated_at": "t",
                     "head": { "ref": "x" } }]),
        )]));
        // Issue 87 is `gh:o/r#87`; PR 87 must not be the same row.
        assert_eq!(g.pulls("o/r").unwrap()[0].task_id(), "gh:o/r!87");
    }

    #[test]
    fn a_comment_posts_to_the_issue() {
        let g = Github::new(FixtureRest::new(vec![]));
        g.comment("o/r", 87, "Dispatched to herdr").unwrap();
        let w = g.rest.wrote.borrow();
        assert_eq!(w[0].0, "POST");
        assert_eq!(w[0].1, "/repos/o/r/issues/87/comments");
        assert_eq!(w[0].2["body"], "Dispatched to herdr");
    }

    #[test]
    fn closing_an_issue_patches_its_state() {
        let g = Github::new(FixtureRest::new(vec![]));
        g.close_issue("o/r", 87).unwrap();
        let w = g.rest.wrote.borrow();
        assert_eq!(w[0].0, "PATCH");
        assert_eq!(w[0].1, "/repos/o/r/issues/87");
        assert_eq!(w[0].2["state"], "closed");
    }

    #[test]
    fn merging_puts_to_the_merge_endpoint() {
        let g = Github::new(FixtureRest::new(vec![]));
        g.merge_pr("o/r", 508).unwrap();
        let w = g.rest.wrote.borrow();
        assert_eq!(w[0].0, "PUT");
        assert_eq!(w[0].1, "/repos/o/r/pulls/508/merge");
    }

    #[test]
    fn a_refused_merge_is_an_error_not_a_success() {
        // GitHub answers 200 with merged:false when a check or review blocks it.
        struct Refuses;
        impl Rest for Refuses {
            fn get(&self, _: &str) -> Result<Value> { Ok(Value::Null) }
            fn post(&self, _: &str, _: &Value) -> Result<Value> { Ok(Value::Null) }
            fn patch(&self, _: &str, _: &Value) -> Result<Value> { Ok(Value::Null) }
            fn put(&self, _: &str, _: &Value) -> Result<Value> {
                Ok(json!({ "merged": false, "message": "required status check is pending" }))
            }
        }
        let g = Github::new(Refuses);
        let err = g.merge_pr("o/r", 508).unwrap_err().to_string();
        assert!(err.contains("required status check"), "{err}");
    }

    #[test]
    fn a_pr_links_to_an_attempt_by_branch() {
        let g = Github::new(FixtureRest::new(vec![(
            "/repos/o/r/pulls".into(),
            json!([{ "number": 291, "title": "Add retry",
                     "html_url": "https://github.com/o/r/pull/291",
                     "state": "open", "updated_at": "t",
                     "head": { "ref": "board/lin-142" } }]),
        )]));
        let prs = g.pulls("o/r").unwrap();
        assert!(pr_matches_branch(&prs[0], "board/lin-142"));
        assert!(!pr_matches_branch(&prs[0], "board/lin-999"));
        // An attempt with no branch must never match everything.
        assert!(!pr_matches_branch(&prs[0], ""));
        assert!(prs[0].open);
    }
}
