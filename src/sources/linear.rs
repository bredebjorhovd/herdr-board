//! Linear source: raw GraphQL over reqwest (impl spec §4.1).
//!
//! Auth is a personal API key sent as a bare `Authorization` header — Linear
//! does **not** want `Bearer` for personal keys (that form is for OAuth access
//! tokens).
//!
//! Everything goes through the [`GraphQl`] transport so `sync --once` can be
//! driven against recorded fixtures in tests without touching the network.

use crate::db::UpsertTask;
use crate::model::{Source, UpstreamState};
use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};

pub const ENDPOINT: &str = "https://api.linear.app/graphql";

/// One GraphQL round trip. Implemented by the HTTP client in production and by
/// a fixture player in tests.
pub trait GraphQl {
    fn query(&self, body: &Value) -> Result<Value>;
}

pub struct HttpTransport {
    client: reqwest::blocking::Client,
    key: String,
    endpoint: String,
}

impl HttpTransport {
    pub fn new(key: String) -> Result<HttpTransport> {
        Ok(HttpTransport {
            client: reqwest::blocking::Client::builder()
                .timeout(std::time::Duration::from_secs(20))
                .user_agent("herdr-board/0.1")
                .build()?,
            key,
            endpoint: ENDPOINT.to_string(),
        })
    }
}

impl GraphQl for HttpTransport {
    fn query(&self, body: &Value) -> Result<Value> {
        let resp = self
            .client
            .post(&self.endpoint)
            // Personal API key: bare, not `Bearer`.
            .header("Authorization", &self.key)
            .header("Content-Type", "application/json")
            .json(body)
            .send()?;

        let status = resp.status();
        if status.as_u16() == 429 {
            bail!("linear rate limited (429)");
        }
        let v: Value = resp.json().map_err(|e| anyhow!("linear: bad JSON: {e}"))?;
        if let Some(errors) = v.get("errors").and_then(Value::as_array)
            && !errors.is_empty()
        {
            let msg = errors
                .iter()
                .filter_map(|e| e.get("message").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("; ");
            bail!("linear GraphQL error: {msg}");
        }
        if !status.is_success() {
            bail!("linear HTTP {status}");
        }
        v.get("data")
            .cloned()
            .ok_or_else(|| anyhow!("linear: response had no `data`"))
    }
}

pub struct Linear<T: GraphQl> {
    pub transport: T,
}

/// A Linear issue, flattened to what the board needs.
#[derive(Debug, Clone)]
pub struct LinearIssue {
    pub id: String,
    pub identifier: String,
    pub title: String,
    pub description: Option<String>,
    pub url: String,
    pub updated_at: String,
    pub state_name: String,
    pub state_type: String,
    pub team_key: Option<String>,
    pub project_name: Option<String>,
    pub labels: Vec<String>,
    /// Attachment URLs — how a linked PR reaches the board.
    pub attachment_urls: Vec<String>,
}

impl LinearIssue {
    pub fn task_id(&self) -> String {
        format!("linear:{}", self.identifier)
    }

    pub fn upstream(&self) -> UpstreamState {
        UpstreamState::from_linear_type(&self.state_type)
    }

    pub fn to_upsert(&self) -> UpsertTask {
        UpsertTask {
            id: self.task_id(),
            source: Source::Linear,
            source_id: self.id.clone(),
            identifier: self.identifier.clone(),
            title: self.title.clone(),
            body: self.description.clone(),
            url: self.url.clone(),
            labels: self.labels.clone(),
            source_state: Some(self.state_name.clone()),
            linear_team: self.team_key.clone(),
            linear_project: self.project_name.clone(),
            upstream: self.upstream(),
            updated_at: self.updated_at.clone(),
        }
    }

    /// First attachment that looks like a pull request.
    pub fn pr_url(&self) -> Option<&str> {
        self.attachment_urls
            .iter()
            .find(|u| u.contains("/pull/") || u.contains("/merge_requests/"))
            .map(String::as_str)
    }
}

const ISSUE_FIELDS: &str = r#"
  id
  identifier
  title
  description
  url
  updatedAt
  state { name type }
  team { key }
  project { name }
  labels { nodes { name } }
  attachments { nodes { url } }
"#;

impl<T: GraphQl> Linear<T> {
    pub fn new(transport: T) -> Linear<T> {
        Linear { transport }
    }

    /// Issues that are (assigned to viewer OR carry a routing label) AND are not
    /// in a completed/canceled state.
    ///
    /// `since` is the last-updated watermark; passing it keeps polls cheap by
    /// asking Linear only for what changed.
    pub fn fetch_board_issues(
        &self,
        labels: &[String],
        since: Option<&str>,
        closed_since: Option<&str>,
    ) -> Result<Vec<LinearIssue>> {
        let mut mine = vec![json!({ "assignee": { "isMe": { "eq": true } } })];
        if !labels.is_empty() {
            mine.push(json!({ "labels": { "name": { "in": labels } } }));
        }

        // Unfinished work, plus anything finished recently.
        //
        // Excluding completed issues outright meant a Linear issue you closed
        // simply vanished: dropped from the fetch, then reaped, and it never
        // reached the board's `done` section at all. `closed_since` keeps
        // today's finished work visible, and tomorrow it falls out of the
        // filter and is reaped on its own.
        let mut relevant = vec![json!({
            "state": { "type": { "nin": ["completed", "canceled"] } }
        })];
        if let Some(c) = closed_since {
            relevant.push(json!({ "updatedAt": { "gte": c } }));
        }

        let mut and = vec![json!({ "or": mine }), json!({ "or": relevant })];
        if let Some(s) = since {
            and.push(json!({ "updatedAt": { "gt": s } }));
        }
        self.paged_issues(json!({ "and": and }))
    }

    /// Issues we hold open attempts against, fetched regardless of the board
    /// filter so writeback targets stay fresh even after they leave the queue
    /// (impl spec §4.1).
    pub fn fetch_issues_by_id(&self, ids: &[String]) -> Result<Vec<LinearIssue>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        self.paged_issues(json!({ "id": { "in": ids } }))
    }

    fn paged_issues(&self, filter: Value) -> Result<Vec<LinearIssue>> {
        let query = format!(
            r#"query Board($filter: IssueFilter, $after: String) {{
                 issues(filter: $filter, first: 50, after: $after) {{
                   pageInfo {{ hasNextPage endCursor }}
                   nodes {{ {ISSUE_FIELDS} }}
                 }}
               }}"#
        );
        let mut out = Vec::new();
        let mut after: Option<String> = None;
        // Bounded so a pathological cursor loop cannot spin forever.
        for _ in 0..50 {
            let data = self.transport.query(&json!({
                "query": query,
                "variables": { "filter": filter, "after": after },
            }))?;
            let issues = data
                .get("issues")
                .ok_or_else(|| anyhow!("linear: response had no `issues`"))?;
            for n in issues
                .get("nodes")
                .and_then(Value::as_array)
                .unwrap_or(&Vec::new())
            {
                if let Some(i) = parse_issue(n) {
                    out.push(i);
                }
            }
            let page = issues.get("pageInfo");
            let has_next = page
                .and_then(|p| p.get("hasNextPage"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if !has_next {
                break;
            }
            after = page
                .and_then(|p| p.get("endCursor"))
                .and_then(Value::as_str)
                .map(str::to_string);
            if after.is_none() {
                break;
            }
        }
        Ok(out)
    }

    /// The team's first `started`-type workflow state, which is where a
    /// dispatched issue moves. Resolved by **type**, never by name.
    pub fn started_state_id(&self, team_key: &str) -> Result<Option<String>> {
        self.state_id_of_type(team_key, "started")
    }

    /// A workflow state resolved by **name**, case-insensitively.
    ///
    /// The one place the board matches a state by name, because Linear gives it
    /// no choice: `In Review` and `In Progress` are both `type: started`, so
    /// there is no review type to ask for. Which name means review is config
    /// (`[linear] review_state`), never a guess — see [`crate::config::LinearConfig`].
    ///
    /// No such state comes back as `Err(every state the team has)`, so a caller
    /// can say what the workflow actually offers rather than only that the name
    /// did not resolve — the difference between a fixable message and a shrug.
    pub fn state_id_named(
        &self,
        team_key: &str,
        name: &str,
    ) -> Result<std::result::Result<String, Vec<String>>> {
        let states = self.team_states(team_key)?;
        let found = states
            .iter()
            .find(|s| {
                s.get("name")
                    .and_then(Value::as_str)
                    .is_some_and(|n| n.eq_ignore_ascii_case(name))
            })
            .and_then(|s| s.get("id"))
            .and_then(Value::as_str)
            .map(str::to_string);
        Ok(match found {
            Some(id) => Ok(id),
            None => Err(states
                .iter()
                .filter_map(|s| s.get("name").and_then(Value::as_str))
                .map(str::to_string)
                .collect()),
        })
    }

    fn state_id_of_type(&self, team_key: &str, want: &str) -> Result<Option<String>> {
        let states = self.team_states(team_key)?;
        let mut started: Vec<&Value> = states
            .iter()
            .filter(|s| s.get("type").and_then(Value::as_str) == Some(want))
            .collect();
        started.sort_by(|a, b| {
            let pa = a.get("position").and_then(Value::as_f64).unwrap_or(0.0);
            let pb = b.get("position").and_then(Value::as_f64).unwrap_or(0.0);
            pa.partial_cmp(&pb).unwrap_or(std::cmp::Ordering::Equal)
        });
        Ok(started
            .first()
            .and_then(|s| s.get("id"))
            .and_then(Value::as_str)
            .map(str::to_string))
    }

    /// Every workflow state on a team, in the order Linear returned them.
    fn team_states(&self, team_key: &str) -> Result<Vec<Value>> {
        let data = self.transport.query(&json!({
            "query": r#"query States($key: String!) {
                teams(filter: { key: { eq: $key } }, first: 1) {
                  nodes {
                    id
                    states { nodes { id name type position } }
                  }
                }
              }"#,
            "variables": { "key": team_key },
        }))?;
        Ok(data
            .get("teams")
            .and_then(|t| t.get("nodes"))
            .and_then(Value::as_array)
            .and_then(|n| n.first())
            .and_then(|t| t.get("states"))
            .and_then(|s| s.get("nodes"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default())
    }

    /// Teams the key can see, for generating routes.
    pub fn teams(&self) -> Result<Vec<(String, String)>> {
        let data = self.transport.query(&json!({
            "query": "{ teams(first: 50) { nodes { key name } } }",
        }))?;
        Ok(data
            .get("teams")
            .and_then(|t| t.get("nodes"))
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|t| {
                        Some((
                            t.get("key")?.as_str()?.to_string(),
                            t.get("name")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Team ids by key, for creating an issue against one.
    pub fn team_ids(&self) -> Result<Vec<(String, String)>> {
        let data = self.transport.query(&json!({
            "query": "{ teams(first: 50) { nodes { id key } } }",
        }))?;
        Ok(data
            .get("teams")
            .and_then(|t| t.get("nodes"))
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|t| {
                        Some((
                            t.get("key")?.as_str()?.to_string(),
                            t.get("id")?.as_str()?.to_string(),
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Label ids by name, so a new issue can be routed the moment it exists.
    pub fn label_ids(&self) -> Result<Vec<(String, String)>> {
        let data = self.transport.query(&json!({
            "query": "{ issueLabels(first: 100) { nodes { id name } } }",
        }))?;
        Ok(data
            .get("issueLabels")
            .and_then(|t| t.get("nodes"))
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|l| {
                        Some((
                            l.get("name")?.as_str()?.to_string(),
                            l.get("id")?.as_str()?.to_string(),
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Create an issue, assigned to whoever owns the API key.
    ///
    /// Assigned deliberately: the board's filter is "assigned to me or carrying
    /// a routing label", and an unassigned, unlabelled issue would be created
    /// into invisibility.
    pub fn create_issue(
        &self,
        team_id: &str,
        title: &str,
        body: Option<&str>,
        label_ids: &[String],
    ) -> Result<(String, String)> {
        let viewer = self
            .transport
            .query(&json!({ "query": "{ viewer { id } }" }))?
            .get("viewer")
            .and_then(|v| v.get("id"))
            .and_then(Value::as_str)
            .map(str::to_string);

        let data = self.transport.query(&json!({
            "query": r#"mutation New($i: IssueCreateInput!) {
                issueCreate(input: $i) { success issue { identifier url } }
              }"#,
            "variables": { "i": {
                "teamId": team_id,
                "title": title,
                "description": body,
                "labelIds": label_ids,
                "assigneeId": viewer,
            }},
        }))?;
        check_success(&data, "issueCreate")?;
        let issue = data
            .get("issueCreate")
            .and_then(|c| c.get("issue"))
            .ok_or_else(|| anyhow!("issueCreate returned no issue"))?;
        Ok((
            issue
                .get("identifier")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            issue
                .get("url")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        ))
    }

    /// The team's first `completed`-type state, for closing a finished issue.
    /// Resolved by type, never by name — teams rename these freely.
    pub fn completed_state_id(&self, team_key: &str) -> Result<Option<String>> {
        self.state_id_of_type(team_key, "completed")
    }

    pub fn set_state(&self, issue_id: &str, state_id: &str) -> Result<()> {
        let data = self.transport.query(&json!({
            "query": r#"mutation Move($id: String!, $stateId: String!) {
                issueUpdate(id: $id, input: { stateId: $stateId }) { success }
              }"#,
            "variables": { "id": issue_id, "stateId": state_id },
        }))?;
        check_success(&data, "issueUpdate")
    }

    pub fn comment(&self, issue_id: &str, body: &str) -> Result<()> {
        let data = self.transport.query(&json!({
            "query": r#"mutation Comment($id: String!, $body: String!) {
                commentCreate(input: { issueId: $id, body: $body }) { success }
              }"#,
            "variables": { "id": issue_id, "body": body },
        }))?;
        check_success(&data, "commentCreate")
    }

    /// Attach a PR link. Falls back to a comment when the workspace rejects the
    /// attachment mutation — the spec allows comment-only.
    pub fn attach_link(&self, issue_id: &str, url: &str, title: &str) -> Result<()> {
        let res = self.transport.query(&json!({
            "query": r#"mutation Attach($id: String!, $url: String!, $title: String!) {
                attachmentCreate(input: { issueId: $id, url: $url, title: $title }) { success }
              }"#,
            "variables": { "id": issue_id, "url": url, "title": title },
        }));
        match res.and_then(|d| check_success(&d, "attachmentCreate")) {
            Ok(()) => Ok(()),
            Err(_) => self.comment(issue_id, &format!("{title}: {url}")),
        }
    }
}

fn check_success(data: &Value, field: &str) -> Result<()> {
    let ok = data
        .get(field)
        .and_then(|f| f.get("success"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if ok {
        Ok(())
    } else {
        bail!("linear {field} did not report success")
    }
}

fn parse_issue(n: &Value) -> Option<LinearIssue> {
    let labels = n
        .get("labels")
        .and_then(|l| l.get("nodes"))
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|l| l.get("name").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let attachment_urls = n
        .get("attachments")
        .and_then(|l| l.get("nodes"))
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|l| l.get("url").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    Some(LinearIssue {
        id: n.get("id")?.as_str()?.to_string(),
        identifier: n.get("identifier")?.as_str()?.to_string(),
        title: n
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        description: n
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_string),
        url: n
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        updated_at: n
            .get("updatedAt")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        state_name: n
            .get("state")
            .and_then(|s| s.get("name"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        state_type: n
            .get("state")
            .and_then(|s| s.get("type"))
            .and_then(Value::as_str)
            .unwrap_or("unstarted")
            .to_string(),
        team_key: n
            .get("team")
            .and_then(|t| t.get("key"))
            .and_then(Value::as_str)
            .map(str::to_string),
        project_name: n
            .get("project")
            .and_then(|t| t.get("name"))
            .and_then(Value::as_str)
            .map(str::to_string),
        labels,
        attachment_urls,
    })
}

/// Replays recorded responses in order. Used by the integration test.
#[cfg(test)]
pub struct FixtureTransport {
    responses: std::cell::RefCell<std::collections::VecDeque<Value>>,
    pub sent: std::cell::RefCell<Vec<Value>>,
}

#[cfg(test)]
impl FixtureTransport {
    pub fn new(responses: Vec<Value>) -> FixtureTransport {
        FixtureTransport {
            responses: std::cell::RefCell::new(responses.into()),
            sent: std::cell::RefCell::new(Vec::new()),
        }
    }
}

#[cfg(test)]
impl GraphQl for FixtureTransport {
    fn query(&self, body: &Value) -> Result<Value> {
        self.sent.borrow_mut().push(body.clone());
        self.responses
            .borrow_mut()
            .pop_front()
            .ok_or_else(|| anyhow!("fixture transport ran out of recorded responses"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one_page(nodes: Value) -> Value {
        json!({ "issues": { "pageInfo": { "hasNextPage": false }, "nodes": nodes } })
    }

    fn issue_json() -> Value {
        json!({
            "id": "uuid-1",
            "identifier": "LIN-142",
            "title": "Add retry to Altinn poller",
            "description": "The poller gives up on the first 502.",
            "url": "https://linear.app/offhand/issue/LIN-142",
            "updatedAt": "2026-07-25T18:00:00.000Z",
            "state": { "name": "Todo", "type": "unstarted" },
            "team": { "key": "OFF" },
            "project": { "name": "Compliance" },
            "labels": { "nodes": [ { "name": "herd" }, { "name": "fintech" } ] },
            "attachments": { "nodes": [ { "url": "https://github.com/o/r/pull/291" } ] }
        })
    }

    #[test]
    fn parses_an_issue_into_a_task() {
        let l = Linear::new(FixtureTransport::new(vec![one_page(
            json!([issue_json()]),
        )]));
        let issues = l.fetch_board_issues(&["herd".into()], None, None).unwrap();
        assert_eq!(issues.len(), 1);
        let i = &issues[0];
        assert_eq!(i.task_id(), "linear:LIN-142");
        assert_eq!(i.identifier, "LIN-142");
        assert_eq!(i.team_key.as_deref(), Some("OFF"));
        assert_eq!(i.labels, vec!["herd", "fintech"]);
        assert_eq!(i.upstream(), UpstreamState::Unstarted);
        assert_eq!(i.pr_url(), Some("https://github.com/o/r/pull/291"));
    }

    #[test]
    fn filter_asks_for_viewer_or_label_and_excludes_terminal_states() {
        let l = Linear::new(FixtureTransport::new(vec![one_page(json!([]))]));
        l.fetch_board_issues(&["herd".into()], Some("2026-07-25T00:00:00Z"), None)
            .unwrap();
        let sent = l.transport.sent.borrow();
        let and = &sent[0]["variables"]["filter"]["and"];
        // Mine: assigned to me, or carrying a routing label.
        assert_eq!(and[0]["or"][0]["assignee"]["isMe"]["eq"], json!(true));
        assert_eq!(and[0]["or"][1]["labels"]["name"]["in"], json!(["herd"]));
        // Relevant: not finished.
        assert_eq!(
            and[1]["or"][0]["state"]["type"]["nin"],
            json!(["completed", "canceled"])
        );
        // The watermark keeps polls cheap.
        assert_eq!(and[2]["updatedAt"]["gt"], json!("2026-07-25T00:00:00Z"));
    }

    #[test]
    fn recently_closed_issues_are_asked_for_too() {
        // Otherwise a Linear issue you closed vanishes instead of appearing
        // under `done today`.
        let l = Linear::new(FixtureTransport::new(vec![one_page(json!([]))]));
        l.fetch_board_issues(&[], None, Some("2026-07-26T00:00:00Z"))
            .unwrap();
        let sent = l.transport.sent.borrow();
        let relevant = &sent[0]["variables"]["filter"]["and"][1]["or"];
        assert_eq!(
            relevant[1]["updatedAt"]["gte"],
            json!("2026-07-26T00:00:00Z")
        );
    }

    #[test]
    fn without_labels_the_filter_is_viewer_only() {
        let l = Linear::new(FixtureTransport::new(vec![one_page(json!([]))]));
        l.fetch_board_issues(&[], None, None).unwrap();
        let sent = l.transport.sent.borrow();
        let and = &sent[0]["variables"]["filter"]["and"];
        assert_eq!(and[0]["or"].as_array().unwrap().len(), 1);
        assert_eq!(and.as_array().unwrap().len(), 2, "no watermark clause");
    }

    #[test]
    fn follows_cursors_across_pages() {
        let page1 = json!({
            "issues": {
                "pageInfo": { "hasNextPage": true, "endCursor": "c1" },
                "nodes": [issue_json()]
            }
        });
        let mut second = issue_json();
        second["identifier"] = json!("LIN-143");
        second["id"] = json!("uuid-2");
        let l = Linear::new(FixtureTransport::new(vec![page1, one_page(json!([second]))]));
        let issues = l.fetch_board_issues(&["herd".into()], None, None).unwrap();
        assert_eq!(issues.len(), 2);
        // The second request carried the cursor.
        assert_eq!(l.transport.sent.borrow()[1]["variables"]["after"], json!("c1"));
    }

    #[test]
    fn state_types_not_names_decide_upstream_state() {
        for (ty, want) in [
            ("started", UpstreamState::Started),
            ("completed", UpstreamState::Terminal),
            ("canceled", UpstreamState::Terminal),
            ("triage", UpstreamState::Unstarted),
            ("backlog", UpstreamState::Unstarted),
        ] {
            let mut n = issue_json();
            n["state"] = json!({ "name": "Anything At All", "type": ty });
            let l = Linear::new(FixtureTransport::new(vec![one_page(json!([n]))]));
            let i = &l.fetch_board_issues(&[], None, None).unwrap()[0];
            assert_eq!(i.upstream(), want, "state type {ty}");
        }
    }

    #[test]
    fn started_state_is_chosen_by_type_and_position() {
        // And `In Review`, the other `started` state, is exactly why dispatch
        // landing on the lowest position is not enough on its own (AGE-21).
        let l = Linear::new(FixtureTransport::new(vec![states_page()]));
        assert_eq!(l.started_state_id("OFF").unwrap().as_deref(), Some("s-prog"));
    }

    fn states_page() -> Value {
        json!({
            "teams": { "nodes": [ { "id": "team-1", "states": { "nodes": [
                { "id": "s-done", "name": "Done",        "type": "completed", "position": 3.0 },
                { "id": "s-rev",  "name": "In Review",   "type": "started",   "position": 2.0 },
                { "id": "s-prog", "name": "In Progress", "type": "started",   "position": 1.0 },
                { "id": "s-todo", "name": "Todo",        "type": "unstarted", "position": 0.0 }
            ] } } ] }
        })
    }

    /// Linear has no review state *type* — `In Review` and `In Progress` are
    /// both `started` — so this one lookup goes by name, from config.
    #[test]
    fn a_review_state_is_found_by_name_because_there_is_no_review_type() {
        let l = Linear::new(FixtureTransport::new(vec![states_page(), states_page()]));
        assert_eq!(l.state_id_named("OFF", "In Review").unwrap(), Ok("s-rev".into()));
        // Case is not something an operator should have to get right.
        assert_eq!(l.state_id_named("OFF", "in review").unwrap(), Ok("s-rev".into()));
    }

    #[test]
    fn a_state_name_that_does_not_exist_comes_back_with_the_ones_that_do() {
        // So the operator is told what to write instead of just that they were
        // wrong — a renamed or non-English workflow is the likely case.
        let l = Linear::new(FixtureTransport::new(vec![states_page()]));
        let Err(have) = l.state_id_named("OFF", "Reviewing").unwrap() else {
            panic!("`Reviewing` must not resolve");
        };
        assert!(have.contains(&"In Review".to_string()), "{have:?}");
        assert_eq!(have.len(), 4);
    }

    #[test]
    fn teams_come_back_as_key_and_name() {
        let l = Linear::new(FixtureTransport::new(vec![json!({
            "teams": { "nodes": [
                { "key": "AGE", "name": "Agents" },
                { "key": "TAL", "name": "Tally" }
            ] }
        })]));
        assert_eq!(
            l.teams().unwrap(),
            vec![
                ("AGE".to_string(), "Agents".to_string()),
                ("TAL".to_string(), "Tally".to_string())
            ]
        );
    }

    #[test]
    fn graphql_errors_surface_as_errors() {
        struct Failing;
        impl GraphQl for Failing {
            fn query(&self, _: &Value) -> Result<Value> {
                bail!("linear GraphQL error: authentication required")
            }
        }
        let l = Linear::new(Failing);
        assert!(l.fetch_board_issues(&[], None, None).is_err());
    }

    #[test]
    fn mutations_require_reported_success() {
        let l = Linear::new(FixtureTransport::new(vec![
            json!({ "commentCreate": { "success": true } }),
            json!({ "commentCreate": { "success": false } }),
        ]));
        assert!(l.comment("uuid-1", "hello").is_ok());
        assert!(l.comment("uuid-1", "hello").is_err());
    }

    #[test]
    fn fetching_by_id_short_circuits_on_an_empty_list() {
        let l = Linear::new(FixtureTransport::new(vec![]));
        assert!(l.fetch_issues_by_id(&[]).unwrap().is_empty());
        assert!(l.transport.sent.borrow().is_empty());
    }
}
