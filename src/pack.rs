//! Forge-neutral context pack: the shared, deterministic renderer plus the
//! data types every backend feeds it.
//!
//! The split (docs/GITLAB.md, phase 2): each forge has a *fetcher* that pulls
//! the raw inputs its API exposes and populates [`PackData`]; [`render`] turns
//! that into the byte-deterministic string that is the shared prompt-cache
//! prefix. The fetcher is backend-specific (GitHub GraphQL today, GitLab REST
//! next); everything in this module is backend-agnostic, so a new backend
//! reuses the renderer verbatim.

use std::collections::BTreeSet;
use std::time::Instant;

use crate::config::{Config, Forge};
use crate::github::PrRef;

/// Web permalink to a file (optionally a line window) at a sha, in the shape
/// the forge's blob viewer expects. GitHub: `<host>/<project>/blob/<sha>/<path>`
/// with an `#L10-L11` anchor; GitLab: the `/-/blob/` prefix and an `#L10-11`
/// anchor (no second `L`). `base_url` is the forge web root
/// (`GREYBEARD_FORGE_URL`) for self-managed GitLab; GitHub always links to
/// public github.com today (GitHub Enterprise web links are a separate concern).
pub fn permalink(
    forge: Forge,
    base_url: Option<&str>,
    pr: &PrRef,
    sha: &str,
    path: &str,
    line: Option<u32>,
) -> String {
    let project = pr.project();
    let trim = |b: &str| b.trim().trim_end_matches('/').to_string();
    match forge {
        Forge::GitHub => match line {
            Some(l) => {
                let start = l.saturating_sub(1).max(1);
                let end = l + 1;
                format!("https://github.com/{project}/blob/{sha}/{path}#L{start}-L{end}")
            }
            None => format!("https://github.com/{project}/blob/{sha}/{path}"),
        },
        Forge::GitLab => {
            let base = base_url
                .map(trim)
                .filter(|h| !h.is_empty())
                .unwrap_or_else(|| "https://gitlab.com".to_string());
            match line {
                Some(l) => {
                    let start = l.saturating_sub(1).max(1);
                    let end = l + 1;
                    format!("{base}/{project}/-/blob/{sha}/{path}#L{start}-{end}")
                }
                None => format!("{base}/{project}/-/blob/{sha}/{path}"),
            }
        }
    }
}

/// The Greybeard comment already present on a change, if any: the id needed to
/// update it in place, the sha it last reviewed, and whether that review
/// completed. An incomplete (degraded) review must be retried, not skipped as
/// "already reviewed".
#[derive(Debug, Clone)]
pub struct ExistingComment {
    pub id: String,
    pub sha: String,
    /// False when the recorded marker verdict was "degraded" — coverage or
    /// verification was incomplete, so a fresh run on the same head is wanted.
    pub complete: bool,
}

impl ExistingComment {
    /// True when this comment already holds a *completed* review of `head_sha`,
    /// i.e. re-reviewing would be redundant. A degraded review of the same head
    /// returns false so it gets another pass.
    pub fn already_covers(&self, head_sha: &str) -> bool {
        self.sha == head_sha && self.complete
    }
}

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

/// Raw blame ranges: (start, end, oid, date, author, headline).
pub type BlameRange = (u32, u32, String, String, String, String);

/// Neutral CI rollup: an overall verdict (None when the forge reported no
/// checks) plus already-formatted per-check rows. The forge fetcher owns the
/// per-check line format (GitHub check runs vs status contexts, GitLab jobs);
/// the renderer only sorts and frames them, so the section stays identical
/// across backends.
#[derive(Debug, Clone, Default)]
pub struct CheckRollup {
    pub overall: Option<String>,
    pub rows: Vec<String>,
}

/// Everything a forge fetcher gathers for one change, before rendering. Holds
/// both the renderer's inputs and the metadata the pipeline reads off the
/// finished [`ContextPack`].
#[derive(Debug, Clone)]
pub struct PackData {
    pub pr: PrRef,
    /// Opaque node/subject id the backend needs to post the comment
    /// (GitHub GraphQL node id; GitLab will stash its own handle here).
    pub pr_node_id: String,
    pub head_sha: String,
    pub state: String,
    pub draft: bool,
    pub title: String,
    pub body: String,
    pub base_ref: String,
    pub author: String,
    pub author_is_bot: bool,
    /// The existing Greybeard comment (id + reviewed sha + completion), if one
    /// authored by us is already on the change.
    pub existing_comment: Option<ExistingComment>,
    /// Changed files with `content` and `changed_ranges` already populated.
    pub changed_files: Vec<ChangedFile>,
    pub diff: String,
    pub checks: CheckRollup,
    /// (path, contents) for each candidate CLAUDE.md; None when absent.
    pub claude_mds: Vec<(String, Option<String>)>,
    /// (path, blame ranges) for the modified files.
    pub blames: Vec<(String, Vec<BlameRange>)>,
    /// Pre-rendered prior-review-comment rows (backend-specific labels).
    pub prior_comments: String,
}

impl PackData {
    /// Render and package into the finished pack, stamping the fetch duration.
    pub fn finish(self, cfg: &Config, started: Instant) -> ContextPack {
        let rendered = render(&self, cfg);
        ContextPack {
            pr: self.pr,
            pr_node_id: self.pr_node_id,
            head_sha: self.head_sha,
            state: self.state,
            draft: self.draft,
            title: self.title,
            author: self.author,
            author_is_bot: self.author_is_bot,
            changed_files: self.changed_files,
            existing_comment: self.existing_comment,
            rendered,
            fetch_ms: started.elapsed().as_millis(),
        }
    }
}

/// Everything the lenses and verifiers get to see. Fetched once; rendered
/// byte-deterministically (it is the shared prompt-cache prefix).
///
/// NOTE on transport: on GitHub everything metadata-shaped goes through ONE
/// GraphQL query (fewer round-trips, and the surface that stayed up during the
/// 2026-08-17 GitHub partial outage while REST sub-resources 404ed); only the
/// diff media type and the contents API use REST.
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
    /// The existing Greybeard comment (id + reviewed sha + completion), if one
    /// authored by us is already on the change.
    pub existing_comment: Option<ExistingComment>,
    pub rendered: String,
    pub fetch_ms: u128,
}

/// Deterministic render (sorted paths, fixed order, no clocks) of the fetched
/// inputs into the context-pack string.
pub fn render(d: &PackData, cfg: &Config) -> String {
    let mut out = String::new();
    out.push_str("<pull_request>\n");
    out.push_str(&format!(
        "repo: {}/{}\nnumber: {}\ntitle: {}\nauthor: {}\nbase: {}\nhead_sha: {}\nstate: {}{}\n",
        d.pr.owner,
        d.pr.repo,
        d.pr.number,
        d.title,
        d.author,
        d.base_ref,
        d.head_sha,
        d.state,
        if d.draft { " (draft)" } else { "" }
    ));
    if !d.body.is_empty() {
        out.push_str(&format!("\n{}\n", d.body));
    }
    out.push_str("</pull_request>\n\n");

    out.push_str("<ci_status>\n");
    out.push_str(&render_checks(&d.checks));
    out.push_str("</ci_status>\n\n");

    // The diff draws from its own cap AND whatever pack budget remains, so no
    // single section can push the whole pack past max_pack_chars.
    out.push_str("<diff>\n");
    let diff_cap = cfg.max_diff_chars.min(cfg.max_pack_chars.saturating_sub(out.len()));
    out.push_str(&budget_diff(&d.diff, diff_cap));
    out.push_str("\n</diff>\n\n");

    // Guidance files are capped per file and budgeted against the pack ceiling.
    // They used to be appended in full and unbounded — a large CLAUDE.md alone
    // could exceed max_pack_chars and produce an oversized model request.
    for (path, content) in d.claude_mds.iter().filter(|(_, c)| c.is_some()) {
        let open = format!("<claude_md path=\"{path}\">\n");
        let close = "\n</claude_md>\n\n";
        let overhead = open.len() + close.len();
        let remaining = cfg.max_pack_chars.saturating_sub(out.len());
        if remaining <= overhead {
            break; // no room left for another section
        }
        let capped = cap_lines(content.as_ref().unwrap(), cfg.max_file_lines);
        let body = fit_within(
            &capped,
            remaining - overhead,
            "\n… [guidance truncated to fit the review context budget]",
        );
        out.push_str(&open);
        out.push_str(&body);
        out.push_str(close);
    }

    // Full file contents until the pack budget is spent. Which files make the
    // cut is risk-ranked — most-changed first, generated files last — while
    // render order stays path-sorted. Deterministic: depends only on sizes.
    let include = select_for_budget(&d.changed_files, cfg.max_pack_chars.saturating_sub(out.len()));
    let mut omitted: Vec<&str> = Vec::new();
    for f in &d.changed_files {
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

    let mut blame_sections: Vec<(String, String)> = d
        .blames
        .iter()
        .map(|(path, ranges)| {
            let file_ranges = d
                .changed_files
                .iter()
                .find(|f| f.path == *path)
                .map(|f| f.changed_ranges.clone())
                .unwrap_or_default();
            (path.clone(), render_blame(ranges, &file_ranges))
        })
        .filter(|(_, b)| !b.is_empty())
        .collect();
    blame_sections.sort_by(|a, b| a.0.cmp(&b.0));
    if !blame_sections.is_empty() {
        let mut block = String::from("<blame note=\"history of the lines this PR touches\">\n");
        for (path, b) in blame_sections {
            block.push_str(&format!("## {path}\n{b}"));
        }
        block.push_str("</blame>\n\n");
        // Supplementary section: include only if it fits the remaining budget.
        if out.len() + block.len() <= cfg.max_pack_chars {
            out.push_str(&block);
        }
    }

    if !d.prior_comments.is_empty() {
        let block = format!(
            "<prior_review_comments note=\"feedback on past PRs that touched these files\">\n{}</prior_review_comments>\n\n",
            d.prior_comments
        );
        if out.len() + block.len() <= cfg.max_pack_chars {
            out.push_str(&block);
        }
    }

    out
}

/// Truncate `s` to at most `max_bytes` on a char boundary, appending `note` when
/// it was cut. Returns the whole string when it already fits.
fn fit_within(s: &str, max_bytes: usize, note: &str) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes.saturating_sub(note.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{note}", &s[..end])
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
pub fn claude_md_candidates(files: &[ChangedFile]) -> Vec<String> {
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

/// Blame lines that overlap the ranges this PR touched.
pub fn render_blame(ranges: &[BlameRange], changed: &[(u32, u32)]) -> String {
    let mut out = String::new();
    for (s, e, oid, date, author, headline) in ranges {
        let overlaps = changed.iter().any(|(cs, ce)| *s <= *ce && *e >= *cs);
        if overlaps {
            out.push_str(&format!("L{s}-L{e} {oid} {date} ({author}): {headline}\n"));
        }
    }
    out
}

/// Frame the neutral CI rollup: overall line + sorted, pre-formatted rows.
pub fn render_checks(checks: &CheckRollup) -> String {
    match &checks.overall {
        None => "no check runs reported\n".to_string(),
        Some(state) => {
            let mut out = format!("overall: {state}\n");
            let mut rows = checks.rows.clone();
            rows.sort();
            out.push_str(&rows.concat());
            out
        }
    }
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
