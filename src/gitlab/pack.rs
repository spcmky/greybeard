//! GitLab context-pack fetcher: pulls MR metadata, diffs, file contents, and CI
//! status from REST v4 and hands them to the shared renderer in [`crate::pack`].
//! Blame and prior-review-comment context are not fetched yet (v1) — the
//! sections simply stay empty; see docs/GITLAB.md.

use std::time::Instant;

use anyhow::{Context, Result};
use futures::future::join_all;

use super::Gitlab;
use crate::config::Config;
use crate::github::PrRef;
use crate::pack::{cap_lines, claude_md_candidates, parse_diff_ranges, ChangedFile, CheckRollup, ContextPack, PackData};

/// Fetch and render the context pack from GitLab.
pub async fn build(gl: &Gitlab, pr: &PrRef, cfg: &Config) -> Result<ContextPack> {
    let started = Instant::now();
    let pid = Gitlab::project_id(pr);
    let iid = pr.number;

    // ── MR metadata ────────────────────────────────────────────────────────
    let mr = gl
        .get_json(&format!("/projects/{pid}/merge_requests/{iid}"))
        .await
        .context("MR metadata")?;
    let title = mr["title"].as_str().unwrap_or("").to_string();
    let body = mr["description"].as_str().unwrap_or("").to_string();
    let author = mr["author"]["username"].as_str().unwrap_or("?").to_string();
    let author_is_bot = super::looks_like_bot(&author);
    let draft = mr["draft"].as_bool().unwrap_or(false);
    let base_ref = mr["target_branch"].as_str().unwrap_or("?").to_string();
    // Prefer the diff_refs head sha (the sha the diffs are against); fall back
    // to the MR `sha`.
    let head_sha = mr["diff_refs"]["head_sha"]
        .as_str()
        .or_else(|| mr["sha"].as_str())
        .unwrap_or_default()
        .to_string();
    // Normalize GitLab's "opened" to the pipeline's "open"; pass others through
    // (closed / merged / locked) so eligibility skips them.
    let state = match mr["state"].as_str().unwrap_or("?") {
        "opened" => "open".to_string(),
        other => other.to_string(),
    };

    // ── Existing greybeard note (marker) ───────────────────────────────────
    let existing_comment = gl.find_marker_note(pr).await;

    // ── Diffs (paginated) → changed files + a GitHub-style unified diff ─────
    let diff_items = gl
        .get_paginated(&format!("/projects/{pid}/merge_requests/{iid}/diffs"))
        .await
        .context("MR diffs")?;
    let (mut changed_files, diff) = assemble_diff(&diff_items);
    changed_files.sort_by(|a, b| a.path.cmp(&b.path));

    // ── Parallel: file contents, CLAUDE.md files, CI status ────────────────
    let content_paths: Vec<String> = changed_files
        .iter()
        .filter(|f| f.status != "deleted")
        .map(|f| f.path.clone())
        .take(cfg.max_pack_files)
        .collect();
    let contents_fut = join_all(content_paths.iter().map(|p| {
        let p = p.clone();
        let sha = head_sha.clone();
        async move { (p.clone(), gl.file_contents(pr, &p, &sha).await.ok().flatten()) }
    }));

    let claude_paths = claude_md_candidates(&changed_files);
    let claude_fut = join_all(claude_paths.iter().map(|p| {
        let p = p.clone();
        let sha = head_sha.clone();
        async move { (p.clone(), gl.file_contents(pr, &p, &sha).await.ok().flatten()) }
    }));

    let checks_fut = build_checks(gl, &pid, iid, &head_sha);

    let (contents, claude_mds, checks) = tokio::join!(contents_fut, claude_fut, checks_fut);

    // Attach hunk ranges from the assembled diff, then file contents.
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

    let data = PackData {
        pr: pr.clone(),
        // GitLab posts notes by MR iid, not an opaque node id — nothing to
        // stash here; upsert_comment addresses the MR directly.
        pr_node_id: String::new(),
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
        checks,
        claude_mds,
        // Blame + prior-review context are not fetched yet (v1).
        blames: Vec::new(),
        prior_comments: String::new(),
    };
    Ok(data.finish(cfg, started))
}

/// Turn GitLab's per-file diff items into `ChangedFile`s plus one GitHub-style
/// unified diff string (with `diff --git` / `---` / `+++` headers) so the
/// shared `parse_diff_ranges` and `budget_diff` work unchanged.
fn assemble_diff(items: &[serde_json::Value]) -> (Vec<ChangedFile>, String) {
    let mut files = Vec::new();
    let mut out = String::new();
    for it in items {
        let old_path = it["old_path"].as_str().unwrap_or("");
        let new_path = it["new_path"].as_str().unwrap_or(old_path);
        let deleted = it["deleted_file"].as_bool().unwrap_or(false);
        let new_file = it["new_file"].as_bool().unwrap_or(false);
        let renamed = it["renamed_file"].as_bool().unwrap_or(false);
        let status = if new_file {
            "added"
        } else if deleted {
            "deleted"
        } else if renamed {
            "renamed"
        } else {
            "modified"
        };
        let hunks = it["diff"].as_str().unwrap_or("");
        // +/- counts from the hunk body (header lines use +++/---, absent here).
        let additions = hunks
            .lines()
            .filter(|l| l.starts_with('+') && !l.starts_with("+++"))
            .count() as u64;
        let deletions = hunks
            .lines()
            .filter(|l| l.starts_with('-') && !l.starts_with("---"))
            .count() as u64;

        out.push_str(&format!("diff --git a/{old_path} b/{new_path}\n"));
        if deleted {
            out.push_str(&format!("--- a/{old_path}\n+++ /dev/null\n"));
        } else {
            out.push_str(&format!("--- a/{old_path}\n+++ b/{new_path}\n"));
        }
        out.push_str(hunks);
        if !hunks.ends_with('\n') {
            out.push('\n');
        }

        files.push(ChangedFile {
            path: new_path.to_string(),
            status: status.to_string(),
            additions,
            deletions,
            changed_ranges: Vec::new(),
            content: None,
        });
    }
    (files, out)
}

/// Build the neutral CI rollup from the MR's head pipeline and its jobs.
/// GitLab pipelines/jobs replace GitHub's check-run rollup; the renderer frames
/// the rows identically.
async fn build_checks(gl: &Gitlab, pid: &str, iid: u64, head_sha: &str) -> CheckRollup {
    let pipelines = match gl
        .get_json(&format!("/projects/{pid}/merge_requests/{iid}/pipelines"))
        .await
    {
        Ok(v) => v,
        Err(_) => return CheckRollup::default(),
    };
    let arr = pipelines.as_array().cloned().unwrap_or_default();
    // Prefer the pipeline for the reviewed head sha; else the newest listed.
    let head = arr
        .iter()
        .find(|p| p["sha"].as_str() == Some(head_sha))
        .or_else(|| arr.first());
    let Some(p) = head else {
        return CheckRollup::default();
    };
    let overall = p["status"].as_str().unwrap_or("?").to_string();
    let mut rows = Vec::new();
    if let Some(pipe_id) = p["id"].as_u64() {
        if let Ok(jobs) = gl
            .get_paginated(&format!("/projects/{pid}/pipelines/{pipe_id}/jobs"))
            .await
        {
            rows = jobs
                .iter()
                .map(|j| {
                    format!(
                        "{}: {}\n",
                        j["name"].as_str().unwrap_or("?"),
                        j["status"].as_str().unwrap_or("?")
                    )
                })
                .collect();
        }
    }
    CheckRollup { overall: Some(overall), rows }
}
