use anyhow::Result;
use serde_json::{json, Value};

use super::{Github, PrRef};

const MARKER_PREFIX: &str = "<!-- greybeard:";

/// Scan GraphQL comment nodes for an existing Greybeard comment;
/// returns (comment node ID, reviewed sha).
pub fn find_marker_graphql(nodes: &[Value]) -> Option<(String, String)> {
    for c in nodes {
        let body = c["body"].as_str().unwrap_or("");
        if let Some(sha) = parse_marker(body) {
            return Some((c["id"].as_str()?.to_string(), sha));
        }
    }
    None
}

/// Extract the reviewed SHA from a marker like `<!-- greybeard:{"v":1,"sha":"abc"} -->`.
pub fn parse_marker(body: &str) -> Option<String> {
    let start = body.find(MARKER_PREFIX)?;
    let rest = &body[start + MARKER_PREFIX.len()..];
    let end = rest.find("-->")?;
    let payload: Value = serde_json::from_str(rest[..end].trim()).ok()?;
    payload["sha"].as_str().map(|s| s.to_string())
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

/// GitHub permalink with full SHA and >=1 line of context either side.
pub fn permalink(pr: &PrRef, sha: &str, path: &str, line: Option<u32>) -> String {
    match line {
        Some(l) => {
            let start = l.saturating_sub(1).max(1);
            let end = l + 1;
            format!(
                "https://github.com/{}/{}/blob/{}/{}#L{}-L{}",
                pr.owner, pr.repo, sha, path, start, end
            )
        }
        None => format!("https://github.com/{}/{}/blob/{}/{}", pr.owner, pr.repo, sha, path),
    }
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
