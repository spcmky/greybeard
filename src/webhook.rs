//! Forge-neutral webhook decoding for serve mode.
//!
//! The debounce / dedupe / circuit-breaker / inflight machinery in `server.rs`
//! is forge-agnostic; only three things differ per forge, and they live here:
//! secret verification (GitHub HMAC vs GitLab plain token compare), the
//! delivery/dedupe id header, and payload → [`Trigger`] parsing. The
//! forge-specific parsers are pure functions of `(event, body)` so they unit-
//! test without constructing a `HeaderMap`.

use http::HeaderMap;
use serde_json::Value;

use crate::config::Forge;
use crate::github::PrRef;

/// What a webhook delivery resolves to.
pub enum Verdict {
    /// Health/handshake ping — answer 200 without scheduling.
    Ping,
    /// Nothing to do; the `&str` is the response reason.
    Ignore(&'static str),
    /// Schedule a review.
    Review(Trigger),
}

/// A resolved request to review a change.
pub struct Trigger {
    pub pr: PrRef,
    /// opened | synchronize | reopened | open | reopen | update | command …
    pub action: String,
    /// Forced (mention command) vs an automatic event.
    pub force: bool,
    /// GitHub App installation id; always `None` on GitLab (no app model).
    pub installation: Option<u64>,
    /// The actor that triggered it — the command channel's per-user cooldown
    /// key. Empty for automatic events.
    pub sender: String,
}

fn header<'a>(h: &'a HeaderMap, key: &str) -> &'a str {
    h.get(key).and_then(|v| v.to_str().ok()).unwrap_or("")
}

/// Constant-time byte compare (length may leak; contents do not).
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Verify the delivery's authenticity for the configured forge: GitHub signs
/// the body (HMAC-SHA256 in `X-Hub-Signature-256`); GitLab sends the configured
/// secret verbatim in `X-Gitlab-Token` (plain, constant-time compared).
pub fn verify(forge: Forge, secret: &str, headers: &HeaderMap, body: &[u8]) -> bool {
    match forge {
        Forge::GitHub => {
            let sig = header(headers, "x-hub-signature-256");
            !sig.is_empty() && crate::server::verify_signature(secret, body, sig)
        }
        Forge::GitLab => verify_gitlab_token(secret, header(headers, "x-gitlab-token")),
    }
}

/// GitLab webhook auth: the received `X-Gitlab-Token` must equal the configured
/// secret exactly (no HMAC). Empty received token never matches.
pub fn verify_gitlab_token(secret: &str, received: &str) -> bool {
    !received.is_empty() && ct_eq(received.as_bytes(), secret.as_bytes())
}

/// The delivery id used to dedupe redelivered webhooks.
pub fn delivery_id(forge: Forge, headers: &HeaderMap) -> String {
    match forge {
        Forge::GitHub => header(headers, "x-github-delivery"),
        Forge::GitLab => header(headers, "x-gitlab-event-uuid"),
    }
    .to_string()
}

/// Decode a verified delivery into a [`Verdict`].
pub fn parse(forge: Forge, bot_login: &str, headers: &HeaderMap, body: &Value) -> Verdict {
    match forge {
        Forge::GitHub => parse_github(bot_login, header(headers, "x-github-event"), body),
        Forge::GitLab => parse_gitlab(bot_login, header(headers, "x-gitlab-event"), body),
    }
}

/// GitHub `pull_request` / `issue_comment` events (and `ping`).
pub fn parse_github(bot_login: &str, event: &str, v: &Value) -> Verdict {
    match event {
        "ping" => Verdict::Ping,
        "pull_request" => {
            let action = v["action"].as_str().unwrap_or("");
            if !matches!(action, "opened" | "synchronize" | "ready_for_review" | "reopened") {
                return Verdict::Ignore("ignored action");
            }
            // Drafts wait for ready_for_review (eligibility would skip anyway).
            if v["pull_request"]["draft"].as_bool().unwrap_or(false) {
                return Verdict::Ignore("draft — waiting for ready_for_review");
            }
            let Some(pr) = github_pr(v) else {
                return Verdict::Ignore("no PR coordinates");
            };
            Verdict::Review(Trigger {
                pr,
                action: action.to_string(),
                force: false,
                installation: crate::server::installation_from_payload(v),
                sender: String::new(),
            })
        }
        "issue_comment" => {
            // Command channel: "@greybeard-bot review" on a PR forces a re-review.
            let is_pr = v["issue"]["pull_request"].is_object();
            let created = v["action"] == "created";
            let body_text = v["comment"]["body"].as_str().unwrap_or("").trim();
            let sender = v["comment"]["user"]["login"].as_str().unwrap_or("");
            let mention = format!("@{}", bot_login.trim_end_matches("[bot]"));
            if created
                && is_pr
                && sender != bot_login
                && body_text.starts_with(&mention)
                && body_text.contains("review")
            {
                let repo_full = v["repository"]["full_name"].as_str().unwrap_or("");
                let number = v["issue"]["number"].as_u64().unwrap_or(0);
                let mut parts = repo_full.split('/');
                if let (Some(owner), Some(repo)) = (parts.next(), parts.next()) {
                    if number > 0 {
                        return Verdict::Review(Trigger {
                            pr: PrRef { owner: owner.to_string(), repo: repo.to_string(), number },
                            action: "command".to_string(),
                            force: true,
                            installation: crate::server::installation_from_payload(v),
                            sender: sender.to_string(),
                        });
                    }
                }
            }
            Verdict::Ignore("ignored comment")
        }
        _ => Verdict::Ignore("ignored event"),
    }
}

fn github_pr(v: &Value) -> Option<PrRef> {
    Some(PrRef {
        owner: v["repository"]["owner"]["login"].as_str()?.to_string(),
        repo: v["repository"]["name"].as_str()?.to_string(),
        number: v["pull_request"]["number"].as_u64()?,
    })
}

/// GitLab `Merge Request Hook` / `Note Hook` events.
pub fn parse_gitlab(bot_login: &str, event: &str, v: &Value) -> Verdict {
    match event {
        "Merge Request Hook" => {
            let attrs = &v["object_attributes"];
            let action = attrs["action"].as_str().unwrap_or("");
            // open / reopen always; `update` only when it carries an `oldrev`
            // (a code push) — otherwise label/assignee/description edits, which
            // also fire `update`, would each trigger a review.
            let is_push = action == "update" && !attrs["oldrev"].is_null();
            if !(action == "open" || action == "reopen" || is_push) {
                return Verdict::Ignore("ignored action");
            }
            // Read `draft` (not the deprecated `work_in_progress`).
            if attrs["draft"].as_bool().unwrap_or(false) {
                return Verdict::Ignore("draft");
            }
            let path = v["project"]["path_with_namespace"].as_str().unwrap_or("");
            let iid = attrs["iid"].as_u64().unwrap_or(0);
            let Some(pr) = crate::gitlab::pr_from_project_path(path, iid) else {
                return Verdict::Ignore("no MR coordinates");
            };
            Verdict::Review(Trigger {
                pr,
                action: action.to_string(),
                force: false,
                installation: None,
                sender: v["user"]["username"].as_str().unwrap_or("").to_string(),
            })
        }
        "Note Hook" => {
            let attrs = &v["object_attributes"];
            if attrs["noteable_type"].as_str() != Some("MergeRequest") {
                return Verdict::Ignore("ignored comment");
            }
            let body_text = attrs["note"].as_str().unwrap_or("").trim();
            let sender = v["user"]["username"].as_str().unwrap_or("");
            let mention = format!("@{}", bot_login.trim_end_matches("[bot]"));
            if sender != bot_login
                && body_text.starts_with(&mention)
                && body_text.contains("review")
            {
                let path = v["project"]["path_with_namespace"].as_str().unwrap_or("");
                let iid = v["merge_request"]["iid"].as_u64().unwrap_or(0);
                if let Some(pr) = crate::gitlab::pr_from_project_path(path, iid) {
                    return Verdict::Review(Trigger {
                        pr,
                        action: "command".to_string(),
                        force: true,
                        installation: None,
                        sender: sender.to_string(),
                    });
                }
            }
            Verdict::Ignore("ignored comment")
        }
        _ => Verdict::Ignore("ignored event"),
    }
}
