use anyhow::Result;
use serde_json::{json, Value};

use super::{Github, PrRef};
use crate::pack::ExistingComment;

const MARKER_PREFIX: &str = "<!-- greybeard:";

/// Scan a page of GraphQL comment nodes for greybeard's own existing comment.
///
/// SECURITY: only a comment the authenticated actor authored (`viewerDidAuthor`)
/// is trusted. Without this check anyone who can comment on the PR could forge a
/// marker — supplying the current head sha to suppress the review, or pointing
/// the update at a comment they control. `viewerDidAuthor` is used rather than
/// matching our own login because a GitHub App installation token cannot always
/// resolve `viewer{login}` (see `Github::whoami`).
pub fn find_marker_in_nodes(nodes: &[Value]) -> Option<ExistingComment> {
    for c in nodes {
        if c["viewerDidAuthor"].as_bool() != Some(true) {
            continue;
        }
        let body = c["body"].as_str().unwrap_or("");
        if let Some((sha, verdict)) = parse_marker_full(body) {
            return Some(ExistingComment {
                id: c["id"].as_str()?.to_string(),
                sha,
                complete: verdict.as_deref() != Some("degraded"),
            });
        }
    }
    None
}

/// Extract the reviewed SHA from a marker like `<!-- greybeard:{"v":1,"sha":"abc"} -->`.
pub fn parse_marker(body: &str) -> Option<String> {
    parse_marker_full(body).map(|(sha, _)| sha)
}

/// Extract `(reviewed sha, verdict)` from a marker. Verdict is None for a v1
/// marker (which carried no verdict field) — callers treat that as a completed
/// review.
pub fn parse_marker_full(body: &str) -> Option<(String, Option<String>)> {
    let start = body.find(MARKER_PREFIX)?;
    let rest = &body[start + MARKER_PREFIX.len()..];
    let end = rest.find("-->")?;
    let payload: Value = serde_json::from_str(rest[..end].trim()).ok()?;
    let sha = payload["sha"].as_str()?.to_string();
    let verdict = payload["verdict"].as_str().map(|s| s.to_string());
    Some((sha, verdict))
}

pub fn render_marker(sha: &str) -> String {
    format!("{MARKER_PREFIX}{} -->", json!({"v": 1, "sha": sha}))
}

/// v2 marker: machine-readable findings for the fix-loop skill. Same hidden
/// HTML comment channel as v1 — no new endpoint or auth surface; any client
/// that can read the PR comment can consume the loop contract.
pub fn render_marker_v2(sha: &str, verdict: &str, findings: Vec<Value>) -> String {
    // Findings carry model-authored text; a literal `-->` in any string would
    // terminate the HTML comment early and break parse_marker. `<`/`>` only
    // occur inside JSON string values, so escaping them as \u003c and \u003e keeps
    // the payload valid JSON (decoded transparently by any parser) while
    // guaranteeing the marker's only `-->` is its own terminator.
    let payload = json!({"v": 2, "sha": sha, "verdict": verdict, "findings": findings})
        .to_string()
        .replace('<', "\\u003c")
        .replace('>', "\\u003e");
    format!("{MARKER_PREFIX}{payload} -->")
}

/// GitHub permalink with full SHA and >=1 line of context either side. Thin
/// wrapper over the forge-neutral [`crate::pack::permalink`] pinned to GitHub.
pub fn permalink(pr: &PrRef, sha: &str, path: &str, line: Option<u32>) -> String {
    crate::pack::permalink(crate::config::Forge::GitHub, None, pr, sha, path, line)
}

/// Create the Greybeard comment, or update the existing one in place — via
/// GraphQL mutations (the REST comment endpoints are proxy-blocked here).
/// Never stacks a second comment.
pub async fn upsert(
    gh: &Github,
    pr_node_id: &str,
    existing_comment_id: Option<&str>,
    body: &str,
) -> Result<String> {
    match existing_comment_id {
        Some(id) => {
            const UPDATE: &str = r#"
mutation($id:ID!,$body:String!){
  updateIssueComment(input:{id:$id,body:$body}){ issueComment{ url } }
}"#;
            let v = gh.graphql(UPDATE, json!({"id": id, "body": body})).await?;
            Ok(v["updateIssueComment"]["issueComment"]["url"]
                .as_str()
                .unwrap_or("?")
                .to_string())
        }
        None => {
            const ADD: &str = r#"
mutation($subjectId:ID!,$body:String!){
  addComment(input:{subjectId:$subjectId,body:$body}){ commentEdge{ node{ url } } }
}"#;
            let v = gh
                .graphql(ADD, json!({"subjectId": pr_node_id, "body": body}))
                .await?;
            Ok(v["addComment"]["commentEdge"]["node"]["url"]
                .as_str()
                .unwrap_or("?")
                .to_string())
        }
    }
}
