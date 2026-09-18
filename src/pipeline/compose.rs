use crate::config::Forge;
use crate::github::{comment, PrRef};
use crate::pipeline::{severity_rank, Confirmed};

/// Deterministic dedupe: same file + overlapping line window (±3) counts as one
/// finding; keep the highest severity (then highest confidence), credit all lenses.
pub fn dedupe(mut confirmed: Vec<Confirmed>) -> Vec<Confirmed> {
    confirmed.sort_by(|a, b| {
        severity_rank(&a.finding.severity)
            .cmp(&severity_rank(&b.finding.severity))
            .then(b.confidence.cmp(&a.confidence))
            .then(a.finding.file.cmp(&b.finding.file))
    });
    let mut kept: Vec<Confirmed> = Vec::new();
    for c in confirmed {
        let dup = kept.iter_mut().find(|k| {
            k.finding.file == c.finding.file
                && match (k.finding.line, c.finding.line) {
                    (Some(a), Some(b)) => a.abs_diff(b) <= 3,
                    (None, None) => true,
                    _ => false,
                }
        });
        match dup {
            Some(k) => {
                if !k.lens.contains(&c.lens) {
                    k.lens = format!("{}, {}", k.lens, c.lens);
                }
            }
            None => kept.push(c),
        }
    }
    kept
}

/// The one line of lore Greybeard allows itself: a closing verdict.
/// Everything above it stays plain-engineer (docs/VOICE.md).
fn verdict_line(confirmed: &[Confirmed], minor: &[Confirmed]) -> &'static str {
    if confirmed.iter().any(|c| c.finding.severity == "blocker") {
        "You shall not pass."
    } else if !confirmed.is_empty() || !minor.is_empty() {
        "Pass — but mind the cracks in the bridge."
    } else {
        "You shall pass."
    }
}

/// Dedupe the minor band against itself, then drop anything that collides
/// with a confirmed finding (the same defect found by two lenses can land in
/// different bands — the higher band wins).
pub fn dedupe_minor(minor: Vec<Confirmed>, confirmed: &[Confirmed]) -> Vec<Confirmed> {
    dedupe(minor)
        .into_iter()
        .filter(|m| {
            !confirmed.iter().any(|c| {
                c.finding.file == m.finding.file
                    && match (c.finding.line, m.finding.line) {
                        (Some(a), Some(b)) => a.abs_diff(b) <= 3,
                        (None, None) => true,
                        _ => false,
                    }
            })
        })
        .collect()
}

const MAX_MINOR_SHOWN: usize = 5;
/// Marker v2 payload caps — the marker is comment metadata, not the report.
const MAX_MARKER_FINDINGS: usize = 20;
const MAX_MARKER_CLAIM_CHARS: usize = 200;

/// Where humans report Greybeard being wrong (or wish for features) — linked
/// from every comment footer.
const FEEDBACK_URL: &str = "https://github.com/REI-Labs/greybeard/issues";

/// Render the single Greybeard PR comment (created once, updated in place).
/// `forge`/`base_url` pick the permalink shape (GitHub blob vs GitLab `/-/blob`,
/// and the self-managed web root).
#[allow(clippy::too_many_arguments)]
pub fn render_comment(
    forge: Forge,
    base_url: Option<&str>,
    pr: &PrRef,
    head_sha: &str,
    confirmed: &[Confirmed],
    minor: &[Confirmed],
    candidates: usize,
    lenses_run: usize,
    unverified: usize,
    lenses_failed: usize,
) -> String {
    let short_sha = &head_sha[..head_sha.len().min(7)];
    let mut out = String::from("## Greybeard review\n\n");
    let degraded = unverified > 0 || lenses_failed > 0;

    if confirmed.is_empty() && minor.is_empty() {
        out.push_str(if degraded {
            "Review incomplete. No findings were confirmed.\n"
        } else {
            "No issues found in the available review context.\n"
        });
    } else if confirmed.is_empty() {
        out.push_str("No blocking issues found — minor notes below.\n");
    } else {
        out.push_str(&format!(
            "Found {} issue{}:\n\n",
            confirmed.len(),
            if confirmed.len() == 1 { "" } else { "s" }
        ));
        for (i, c) in confirmed.iter().enumerate() {
            out.push_str(&format!(
                "{}. **[{}]** {}\n   {}\n   {}\n\n",
                i + 1,
                c.finding.severity,
                c.finding.claim.trim_end_matches('.'),
                c.finding.evidence.replace('\n', " "),
                crate::pack::permalink(
                    forge,
                    base_url,
                    pr,
                    head_sha,
                    &c.finding.file,
                    c.finding.line
                ),
            ));
        }
    }

    if !minor.is_empty() {
        let mut sorted: Vec<&Confirmed> = minor.iter().collect();
        sorted.sort_by_key(|c| std::cmp::Reverse(c.confidence));
        out.push_str(&format!(
            "\n<details>\n<summary>Minor notes ({}) — verified, low impact</summary>\n\n",
            minor.len()
        ));
        for c in sorted.iter().take(MAX_MINOR_SHOWN) {
            out.push_str(&format!(
                "- **[{}]** {}: [{}]({})\n  {}\n",
                c.finding.severity,
                c.finding.claim.trim_end_matches('.'),
                match c.finding.line {
                    Some(l) => format!("{}:{}", c.finding.file, l),
                    None => c.finding.file.clone(),
                },
                crate::pack::permalink(
                    forge,
                    base_url,
                    pr,
                    head_sha,
                    &c.finding.file,
                    c.finding.line
                ),
                c.finding.evidence.replace('\n', " "),
            ));
        }
        if minor.len() > MAX_MINOR_SHOWN {
            out.push_str(&format!(
                "- …and {} more below the display cap.\n",
                minor.len() - MAX_MINOR_SHOWN
            ));
        }
        out.push_str("\n</details>\n");
    }

    // A degraded run degrades LOUDLY: no verdict line, explicit warnings instead
    // (bake-off 2026-08-17 — never a silent "You shall pass"). Two independent
    // ways to degrade: a lens that never produced findings (find stage failed →
    // incomplete coverage), or a dead verifier (verify stage failed → findings
    // dropped). Either one suppresses the verdict.
    if lenses_failed > 0 {
        out.push_str(&format!(
            "\n**Coverage incomplete:** {lenses_failed} of {lenses_run} review lens{} failed to \
             run on this pass — those checks did not happen. Treat this review as incomplete.\n",
            if lenses_failed == 1 { "" } else { "es" },
        ));
    }
    if unverified > 0 {
        out.push_str(&format!(
            "\n**Verification degraded:** {unverified} candidate finding{} could not be \
             verified on this run and {} not shown. Treat this review as incomplete.\n",
            if unverified == 1 { "" } else { "s" },
            if unverified == 1 { "is" } else { "are" },
        ));
    }
    if !degraded {
        out.push_str(&format!("\n_{}_\n", verdict_line(confirmed, minor)));
    }
    let verdict = if degraded {
        "degraded"
    } else if confirmed.iter().any(|c| c.finding.severity == "blocker") {
        "blocked"
    } else if !confirmed.is_empty() || !minor.is_empty() {
        "cracks"
    } else {
        "pass"
    };
    let mut minor_sorted: Vec<&Confirmed> = minor.iter().collect();
    minor_sorted.sort_by_key(|c| std::cmp::Reverse(c.confidence));
    let marker_findings: Vec<serde_json::Value> = confirmed
        .iter()
        .map(|c| (c, "confirmed"))
        .chain(minor_sorted.into_iter().map(|c| (c, "minor")))
        .take(MAX_MARKER_FINDINGS)
        .map(|(c, band)| {
            serde_json::json!({
                "file": c.finding.file,
                "line": c.finding.line,
                "severity": c.finding.severity,
                "band": band,
                "confidence": c.confidence,
                "claim": c.finding.claim.chars().take(MAX_MARKER_CLAIM_CHARS).collect::<String>(),
            })
        })
        .collect();

    out.push_str(&format!(
        "\n<sub>Greybeard · reviewed {short_sha} · {lenses_run} lenses, {candidates} candidates, {} confirmed, {} minor · [bugs/ideas]({FEEDBACK_URL})</sub>\n{}\n",
        confirmed.len(),
        minor.len(),
        comment::render_marker_v2(head_sha, verdict, marker_findings),
    ));
    out
}

/// Terminal report: working-tree coordinates, without remote links or PR markers.
pub fn render_local(
    local: &crate::local::LocalReview,
    report: &super::review::ReviewReport,
) -> String {
    let mut out = format!(
        "## Greybeard local review\n\nRepository: {}\nComparison: {}\n\n",
        local.root.display(),
        local.comparison
    );
    if report.confirmed.is_empty() && report.minor.is_empty() {
        out.push_str(
            if report.summary.unverified > 0 || report.summary.lenses_failed > 0 {
                "Review incomplete. No findings were confirmed.\n"
            } else {
                "No issues found in the available local context.\n"
            },
        );
    }
    for (heading, findings) in [
        ("Findings", &report.confirmed),
        ("Minor notes", &report.minor),
    ] {
        if findings.is_empty() {
            continue;
        }
        out.push_str(&format!("\n### {heading}\n\n"));
        for c in findings {
            let location = match c.finding.line {
                Some(line) => format!("{}:{line}", c.finding.file),
                None => c.finding.file.clone(),
            };
            out.push_str(&format!(
                "- **[{}] {}** — {}\n  {}\n",
                c.finding.severity,
                location,
                c.finding.claim.trim_end_matches('.'),
                c.finding.evidence.replace('\n', " ")
            ));
        }
    }
    let summary = &report.summary;
    if summary.lenses_failed > 0 {
        out.push_str(&format!(
            "\n**Coverage incomplete:** {} review lenses failed.\n",
            summary.lenses_failed
        ));
    }
    if summary.unverified > 0 {
        out.push_str(&format!(
            "\n**Verification degraded:** {} candidate findings could not be verified.\n",
            summary.unverified
        ));
    }
    if summary.lenses_failed == 0 && summary.unverified == 0 {
        out.push_str(&format!(
            "\n_{}_\n",
            verdict_line(&report.confirmed, &report.minor)
        ));
    }
    out.push_str("\nLocal context only; live CI status and prior PR feedback were unavailable.\n");
    out
}
