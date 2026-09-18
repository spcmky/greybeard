pub mod compose;
pub mod discovery;
pub mod review;
pub mod verify;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Finding {
    pub file: String,
    #[serde(default)]
    pub line: Option<u32>,
    pub claim: String,
    /// Defaulted rather than required: on Bedrock there is no server-side
    /// schema enforcement, and one omitted field must not drop the whole lens.
    #[serde(default = "default_severity")]
    pub severity: String,
    #[serde(default)]
    pub evidence: String,
}

fn default_severity() -> String {
    "gap".to_string()
}

#[derive(Debug, Clone, Deserialize)]
pub struct LensReport {
    /// Required, NOT defaulted: a response missing this field (a bare `{}` or an
    /// `{"error":...}` object) must fail to parse so the structured() retry
    /// fires — otherwise it silently becomes a successful empty review. The
    /// prompt and schema both ask for `{"findings": []}` when there is nothing.
    pub findings: Vec<Finding>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum VerdictStatus {
    Confirmed,
    Refuted,
    Unverified,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Citation {
    pub file: String,
    pub line: u32,
    pub quote: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Verdict {
    pub citations: Vec<Citation>,
    pub trigger: String,
    pub expected: String,
    pub actual: String,
    pub safeguards: String,
    pub reason: String,
    pub requested_files: Vec<String>,
    pub status: VerdictStatus,
    pub confidence: u8,
    pub severity: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Eligibility {
    pub skip: bool,
    #[serde(default)]
    pub reason: String,
}

/// A finding that survived adversarial verification.
#[derive(Debug, Clone)]
pub struct Confirmed {
    pub finding: Finding,
    pub lens: String,
    pub confidence: u8,
}

pub fn severity_rank(sev: &str) -> u8 {
    match sev {
        "blocker" => 0,
        "gap" => 1,
        _ => 2,
    }
}
