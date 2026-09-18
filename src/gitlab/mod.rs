//! GitLab backend: a thin REST v4 client implementing the [`Forge`] trait.
//!
//! GitLab has no App/JWT/installation model — identity is just a token
//! (`GREYBEARD_TOKEN`, sent as `PRIVATE-TOKEN`); a project/group access token
//! posts notes as its bot user. The single-round-trip GraphQL the GitHub
//! backend uses becomes several parallel REST calls here (docs/GITLAB.md).

pub mod pack;

use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use crate::config::Config;
use crate::forge::Forge;
use crate::github::comment::parse_marker_full;
use crate::github::PrRef;
use crate::pack::{ContextPack, ExistingComment};

/// REST v4 API base + web base from an optional forge root
/// (`GREYBEARD_FORGE_URL`). `None` targets public gitlab.com; `Some(host)` a
/// self-managed root, where the API lives under `/api/v4`.
pub fn api_endpoints(base: Option<&str>) -> (String, String) {
    match base.map(|h| h.trim().trim_end_matches('/')).filter(|h| !h.is_empty()) {
        None => ("https://gitlab.com/api/v4".to_string(), "https://gitlab.com".to_string()),
        Some(host) => (format!("{host}/api/v4"), host.to_string()),
    }
}

/// Parse a GitLab MR URL into a [`PrRef`]. GitLab project paths nest namespaces
/// and use a `/-/` separator:
/// `https://gitlab.com/group/subgroup/project/-/merge_requests/123`. The leading
/// namespaces go in `owner` and the project in `repo`, so `PrRef::project()`
/// rebuilds the full path (`group/subgroup/project`).
pub fn parse_mr_url(url: &str) -> Result<PrRef> {
    // Drop any query string / fragment first: neither is part of the project
    // path or the iid, and a namespace segment can never contain '?' or '#'.
    // Without this a copied link like `.../merge_requests/5?tab=diffs` or
    // `.../5#note_1` would fail the iid parse below.
    let path_only = url.split(['?', '#']).next().unwrap_or(url);
    let trimmed = path_only.trim().trim_end_matches('/');
    let no_scheme = trimmed
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    let (left, right) = no_scheme.split_once("/-/merge_requests/").with_context(|| {
        format!("not a GitLab MR URL: {url} (expected https://<host>/<path>/-/merge_requests/<iid>)")
    })?;
    // left = host/group/.../project — drop the host segment.
    let mut segs = left.split('/');
    let _host = segs.next();
    let path: Vec<&str> = segs.filter(|s| !s.is_empty()).collect();
    if path.len() < 2 {
        bail!("GitLab MR URL missing a namespaced project path: {url}");
    }
    let (repo, owner_parts) = path.split_last().unwrap();
    let iid = right.split('/').next().unwrap_or("");
    let number: u64 = iid.parse().with_context(|| format!("MR iid in {url}"))?;
    Ok(PrRef {
        owner: owner_parts.join("/"),
        repo: (*repo).to_string(),
        number,
    })
}

/// Build a [`PrRef`] from a GitLab project path (`group/subgroup/project`) and
/// an MR iid — the shape webhook payloads carry (`path_with_namespace` and the
/// `iid`). Leading namespaces go in `owner`, the project in `repo`. None if the
/// path has fewer than two segments or the iid is 0.
pub fn pr_from_project_path(path: &str, iid: u64) -> Option<PrRef> {
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if segs.len() < 2 || iid == 0 {
        return None;
    }
    let (repo, owner) = segs.split_last().unwrap();
    Some(PrRef {
        owner: owner.join("/"),
        repo: (*repo).to_string(),
        number: iid,
    })
}

/// Percent-encode a path segment set for use as a GitLab `:id` / `:file_path`
/// (encodes `/` as `%2F`, per the REST convention for URL-encoded paths).
pub fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Scan a page of MR notes for greybeard's own marker. `me` is our username;
/// only a note WE authored is trusted (a forged marker on someone else's note
/// is ignored). Returns the note id, reviewed sha, and completion.
pub fn find_marker_in_notes(notes: &[Value], me: &str) -> Option<ExistingComment> {
    for n in notes {
        if n["author"]["username"].as_str() != Some(me) {
            continue;
        }
        if let Some((sha, verdict)) = parse_marker_full(n["body"].as_str().unwrap_or("")) {
            return Some(ExistingComment {
                id: n["id"].as_u64().unwrap_or(0).to_string(),
                sha,
                complete: verdict.as_deref() != Some("degraded"),
            });
        }
    }
    None
}

/// A crude bot-author heuristic. GitLab has no first-class bot-actor type
/// (docs/GITLAB.md open question), so key off the service-account username
/// convention. Conservative: only usernames that clearly read as bots.
pub fn looks_like_bot(username: &str) -> bool {
    let u = username.to_ascii_lowercase();
    // Separator-delimited bot suffix convention — `-bot` / `_bot` / `.bot`.
    // A separator is required so real names ending in "bot" (robot, talbot)
    // don't match.
    let suffix_bot = u.ends_with("-bot") || u.ends_with("_bot") || u.ends_with(".bot");
    // GitLab project/group access tokens author as `project_<id>_bot_<hash>` /
    // `group_<id>_bot_<hash>` — the `_bot` segment is what makes them a bot,
    // not the prefix alone (a human could own a `project_planning` account).
    let token_user =
        (u.starts_with("project_") || u.starts_with("group_")) && u.contains("_bot");
    suffix_bot || token_user || u.contains("service-account")
}

/// Thin GitLab REST v4 client.
pub struct Gitlab {
    http: reqwest::Client,
    token: String,
    api_base: String,
    web_base: String,
    /// How we authenticated — always a token on GitLab (no App model).
    pub auth_mode: &'static str,
}

impl Gitlab {
    /// `base` is `GREYBEARD_FORGE_URL` (a self-managed GitLab root); `None`
    /// targets public gitlab.com. The token is `GREYBEARD_TOKEN` (the
    /// forge-neutral name), with `GITLAB_TOKEN` accepted as an alias.
    pub async fn connect(base: Option<&str>) -> Result<Self> {
        let (api_base, web_base) = api_endpoints(base);
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()?;
        let token = match std::env::var("GREYBEARD_TOKEN").or_else(|_| std::env::var("GITLAB_TOKEN")) {
            Ok(t) if !t.is_empty() => t,
            _ => bail!(
                "GREYBEARD_TOKEN is required for GREYBEARD_FORGE=gitlab — a GitLab personal, \
                 project, or group access token (sent as PRIVATE-TOKEN)."
            ),
        };
        Ok(Self { http, token, api_base, web_base, auth_mode: "access token" })
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let url = if path.starts_with("http") {
            path.to_string()
        } else {
            format!("{}{}", self.api_base, path)
        };
        self.http
            .request(method, url)
            .header("private-token", &self.token)
            .header("user-agent", "greybeard")
    }

    /// GET returning JSON, mapping 404 to None instead of an error.
    pub async fn get_optional(&self, path: &str) -> Result<Option<Value>> {
        let r = self.request(reqwest::Method::GET, path).send().await?;
        if r.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let status = r.status();
        let v: Value = r.json().await?;
        if !status.is_success() {
            bail!("GET {path}: {status}: {}", v["message"].as_str().unwrap_or("?"));
        }
        Ok(Some(v))
    }

    /// GET returning JSON, erroring on any non-success (incl. 404).
    pub async fn get_json(&self, path: &str) -> Result<Value> {
        self.get_optional(path)
            .await?
            .with_context(|| format!("GET {path}: not found"))
    }

    /// GET every page of a list endpoint, following GitLab's `X-Next-Page`
    /// header rather than guessing from page size — some endpoints (notably MR
    /// `/diffs`) cap `per_page` below what we request, so a "short page" check
    /// would stop early and silently drop later pages. Paths passed here must
    /// not already carry a query string.
    pub async fn get_paginated(&self, path: &str) -> Result<Vec<Value>> {
        let mut out = Vec::new();
        let mut page: u32 = 1;
        loop {
            let url = format!("{path}?per_page=100&page={page}");
            let r = self.request(reqwest::Method::GET, &url).send().await?;
            let status = r.status();
            let next = r
                .headers()
                .get("x-next-page")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.trim().to_string())
                .unwrap_or_default();
            if !status.is_success() {
                bail!("GET {url}: {status}");
            }
            let v: Value = r.json().await?;
            if let Some(arr) = v.as_array() {
                out.extend(arr.iter().cloned());
            }
            match next.parse::<u32>() {
                Ok(n) if n > page => page = n,
                _ => break,
            }
            if page > 200 {
                break; // hard safety cap
            }
        }
        Ok(out)
    }

    /// GET raw text (a file's contents), mapping 404 / non-success to None so a
    /// missing or oversized file drops out of the pack instead of failing it.
    pub async fn get_raw_optional(&self, path: &str) -> Result<Option<String>> {
        let r = self.request(reqwest::Method::GET, path).send().await?;
        if !r.status().is_success() {
            return Ok(None);
        }
        Ok(Some(r.text().await?))
    }

    /// The URL-encoded project id for `pr` (`group%2Fsubgroup%2Fproject`).
    pub fn project_id(pr: &PrRef) -> String {
        enc(&pr.project())
    }

    /// The authenticated bot/user's username (`/user`) — used to recognise our
    /// own notes when scanning for the marker.
    pub async fn current_username(&self) -> Result<String> {
        let u = self.get_json("/user").await?;
        u["username"]
            .as_str()
            .map(|s| s.to_string())
            .context("/user response had no username")
    }

    /// Find greybeard's existing note by its marker. `me` is our own username:
    /// a marker is only trusted on a note WE authored, otherwise anyone who can
    /// comment could forge review state. Pages the MR notes newest-edited first
    /// and stops at the first match.
    ///
    /// Returns `Err` on any lookup failure (network, non-success status, bad
    /// body): a failed lookup is NOT the same as "no note exists", and treating
    /// it as such would post a duplicate comment. Callers propagate the error
    /// and abort the run instead.
    pub async fn find_marker_note(&self, pr: &PrRef, me: &str) -> Result<Option<ExistingComment>> {
        let pid = Self::project_id(pr);
        let iid = pr.number;
        let mut page: u32 = 1;
        loop {
            // Greybeard edits its note in place, so ordering by `updated_at`
            // desc surfaces it early even though it was created on the first
            // review; correctness doesn't depend on the order (we page until a
            // match or the notes run out), only the common-case cost does.
            let url = format!(
                "/projects/{pid}/merge_requests/{iid}/notes\
                 ?per_page=100&page={page}&order_by=updated_at&sort=desc"
            );
            let r = self.request(reqwest::Method::GET, &url).send().await?;
            let next = r
                .headers()
                .get("x-next-page")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.trim().to_string())
                .unwrap_or_default();
            let status = r.status();
            if !status.is_success() {
                bail!("listing MR notes (page {page}): {status}");
            }
            let v: Value = r.json().await?;
            if let Some(arr) = v.as_array() {
                if let Some(ec) = find_marker_in_notes(arr, me) {
                    return Ok(Some(ec));
                }
            }
            match next.parse::<u32>() {
                Ok(p) if p > page => page = p,
                _ => return Ok(None),
            }
            if page > 200 {
                return Ok(None);
            }
        }
    }

    /// Fetch a file's contents at a ref. None for 404 / non-file.
    pub async fn file_contents(&self, pr: &PrRef, path: &str, git_ref: &str) -> Result<Option<String>> {
        let url = format!(
            "/projects/{}/repository/files/{}/raw?ref={}",
            Self::project_id(pr),
            enc(path),
            enc(git_ref)
        );
        self.get_raw_optional(&url).await
    }
}

impl Forge for Gitlab {
    async fn file_contents(&self, pr: &PrRef, path: &str, sha: &str) -> Result<Option<String>> {
        self.file_contents(pr, path, sha).await
    }

    fn auth_mode(&self) -> &'static str {
        self.auth_mode
    }

    async fn whoami(&self) -> Result<String> {
        let u = self.get_json("/user").await?;
        Ok(u["username"].as_str().unwrap_or("?").to_string())
    }

    async fn build_pack(&self, pr: &PrRef, cfg: &Config) -> Result<ContextPack> {
        pack::build(self, pr, cfg).await
    }

    async fn upsert_comment(&self, pack: &ContextPack, body: &str) -> Result<String> {
        let pid = Self::project_id(&pack.pr);
        let iid = pack.pr.number;
        let note_id = pack.existing_comment.as_ref().map(|ec| ec.id.clone());
        match note_id {
            Some(id) => {
                let path = format!("/projects/{pid}/merge_requests/{iid}/notes/{id}");
                let r = self
                    .request(reqwest::Method::PUT, &path)
                    .json(&json!({"body": body}))
                    .send()
                    .await?;
                let status = r.status();
                if !status.is_success() {
                    let v: Value = r.json().await.unwrap_or_default();
                    bail!("update note {status}: {}", v["message"].as_str().unwrap_or("?"));
                }
            }
            None => {
                let path = format!("/projects/{pid}/merge_requests/{iid}/notes");
                let r = self
                    .request(reqwest::Method::POST, &path)
                    .json(&json!({"body": body}))
                    .send()
                    .await?;
                let status = r.status();
                if !status.is_success() {
                    let v: Value = r.json().await.unwrap_or_default();
                    bail!("create note {status}: {}", v["message"].as_str().unwrap_or("?"));
                }
            }
        }
        // Notes have no stable standalone web URL in the create response; point
        // at the MR (the single greybeard note is easy to find there).
        Ok(format!(
            "{}/{}/-/merge_requests/{iid}",
            self.web_base,
            pack.pr.project()
        ))
    }

    async fn still_open(&self, pr: &PrRef) -> Result<bool> {
        let mr = self
            .get_json(&format!(
                "/projects/{}/merge_requests/{}",
                Self::project_id(pr),
                pr.number
            ))
            .await?;
        Ok(mr["state"].as_str() == Some("opened"))
    }
}
