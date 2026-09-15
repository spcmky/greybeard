use std::time::Instant;

use anyhow::Result;
use futures::future::join_all;

use crate::config::Config;
use crate::forge::Forge;
use crate::github::PrRef;
use crate::llm::{Llm, Tier};
use crate::pipeline::{compose, Confirmed, Eligibility, LensReport, Verdict};
use crate::prompts;
use crate::telemetry::Telemetry;

const MAX_FINDINGS_PER_LENS: usize = 8;

pub struct ReviewArgs {
    pub dry_run: bool,
    pub force: bool,
}

/// What a finished run looked like — feeds the structured run event, the
/// metrics counters, and the JSONL log line in server mode.
#[derive(Debug, Clone, Default)]
pub struct RunSummary {
    /// posted | dry-run | skipped
    pub outcome: &'static str,
    /// Why a skipped run skipped.
    pub reason: Option<String>,
    pub confirmed: usize,
    pub minor: usize,
    pub candidates: usize,
    pub unverified: usize,
    pub lenses_failed: usize,
    pub duration: std::time::Duration,
    pub usage: crate::llm::Usage,
}

impl RunSummary {
    fn skipped(reason: &str, wall: Instant, telemetry: &Telemetry) -> Self {
        Self {
            outcome: "skipped",
            reason: Some(reason.to_string()),
            duration: wall.elapsed(),
            usage: telemetry.totals(),
            ..Self::default()
        }
    }
}

pub async fn run<F: Forge>(
    forge: &F,
    llm: &Llm,
    cfg: &Config,
    telemetry: &Telemetry,
    pr: &PrRef,
    args: &ReviewArgs,
) -> Result<RunSummary> {
    let wall = Instant::now();

    // ── Stage 0: context pack + deterministic eligibility ──────────────────
    let pack = forge.build_pack(pr, cfg).await?;
    eprintln!(
        "greybeard: pack built in {}ms ({} changed files, {} chars)",
        pack.fetch_ms,
        pack.changed_files.len(),
        pack.rendered.len()
    );

    if !args.force {
        // A non-open PR is a benign race (closed between event and run), not a
        // pipeline failure — it must not feed the health failure streak.
        if pack.state != "open" {
            println!("skipped: PR is {} (use --force to override)", pack.state);
            return Ok(RunSummary::skipped(&format!("PR is {}", pack.state), wall, telemetry));
        }
        if pack.draft {
            println!("skipped: PR is a draft");
            return Ok(RunSummary::skipped("draft", wall, telemetry));
        }
        // Hard pre-model rule: bot-authored PRs (dependabot batches would
        // otherwise burn a review each). GREYBEARD_REVIEW_BOT_PRS=true or
        // --force to override.
        if pack.author_is_bot && !cfg.review_bot_prs {
            println!("skipped: bot-authored PR ({})", pack.author);
            return Ok(RunSummary::skipped(&format!("bot-authored PR ({})", pack.author), wall, telemetry));
        }
        if let Some((_, sha)) = &pack.existing_comment {
            if *sha == pack.head_sha {
                println!("skipped: already reviewed {}", &pack.head_sha[..7]);
                return Ok(RunSummary::skipped("already reviewed this head", wall, telemetry));
            }
        }
    }
    if pack.changed_files.is_empty() {
        println!("skipped: no changed files");
        return Ok(RunSummary::skipped("no changed files", wall, telemetry));
    }

    let system = prompts::system_blocks(&pack.rendered);

    // ── Stage 0b: cache warm (lens tier, prefill-only) ∥ eligibility (fast) ─
    let (_, eligibility) = tokio::join!(llm.warm(Tier::Lens, &system), async {
        llm.structured::<Eligibility>(
            Tier::Verify,
            "eligibility",
            &system,
            &prompts::eligibility_user_message(),
            &prompts::eligibility_schema(),
        )
        .await
    });
    if !args.force {
        if let Ok(e) = &eligibility {
            if e.skip {
                println!("skipped: {}", e.reason);
                return Ok(RunSummary::skipped(&e.reason, wall, telemetry));
            }
        }
    }

    // ── Stage 1+2: lenses in parallel, each lens's findings verified as soon
    //    as that lens completes (no barrier between find and verify) ─────────
    let lenses_failed = std::sync::atomic::AtomicUsize::new(0);
    let lens_chains = prompts::LENSES.iter().map(|lens| {
        let system = system.clone();
        let lenses_failed = &lenses_failed;
        async move {
            let report = llm
                .structured::<LensReport>(
                    Tier::Lens,
                    &format!("lens:{}", lens.key),
                    &system,
                    &prompts::lens_user_message(lens),
                    &prompts::findings_schema(),
                )
                .await;
            let findings = match report {
                Ok(r) => r.findings,
                Err(e) => {
                    eprintln!("greybeard: lens {} dropped: {e}", lens.key);
                    lenses_failed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    return Vec::new();
                }
            };
            let candidates: Vec<_> = findings.into_iter().take(MAX_FINDINGS_PER_LENS).collect();
            let verdicts = join_all(candidates.iter().map(|f| {
                let system = system.clone();
                let user = prompts::verify_user_message(
                    lens.key,
                    &f.file,
                    f.line,
                    &f.severity,
                    &f.claim,
                    &f.evidence,
                );
                async move {
                    llm.structured::<Verdict>(
                        Tier::Verify,
                        &format!("verify:{}", short_path(&f.file)),
                        &system,
                        &user,
                        &prompts::verdict_schema(),
                    )
                    .await
                }
            }))
            .await;
            candidates.into_iter().zip(verdicts).collect::<Vec<_>>()
        }
    });
    let results: Vec<Vec<_>> = join_all(lens_chains).await;

    let mut candidates_total = 0usize;
    let mut unverified = 0usize;
    let mut confirmed: Vec<Confirmed> = Vec::new();
    let mut minor: Vec<Confirmed> = Vec::new();
    for (lens, chain) in prompts::LENSES.iter().zip(results) {
        for (finding, verdict) in chain {
            candidates_total += 1;
            match verdict {
                Ok(v) if v.real && v.confidence >= cfg.confidence_threshold => {
                    confirmed.push(Confirmed {
                        finding,
                        lens: lens.key.to_string(),
                        confidence: v.confidence,
                    });
                }
                // Verified true but low importance: the collapsed Minor notes
                // band (gate option B — docs/GATE.md).
                Ok(v) if v.real && v.confidence >= cfg.minor_threshold => {
                    minor.push(Confirmed {
                        finding,
                        lens: lens.key.to_string(),
                        confidence: v.confidence,
                    });
                }
                Ok(v) => {
                    eprintln!(
                        "greybeard: rejected [{}] {} ({}): {}",
                        v.confidence, finding_summary(&finding.file, finding.line), lens.key, v.reason
                    );
                }
                // A dead verifier must degrade LOUDLY, never silently drop
                // findings.
                Err(e) => {
                    unverified += 1;
                    eprintln!(
                        "greybeard: UNVERIFIED {} ({}): {e}",
                        finding_summary(&finding.file, finding.line), lens.key
                    );
                }
            }
        }
    }

    // ── Stage 3: dedupe, render, post (update in place) ─────────────────────
    let confirmed = compose::dedupe(confirmed);
    let minor = compose::dedupe_minor(minor, &confirmed);
    let body = compose::render_comment(
        cfg.forge,
        cfg.forge_base_url.as_deref(),
        pr,
        &pack.head_sha,
        &confirmed,
        &minor,
        candidates_total,
        prompts::LENSES.len(),
        unverified,
    );

    if args.dry_run {
        println!("--- dry run: comment body ---\n{body}");
    } else {
        // Cheap "still open?" re-check replaces the old eligibility re-check agent.
        if !args.force && !forge.still_open(pr).await? {
            println!("skipped: PR closed while reviewing — not posting");
            return Ok(RunSummary::skipped("PR closed while reviewing", wall, telemetry));
        }
        let url = forge.upsert_comment(&pack, &body).await?;
        println!("posted: {url}");
    }

    println!(
        "{} confirmed, {} minor / {} candidates in {:.1}s",
        confirmed.len(),
        minor.len(),
        candidates_total,
        wall.elapsed().as_secs_f32()
    );
    print!("{}", telemetry.report(wall.elapsed()));
    Ok(RunSummary {
        outcome: if args.dry_run { "dry-run" } else { "posted" },
        reason: None,
        confirmed: confirmed.len(),
        minor: minor.len(),
        candidates: candidates_total,
        unverified,
        lenses_failed: lenses_failed.into_inner(),
        duration: wall.elapsed(),
        usage: telemetry.totals(),
    })
}

fn short_path(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_string()
}

fn finding_summary(file: &str, line: Option<u32>) -> String {
    match line {
        Some(l) => format!("{file}:{l}"),
        None => file.to_string(),
    }
}
