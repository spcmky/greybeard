use std::time::Instant;

use anyhow::Result;
use futures::future::join_all;

use crate::config::Config;
use crate::forge::Forge;
use crate::github::PrRef;
use crate::llm::{Llm, Tier};
use crate::pipeline::{compose, verify, Confirmed, Eligibility, LensReport, VerdictStatus};
use crate::prompts;
use crate::telemetry::Telemetry;

const MAX_FINDINGS_PER_LENS: usize = 8;

fn finding_summary(file: &str, line: Option<u32>) -> String {
    match line {
        Some(line) => format!("{file}:{line}"),
        None => file.to_string(),
    }
}

pub struct ReviewArgs {
    pub dry_run: bool,
    pub force: bool,
}

/// What a finished run looked like — feeds the structured run event, the
/// metrics counters, and the JSONL log line in server mode.
#[derive(Debug, Clone, Default)]
pub struct RunSummary {
    /// posted | dry-run | local | skipped
    pub outcome: &'static str,
    /// Why a skipped run skipped.
    pub reason: Option<String>,
    pub confirmed: usize,
    pub minor: usize,
    pub candidates: usize,
    pub unverified: usize,
    pub lenses_failed: usize,
    pub lenses_run: usize,
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
    let source = verify::RemoteSource { forge, pack: &pack };
    let report = match analyze(&pack, &source, llm, cfg, telemetry, args, wall).await? {
        Analysis::Skipped(summary) => return Ok(summary),
        Analysis::Reviewed(report) => report,
    };
    let body = compose::render_comment(
        cfg.forge,
        cfg.forge_base_url.as_deref(),
        pr,
        &pack.head_sha,
        &report.confirmed,
        &report.minor,
        report.summary.candidates,
        report.summary.lenses_run,
        report.summary.unverified,
        report.summary.lenses_failed,
    );

    if args.dry_run {
        println!("--- dry run: comment body ---\n{body}");
    } else {
        // Cheap "still open?" re-check replaces the old eligibility re-check agent.
        if !args.force && !forge.still_open(pr).await? {
            println!("skipped: PR closed while reviewing — not posting");
            return Ok(RunSummary::skipped(
                "PR closed while reviewing",
                wall,
                telemetry,
            ));
        }
        let url = forge.upsert_comment(&pack, &body).await?;
        println!("posted: {url}");
    }

    Ok(report.finish(
        if args.dry_run { "dry-run" } else { "posted" },
        wall,
        telemetry,
    ))
}

pub async fn run_local(
    local: &crate::local::LocalReview,
    llm: &Llm,
    cfg: &Config,
    telemetry: &Telemetry,
    force: bool,
) -> Result<RunSummary> {
    let wall = Instant::now();
    let args = ReviewArgs {
        dry_run: true,
        force,
    };
    match analyze(&local.pack, local, llm, cfg, telemetry, &args, wall).await? {
        Analysis::Skipped(summary) => Ok(summary),
        Analysis::Reviewed(report) => {
            println!("{}", compose::render_local(local, &report));
            Ok(report.finish("local", wall, telemetry))
        }
    }
}

pub struct ReviewReport {
    pub confirmed: Vec<Confirmed>,
    pub minor: Vec<Confirmed>,
    pub summary: RunSummary,
}

impl ReviewReport {
    fn finish(mut self, outcome: &'static str, wall: Instant, telemetry: &Telemetry) -> RunSummary {
        self.summary.outcome = outcome;
        self.summary.confirmed = self.confirmed.len();
        self.summary.minor = self.minor.len();
        self.summary.duration = wall.elapsed();
        self.summary.usage = telemetry.totals();
        println!(
            "{} confirmed, {} minor / {} candidates in {:.1}s",
            self.summary.confirmed,
            self.summary.minor,
            self.summary.candidates,
            self.summary.duration.as_secs_f32()
        );
        print!("{}", telemetry.report(self.summary.duration));
        self.summary
    }
}

enum Analysis {
    Skipped(RunSummary),
    Reviewed(ReviewReport),
}

async fn analyze<S: verify::Source>(
    pack: &crate::pack::ContextPack,
    source: &S,
    llm: &Llm,
    cfg: &Config,
    telemetry: &Telemetry,
    args: &ReviewArgs,
    wall: Instant,
) -> Result<Analysis> {
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
            return Ok(Analysis::Skipped(RunSummary::skipped(
                &format!("PR is {}", pack.state),
                wall,
                telemetry,
            )));
        }
        if pack.draft {
            println!("skipped: PR is a draft");
            return Ok(Analysis::Skipped(RunSummary::skipped(
                "draft", wall, telemetry,
            )));
        }
        // Hard pre-model rule: bot-authored PRs (dependabot batches would
        // otherwise burn a review each). GREYBEARD_REVIEW_BOT_PRS=true or
        // --force to override.
        if pack.author_is_bot && !cfg.review_bot_prs {
            println!("skipped: bot-authored PR ({})", pack.author);
            return Ok(Analysis::Skipped(RunSummary::skipped(
                &format!("bot-authored PR ({})", pack.author),
                wall,
                telemetry,
            )));
        }
        // Skip only if a *completed* review of this exact head already exists.
        // A degraded/incomplete prior review of the same head must be retried,
        // not treated as done.
        if let Some(ec) = &pack.existing_comment {
            if ec.already_covers(&pack.head_sha) {
                println!("skipped: already reviewed {}", &pack.head_sha[..7]);
                return Ok(Analysis::Skipped(RunSummary::skipped(
                    "already reviewed this head",
                    wall,
                    telemetry,
                )));
            }
        }
    }
    if pack.changed_files.is_empty() {
        println!("skipped: no changed files");
        return Ok(Analysis::Skipped(RunSummary::skipped(
            "no changed files",
            wall,
            telemetry,
        )));
    }

    if !args.force {
        let mut limits = cfg.clone();
        limits.max_pack_chars = 16_000;
        limits.max_diff_chars = 12_000;
        let system = prompts::system_blocks(&crate::pack::render(&pack.source, &limits));
        let eligibility = llm
            .structured::<Eligibility>(
                Tier::Verify,
                "eligibility",
                &system,
                &prompts::eligibility_user_message(),
                &prompts::eligibility_schema(),
            )
            .await;
        if let Ok(e) = eligibility {
            if e.skip {
                println!("skipped: {}", e.reason);
                return Ok(Analysis::Skipped(RunSummary::skipped(
                    &e.reason, wall, telemetry,
                )));
            }
        }
    }

    let jobs = super::discovery::jobs(pack, cfg);
    let lenses_run = jobs.len();
    eprintln!(
        "greybeard: reviewing {} files in {} focused review calls",
        pack.changed_files.len(),
        lenses_run
    );
    let lenses_failed = std::sync::atomic::AtomicUsize::new(0);
    let lens_chains = jobs.iter().enumerate().map(|(index, job)| {
        let lens = job.key.as_str();
        let system = prompts::system_blocks(&job.context);
        let lenses_failed = &lenses_failed;
        async move {
            let report = llm
                .structured::<LensReport>(
                    Tier::Lens,
                    &format!("lens:{}:{}", lens, index + 1),
                    &system,
                    &prompts::discovery_user_message(&job.instruction),
                    &prompts::findings_schema(),
                )
                .await;
            let findings = match report {
                Ok(r) => r.findings,
                Err(e) => {
                    eprintln!("greybeard: lens {} dropped: {e}", lens);
                    lenses_failed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    return Vec::new();
                }
            };
            let mut candidates = findings;
            let overflow = if candidates.len() > MAX_FINDINGS_PER_LENS {
                candidates.split_off(MAX_FINDINGS_PER_LENS)
            } else {
                Vec::new()
            };
            eprintln!(
                "greybeard: lens {}/{} ({}) returned {} candidates",
                index + 1,
                lenses_run,
                lens,
                candidates.len()
            );
            let verdicts = join_all(
                candidates
                    .iter()
                    .map(|f| verify::verify(source, pack, llm, cfg, lens, f)),
            )
            .await;
            candidates
                .into_iter()
                .zip(verdicts)
                .chain(overflow.into_iter().map(|finding| {
                    (
                        finding,
                        Err(anyhow::anyhow!(
                            "discovery batch exceeded its verification budget"
                        )),
                    )
                }))
                .collect::<Vec<_>>()
        }
    });
    let results: Vec<Vec<_>> = join_all(lens_chains).await;
    // All lens tasks are done — collapse the atomic to a plain count used both
    // for the rendered comment (coverage warning) and the run summary.
    let lenses_failed = lenses_failed.into_inner();

    let mut candidates_total = 0usize;
    let mut unverified = 0usize;
    let mut confirmed: Vec<Confirmed> = Vec::new();
    let mut minor: Vec<Confirmed> = Vec::new();
    for (job, chain) in jobs.iter().zip(results) {
        let lens = job.key.as_str();
        for (mut finding, verdict) in chain {
            candidates_total += 1;
            match verdict {
                Ok(v) if v.status == VerdictStatus::Confirmed => {
                    finding.line = Some(v.citations[0].line);
                    finding.severity = v.severity.clone();
                    finding.evidence = verify::evidence(&v);
                    let destination = if v.severity == "nit" {
                        &mut minor
                    } else {
                        &mut confirmed
                    };
                    destination.push(Confirmed {
                        finding,
                        lens: lens.to_string(),
                        confidence: v.confidence,
                    });
                }
                Ok(v) if v.status == VerdictStatus::Refuted => {
                    eprintln!(
                        "greybeard: refuted {} ({}): {}",
                        finding_summary(&finding.file, finding.line),
                        lens,
                        verify::evidence(&v)
                    );
                }
                Ok(v) => {
                    unverified += 1;
                    eprintln!(
                        "greybeard: UNVERIFIED {} ({}): {}",
                        finding_summary(&finding.file, finding.line),
                        lens,
                        v.reason
                    );
                }
                // A dead verifier must degrade LOUDLY, never silently drop
                // findings.
                Err(e) => {
                    unverified += 1;
                    eprintln!(
                        "greybeard: UNVERIFIED {} ({}): {e}",
                        finding_summary(&finding.file, finding.line),
                        lens
                    );
                }
            }
        }
    }

    // ── Stage 3: dedupe, render, post (update in place) ─────────────────────
    let confirmed = compose::dedupe(confirmed);
    let minor = compose::dedupe_minor(minor, &confirmed);
    Ok(Analysis::Reviewed(ReviewReport {
        confirmed,
        minor,
        summary: RunSummary {
            candidates: candidates_total,
            unverified,
            lenses_failed,
            lenses_run,
            ..RunSummary::default()
        },
    }))
}
