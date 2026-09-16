//! GitHub context-pack *fetcher*: pulls the raw inputs from GitHub's GraphQL +
//! REST APIs and hands them to the shared, forge-neutral renderer in
//! [`crate::pack`]. The deterministic rendering and all the pure budgeting
//! helpers live there; this file is the GitHub-specific half of the phase-2
//! split (docs/GITLAB.md).

use std::time::Instant;

use anyhow::{Context, Result};
use futures::future::join_all;
use serde_json::{json, Value};

use super::{Github, PrRef};
use crate::config::Config;
use crate::pack::{BlameRange, ChangedFile, CheckRollup, PackData};

// Back-compat: the neutral renderer/types moved to `crate::pack`, but the old
// `github::pack::*` paths stay valid so nothing downstream (or in the test
// suite) has to chase the move. These `pub use`s double as this file's own
// imports for `cap_lines` / `parse_diff_ranges`.
pub use crate::pack::{budget_diff, cap_lines, is_generated, parse_diff_ranges, ContextPack};

const PR_QUERY: &str = r#"
query($owner:String!,$repo:String!,$number:Int!){
  repository(owner:$owner,name:$repo){
    pullRequest(number:$number){
      id state isDraft title body baseRefName baseRefOid headRefOid
      author{login __typename}
      files(first:100){nodes{path additions deletions changeType}}
      comments(first:100){nodes{id body viewerDidAuthor} pageInfo{hasNextPage endCursor}}
      commits(last:1){nodes{commit{statusCheckRollup{
        state
        contexts(first:50){nodes{
          __typename
          ... on CheckRun{name conclusion status}
          ... on StatusContext{context state}
        }}
      }}}}
    }
  }
}"#;

/// REST path for the PR diff pinned to a specific head revision. The compare
/// endpoint (`base...head`) with the reviewed head SHA keeps the diff consistent
/// with the file contents and blame, which are fetched at the same SHA — a push
/// mid-build can no longer mix a newer diff with older file contents.
pub fn pr_diff_path(pr: &PrRef, base: &str, head_sha: &str) -> String {
    format!("/repos/{}/{}/compare/{base}...{head_sha}", pr.owner, pr.repo)
}

/// Fetch and render the context pack from GitHub.
pub async fn build(gh: &Github, pr: &PrRef, cfg: &Config) -> Result<ContextPack> {
    let started = Instant::now();

    // ── Step 1: one GraphQL query — meta, files, comments, CI rollup ────────
    let data = gh
        .graphql(
            PR_QUERY,
            json!({"owner": pr.owner, "repo": pr.repo, "number": pr.number}),
        )
        .await
        .context("PR GraphQL query")?;
    let p = &data["repository"]["pullRequest"];
    if p.is_null() {
        anyhow::bail!("PR #{} not found in {}/{}", pr.number, pr.owner, pr.repo);
    }
    let pr_node_id = p["id"].as_str().unwrap_or_default().to_string();
    let head_sha = p["headRefOid"].as_str().unwrap_or_default().to_string();
    let state = p["state"].as_str().unwrap_or("?").to_lowercase();
    let draft = p["isDraft"].as_bool().unwrap_or(false);
    let title = p["title"].as_str().unwrap_or("").to_string();
    let body = p["body"].as_str().unwrap_or("").to_string();
    let author = p["author"]["login"].as_str().unwrap_or("?").to_string();
    let author_is_bot = p["author"]["__typename"] == "Bot";
    let base_ref = p["baseRefName"].as_str().unwrap_or("?").to_string();

    let existing_comment = find_existing_comment(gh, pr, &p["comments"]).await?;

    let mut changed_files: Vec<ChangedFile> = p["files"]["nodes"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|f| ChangedFile {
            path: f["path"].as_str().unwrap_or("").to_string(),
            status: f["changeType"].as_str().unwrap_or("?").to_lowercase(),
            additions: f["additions"].as_u64().unwrap_or(0),
            deletions: f["deletions"].as_u64().unwrap_or(0),
            changed_ranges: Vec::new(),
            content: None,
        })
        .collect();
    changed_files.sort_by(|a, b| a.path.cmp(&b.path));

    // ── Step 2: parallel — diff (REST media), file contents (REST contents),
    //    CLAUDE.md files, blame (GraphQL), prior-PR feedback (GraphQL) ───────
    // Pin the diff to the captured head sha (compare endpoint) rather than the
    // mutable pulls/{n} diff: file contents and blame below are fetched at
    // head_sha, so an unpinned diff could mix a newer push's changes with older
    // file contents and citations.
    let base_sha = p["baseRefOid"].as_str().unwrap_or_default();
    let diff_base = if base_sha.is_empty() { base_ref.as_str() } else { base_sha };
    let diff_url = pr_diff_path(pr, diff_base, &head_sha);
    let diff_fut = gh.get_raw(&diff_url, "application/vnd.github.diff");

    let content_paths: Vec<String> = changed_files
        .iter()
        .filter(|f| f.status != "deleted")
        .map(|f| f.path.clone())
        .take(cfg.max_pack_files)
        .collect();
    let contents_fut = join_all(content_paths.iter().map(|p| {
        let p = p.clone();
        let sha = head_sha.clone();
        async move { (p.clone(), gh.file_contents(pr, &p, &sha).await.ok().flatten()) }
    }));

    let claude_md_paths = crate::pack::claude_md_candidates(&changed_files);
    let claude_fut = join_all(claude_md_paths.iter().map(|p| {
        let p = p.clone();
        let sha = head_sha.clone();
        async move { (p.clone(), gh.file_contents(pr, &p, &sha).await.ok().flatten()) }
    }));

    let blame_paths: Vec<String> = changed_files
        .iter()
        .filter(|f| f.status == "modified")
        .map(|f| f.path.clone())
        .take(20)
        .collect();
    let blame_fut = join_all(blame_paths.iter().map(|path| {
        let path = path.clone();
        let sha = head_sha.clone();
        async move { (path.clone(), fetch_blame_ranges(gh, pr, &sha, &path).await.unwrap_or_default()) }
    }));

    let prior_paths: Vec<String> = changed_files.iter().map(|f| f.path.clone()).take(6).collect();
    let prior_fut = fetch_prior_comments(gh, pr, &head_sha, &prior_paths);

    let (diff, contents, claude_mds, blames, prior_comments) =
        tokio::join!(diff_fut, contents_fut, claude_fut, blame_fut, prior_fut);
    let diff = diff.context("PR diff")?;

    // Hunk ranges come from the diff; attach to files, then filter blame with them.
    for (path, ranges) in parse_diff_ranges(&diff) {
        if let Some(f) = changed_files.iter_mut().find(|f| f.path == path) {
            f.changed_ranges = ranges;
        }
    }
    for (path, content) in contents {
        if let Some(f) = changed_files.iter_mut().find(|f| f.path == path) {
            f.content = content.map(|c| cap_lines(&c, cfg.max_file_lines));
        }
    }

    // ── Step 3: assemble neutral inputs → shared deterministic render ───────
    let data = PackData {
        pr: pr.clone(),
        pr_node_id,
        head_sha,
        state,
        draft,
        title,
        body,
        base_ref,
        author,
        author_is_bot,
        existing_comment,
        changed_files,
        diff,
        checks: checks_from_graphql(&p["commits"]["nodes"]),
        claude_mds,
        blames,
        prior_comments: prior_comments.unwrap_or_default(),
    };
    Ok(data.finish(cfg, started))
}

const COMMENTS_PAGE_QUERY: &str = r#"
query($owner:String!,$repo:String!,$number:Int!,$after:String!){
  repository(owner:$owner,name:$repo){
    pullRequest(number:$number){
      comments(first:100,after:$after){nodes{id body viewerDidAuthor} pageInfo{hasNextPage endCursor}}
    }
  }
}"#;

/// Find greybeard's own existing comment, following comment pagination until the
/// marker is found or the comments are exhausted. Greybeard comments early and
/// updates in place, so it is almost always on the first page (`first:100`);
/// only a PR with >100 comments ahead of ours costs extra round-trips. Without
/// this, an existing review outside the first page is missed and a duplicate
/// comment gets posted.
async fn find_existing_comment(
    gh: &Github,
    pr: &PrRef,
    first_page: &Value,
) -> Result<Option<crate::pack::ExistingComment>> {
    let mut connection = std::borrow::Cow::Borrowed(first_page);
    let mut guard = 0;
    loop {
        if let Some(nodes) = connection["nodes"].as_array() {
            if let Some(ec) = super::comment::find_marker_in_nodes(nodes) {
                return Ok(Some(ec));
            }
        }
        if connection["pageInfo"]["hasNextPage"].as_bool() != Some(true) {
            return Ok(None);
        }
        let after = connection["pageInfo"]["endCursor"].as_str().unwrap_or_default().to_string();
        guard += 1;
        if guard > 50 {
            // ~5000 comments scanned — stop rather than page forever.
            return Ok(None);
        }
        let data = gh
            .graphql(
                COMMENTS_PAGE_QUERY,
                json!({"owner": pr.owner, "repo": pr.repo, "number": pr.number, "after": after}),
            )
            .await
            .context("PR comments pagination")?;
        connection = std::borrow::Cow::Owned(data["repository"]["pullRequest"]["comments"].clone());
    }
}

/// Convert GitHub's `statusCheckRollup` into the neutral [`CheckRollup`].
/// Owns GitHub's per-check line format; the renderer sorts + frames the rows.
fn checks_from_graphql(commit_nodes: &Value) -> CheckRollup {
    let rollup = &commit_nodes[0]["commit"]["statusCheckRollup"];
    if rollup.is_null() {
        return CheckRollup::default();
    }
    let rows = rollup["contexts"]["nodes"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|c| {
            if c["__typename"] == "CheckRun" {
                format!(
                    "{}: {} ({})\n",
                    c["name"].as_str().unwrap_or("?"),
                    c["conclusion"].as_str().unwrap_or("pending"),
                    c["status"].as_str().unwrap_or("?")
                )
            } else {
                format!(
                    "{}: {}\n",
                    c["context"].as_str().unwrap_or("?"),
                    c["state"].as_str().unwrap_or("?")
                )
            }
        })
        .collect();
    CheckRollup {
        overall: Some(rollup["state"].as_str().unwrap_or("?").to_string()),
        rows,
    }
}

async fn fetch_blame_ranges(
    gh: &Github,
    pr: &PrRef,
    sha: &str,
    path: &str,
) -> Result<Vec<BlameRange>> {
    const QUERY: &str = r#"
query($owner:String!,$repo:String!,$sha:GitObjectID!,$path:String!){
  repository(owner:$owner,name:$repo){
    object(oid:$sha){
      ... on Commit {
        blame(path:$path){
          ranges{ startingLine endingLine commit{ abbreviatedOid messageHeadline committedDate author{ name } } }
        }
      }
    }
  }
}"#;
    let data = gh
        .graphql(
            QUERY,
            json!({"owner": pr.owner, "repo": pr.repo, "sha": sha, "path": path}),
        )
        .await?;
    Ok(data["repository"]["object"]["blame"]["ranges"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|r| {
            (
                r["startingLine"].as_u64().unwrap_or(0) as u32,
                r["endingLine"].as_u64().unwrap_or(0) as u32,
                r["commit"]["abbreviatedOid"].as_str().unwrap_or("?").to_string(),
                r["commit"]["committedDate"].as_str().unwrap_or("?").to_string(),
                r["commit"]["author"]["name"].as_str().unwrap_or("?").to_string(),
                r["commit"]["messageHeadline"].as_str().unwrap_or("").to_string(),
            )
        })
        .collect())
}

/// Review comments from recent merged PRs that touched the same files —
/// commit history by path → associated PRs → their review threads (all GraphQL).
async fn fetch_prior_comments(
    gh: &Github,
    pr: &PrRef,
    sha: &str,
    paths: &[String],
) -> Result<String> {
    const QUERY: &str = r#"
query($owner:String!,$repo:String!,$sha:GitObjectID!,$path:String!){
  repository(owner:$owner,name:$repo){
    object(oid:$sha){
      ... on Commit {
        history(first:3,path:$path){
          nodes{
            associatedPullRequests(first:2){
              nodes{
                number
                reviewThreads(first:10){
                  nodes{ comments(first:2){ nodes{ path body author{login} } } }
                }
              }
            }
          }
        }
      }
    }
  }
}"#;
    let results = join_all(paths.iter().map(|path| {
        let vars = json!({"owner": pr.owner, "repo": pr.repo, "sha": sha, "path": path});
        async move { gh.graphql(QUERY, vars).await.ok() }
    }))
    .await;

    let wanted: std::collections::BTreeSet<&str> = paths.iter().map(|s| s.as_str()).collect();
    // (pr_number, path, author, body) — BTreeSet dedupes and gives stable order.
    let mut rows: std::collections::BTreeSet<(u64, String, String, String)> =
        std::collections::BTreeSet::new();
    for data in results.into_iter().flatten() {
        let commits = data["repository"]["object"]["history"]["nodes"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        for c in commits {
            for assoc in c["associatedPullRequests"]["nodes"].as_array().cloned().unwrap_or_default() {
                let n = assoc["number"].as_u64().unwrap_or(0);
                if n == 0 || n == pr.number {
                    continue;
                }
                for t in assoc["reviewThreads"]["nodes"].as_array().cloned().unwrap_or_default() {
                    for cm in t["comments"]["nodes"].as_array().cloned().unwrap_or_default() {
                        let path = cm["path"].as_str().unwrap_or("");
                        let body = cm["body"].as_str().unwrap_or("").trim();
                        if wanted.contains(path) && !body.is_empty() {
                            rows.insert((
                                n,
                                path.to_string(),
                                cm["author"]["login"].as_str().unwrap_or("?").to_string(),
                                cap_lines(body, 6),
                            ));
                        }
                    }
                }
            }
        }
    }
    Ok(rows
        .into_iter()
        .take(40)
        .map(|(n, path, author, body)| format!("PR #{n} {path} ({author}): {body}\n"))
        .collect())
}
