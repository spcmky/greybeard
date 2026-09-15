use std::collections::BTreeSet;
use std::time::Instant;

use anyhow::{Context, Result};
use futures::future::join_all;
use serde_json::{json, Value};

use super::{Github, PrRef};
use crate::config::Config;

/// One changed file in the PR.
#[derive(Debug, Clone)]
pub struct ChangedFile {
    pub path: String,
    pub status: String,
    pub additions: u64,
    pub deletions: u64,
    /// New-side line ranges touched by the diff (from @@ hunks).
    pub changed_ranges: Vec<(u32, u32)>,
    /// Full contents at head SHA (capped), None for removed/binary/oversized.
    pub content: Option<String>,
}

/// Everything the lenses and verifiers get to see. Fetched once; rendered
/// byte-deterministically (it is the shared prompt-cache prefix).
///
/// NOTE on transport: everything metadata-shaped goes through ONE GraphQL
/// query (fewer round-trips, and the surface that stayed up during the
/// 2026-08-17 GitHub partial outage while REST sub-resources 404ed); only
/// the diff media type and the contents API use REST.
#[derive(Debug, Clone)]
pub struct ContextPack {
    pub pr: PrRef,
    /// GraphQL node ID of the PR (subjectId for addComment).
    pub pr_node_id: String,
    pub head_sha: String,
    pub state: String,
    pub draft: bool,
    pub title: String,
    pub author: String,
    pub author_is_bot: bool,
    pub changed_files: Vec<ChangedFile>,
    /// (comment node ID, reviewed sha) parsed from an existing Greybeard comment.
    pub existing_comment: Option<(String, String)>,
    pub rendered: String,
    pub fetch_ms: u128,
}

const PR_QUERY: &str = r#"
query($owner:String!,$repo:String!,$number:Int!){
  repository(owner:$owner,name:$repo){
    pullRequest(number:$number){
      id state isDraft title body baseRefName headRefOid
      author{login __typename}
      files(first:100){nodes{path additions deletions changeType}}
      comments(last:100){nodes{id body}}
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

    let existing_comment = p["comments"]["nodes"]
        .as_array()
        .and_then(|nodes| super::comment::find_marker_graphql(nodes));

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
    let diff_url = format!("/repos/{}/{}/pulls/{}", pr.owner, pr.repo, pr.number);
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

    let claude_md_paths = claude_md_candidates(&changed_files);
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

    // ── Step 3: deterministic render (sorted paths, fixed order, no clocks) ─
    let mut out = String::new();
    out.push_str("<pull_request>\n");
    out.push_str(&format!(
        "repo: {}/{}\nnumber: {}\ntitle: {}\nauthor: {}\nbase: {}\nhead_sha: {}\nstate: {}{}\n",
        pr.owner,
        pr.repo,
        pr.number,
        title,
        author,
        base_ref,
        head_sha,
        state,
        if draft { " (draft)" } else { "" }
    ));
    if !body.is_empty() {
        out.push_str(&format!("\n{body}\n"));
    }
    out.push_str("</pull_request>\n\n");

    out.push_str("<ci_status>\n");
    out.push_str(&render_checks(&p["commits"]["nodes"]));
    out.push_str("</ci_status>\n\n");

    out.push_str("<diff>\n");
    out.push_str(&budget_diff(&diff, cfg.max_diff_chars));
    out.push_str("\n</diff>\n\n");

    for (path, content) in claude_mds.into_iter().filter(|(_, c)| c.is_some()) {
        out.push_str(&format!(
            "<claude_md path=\"{path}\">\n{}\n</claude_md>\n\n",
            content.unwrap()
        ));
    }

    // Full file contents until the pack budget is spent. Which files make the
    // cut is risk-ranked — most-changed first, generated files last — while
    // render order stays path-sorted. Deterministic: depends only on sizes.
    let include = select_for_budget(&changed_files, cfg.max_pack_chars.saturating_sub(out.len()));
    let mut omitted: Vec<&str> = Vec::new();
    for f in &changed_files {
        if let Some(content) = &f.content {
            if !include.contains(f.path.as_str()) {
                omitted.push(&f.path);
                continue;
            }
            out.push_str(&format!(
                "<file path=\"{}\" status=\"{}\" additions=\"{}\" deletions=\"{}\">\n",
                f.path, f.status, f.additions, f.deletions
            ));
            for (i, line) in content.lines().enumerate() {
                out.push_str(&format!("{:>5}| {line}\n", i + 1));
            }
            out.push_str("</file>\n\n");
        }
    }
    if !omitted.is_empty() {
        out.push_str(&format!(
            "<pack_note>Full contents of {} changed file(s) were omitted to fit the review \
             context: {}. Their complete diffs appear above — judge them from the diff and \
             say so if a finding needs the surrounding file to confirm.</pack_note>\n\n",
            omitted.len(),
            omitted.join(", ")
        ));
    }

    let mut blame_sections: Vec<(String, String)> = blames
        .into_iter()
        .map(|(path, ranges)| {
            let file_ranges = changed_files
                .iter()
                .find(|f| f.path == path)
                .map(|f| f.changed_ranges.clone())
                .unwrap_or_default();
            (path, render_blame(&ranges, &file_ranges))
        })
        .filter(|(_, b)| !b.is_empty())
        .collect();
    blame_sections.sort_by(|a, b| a.0.cmp(&b.0));
    if !blame_sections.is_empty() {
        out.push_str("<blame note=\"history of the lines this PR touches\">\n");
        for (path, b) in blame_sections {
            out.push_str(&format!("## {path}\n{b}"));
        }
        out.push_str("</blame>\n\n");
    }

    let prior = prior_comments.unwrap_or_default();
    if !prior.is_empty() {
        out.push_str(
            "<prior_review_comments note=\"feedback on past PRs that touched these files\">\n",
        );
        out.push_str(&prior);
        out.push_str("</prior_review_comments>\n\n");
    }

    Ok(ContextPack {
        pr: pr.clone(),
        pr_node_id,
        head_sha,
        state,
        draft,
        title,
        author,
        author_is_bot,
        changed_files,
        existing_comment,
        rendered: out,
        fetch_ms: started.elapsed().as_millis(),
    })
}

/// Files whose diffs and contents are machine output — least reviewable,
/// first to drop when budgets bite.
pub fn is_generated(path: &str) -> bool {
    const LOCKFILES: [&str; 9] = [
        "Cargo.lock", "package-lock.json", "yarn.lock", "pnpm-lock.yaml", "uv.lock",
        "poetry.lock", "Gemfile.lock", "composer.lock", "go.sum",
    ];
    let name = path.rsplit('/').next().unwrap_or(path);
    LOCKFILES.contains(&name)
        || name.ends_with(".min.js")
        || name.ends_with(".min.css")
        || name.ends_with(".map")
        || name.ends_with(".snap")
        || name.ends_with("_pb2.py")
        || path.starts_with("dist/")
        || path.starts_with("build/")
        || path.starts_with("vendor/")
        || path.contains("/dist/")
        || path.contains("/vendor/")
        || path.contains("node_modules/")
}

/// Keep the diff inside `cap`: stub generated files' hunks first, then the
/// largest remaining per-file diffs. Original file order is preserved and the
/// result depends only on the input — deterministic for the cache prefix.
pub fn budget_diff(diff: &str, cap: usize) -> String {
    if diff.len() <= cap {
        return diff.to_string();
    }
    // Split into per-file segments on "diff --git" boundaries.
    let mut segments: Vec<(String, String)> = Vec::new(); // (path, segment)
    let mut current = String::new();
    let mut current_path = String::new();
    for line in diff.lines() {
        if line.starts_with("diff --git ") {
            if !current.is_empty() {
                segments.push((current_path.clone(), std::mem::take(&mut current)));
            }
            current_path = line.rsplit(" b/").next().unwrap_or("?").to_string();
        }
        current.push_str(line);
        current.push('\n');
    }
    if !current.is_empty() {
        segments.push((current_path, current));
    }

    let stub = |path: &str, seg: &str, why: &str| {
        format!(
            "diff --git (omitted) {path}\n[{why}: {} diff lines omitted — {path}]\n",
            seg.lines().count()
        )
    };
    let total = |segs: &[(String, String)]| segs.iter().map(|(_, s)| s.len()).sum::<usize>();

    // Pass 1: stub generated files.
    let mut segments: Vec<(String, String)> = segments
        .into_iter()
        .map(|(p, s)| {
            if is_generated(&p) {
                let st = stub(&p, &s, "generated file");
                (p, st)
            } else {
                (p, s)
            }
        })
        .collect();

    // Pass 2: stub the largest remaining segments until under budget.
    while total(&segments) > cap {
        let Some((idx, _)) = segments
            .iter()
            .enumerate()
            .filter(|(_, (_, s))| !s.starts_with("diff --git (omitted)"))
            .max_by_key(|(i, (_, s))| (s.len(), std::cmp::Reverse(*i)))
        else {
            break; // everything already stubbed
        };
        let (p, s) = segments[idx].clone();
        segments[idx] = (p.clone(), stub(&p, &s, "diff budget"));
    }
    segments.into_iter().map(|(_, s)| s).collect()
}

/// Parse unified diff into per-file new-side hunk ranges.
pub fn parse_diff_ranges(diff: &str) -> Vec<(String, Vec<(u32, u32)>)> {
    let mut out: Vec<(String, Vec<(u32, u32)>)> = Vec::new();
    let mut current: Option<usize> = None;
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("+++ b/") {
            out.push((rest.to_string(), Vec::new()));
            current = Some(out.len() - 1);
        } else if line.starts_with("+++ /dev/null") {
            current = None;
        } else if let (Some(idx), true) = (current, line.starts_with("@@")) {
            // @@ -a,b +c,d @@
            if let Some(plus) = line.split(' ').find(|s| s.starts_with('+')) {
                let nums = plus.trim_start_matches('+');
                let mut parts = nums.split(',');
                let start: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
                let count: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(1);
                if start > 0 {
                    out[idx].1.push((start, start + count.saturating_sub(1)));
                }
            }
        }
    }
    out
}

/// Pick which files get full contents under a char budget: not-generated
/// first, most-changed first, path as the deterministic tiebreak.
fn select_for_budget(files: &[ChangedFile], budget: usize) -> BTreeSet<&str> {
    let mut ranked: Vec<&ChangedFile> = files.iter().filter(|f| f.content.is_some()).collect();
    ranked.sort_by(|a, b| {
        is_generated(&a.path)
            .cmp(&is_generated(&b.path))
            .then((b.additions + b.deletions).cmp(&(a.additions + a.deletions)))
            .then(a.path.cmp(&b.path))
    });
    let mut remaining = budget;
    let mut include = BTreeSet::new();
    for f in ranked {
        let content = f.content.as_deref().unwrap_or("");
        // header + per-line "  123| " gutter overhead
        let est = content.len() + 8 * content.lines().count() + 120;
        if est <= remaining {
            remaining -= est;
            include.insert(f.path.as_str());
        }
    }
    include
}

/// CLAUDE.md at repo root + each changed file's directory chain.
fn claude_md_candidates(files: &[ChangedFile]) -> Vec<String> {
    let mut set = BTreeSet::new();
    set.insert("CLAUDE.md".to_string());
    for f in files {
        let mut dir = std::path::Path::new(&f.path).parent();
        while let Some(d) = dir {
            if !d.as_os_str().is_empty() {
                set.insert(format!("{}/CLAUDE.md", d.to_string_lossy()));
            }
            dir = d.parent();
        }
    }
    set.into_iter().take(15).collect()
}

/// Raw blame ranges: (start, end, oid, date, author, headline).
type BlameRange = (u32, u32, String, String, String, String);

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

fn render_blame(ranges: &[BlameRange], changed: &[(u32, u32)]) -> String {
    let mut out = String::new();
    for (s, e, oid, date, author, headline) in ranges {
        let overlaps = changed.iter().any(|(cs, ce)| *s <= *ce && *e >= *cs);
        if overlaps {
            out.push_str(&format!("L{s}-L{e} {oid} {date} ({author}): {headline}\n"));
        }
    }
    out
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

    let wanted: BTreeSet<&str> = paths.iter().map(|s| s.as_str()).collect();
    // (pr_number, path, author, body) — BTreeSet dedupes and gives stable order.
    let mut rows: BTreeSet<(u64, String, String, String)> = BTreeSet::new();
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

fn render_checks(commit_nodes: &Value) -> String {
    let rollup = &commit_nodes[0]["commit"]["statusCheckRollup"];
    if rollup.is_null() {
        return "no check runs reported\n".to_string();
    }
    let mut out = format!("overall: {}\n", rollup["state"].as_str().unwrap_or("?"));
    let mut rows: Vec<String> = rollup["contexts"]["nodes"]
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
    rows.sort();
    out.push_str(&rows.concat());
    out
}

pub fn cap_lines(s: &str, max: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    if lines.len() <= max {
        s.to_string()
    } else {
        format!(
            "{}\n… [truncated: {} of {} lines shown]",
            lines[..max].join("\n"),
            max,
            lines.len()
        )
    }
}
