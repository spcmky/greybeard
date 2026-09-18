//! Opt-in local model check; uses the current Greybeard environment and never
//! connects to a forge or posts a review. See docs/SETUP.md.
use greybeard::config::{Config, Provider};
use greybeard::llm::{Llm, Tier};
use greybeard::pipeline::{Eligibility, LensReport, Verdict, VerdictStatus};
use greybeard::prompts;
use greybeard::telemetry::Telemetry;

#[tokio::test]
#[ignore = "requires an explicitly configured live OpenAI-compatible model server"]
async fn local_model_review_schemas() -> anyhow::Result<()> {
    let cfg = Config::from_env()?;
    assert_eq!(cfg.provider, Provider::OpenAi);
    let telemetry = Telemetry::new();
    let llm = Llm::new(cfg, telemetry.clone()).await?;
    let started = std::time::Instant::now();
    let system = prompts::system_blocks(
        r#"
<pull_request>
title: Add average helper
state: open
author: developer (human)
description: Add a helper to compute the arithmetic mean of a list of numbers.
</pull_request>
<diff>
diff --git a/stats.py b/stats.py
new file mode 100644
--- /dev/null
+++ b/stats.py
@@ -0,0 +1,2 @@
+def average(values):
+    return sum(values) / (len(values) - 1)
</diff>
<file path="stats.py">
1: def average(values):
2:     return sum(values) / (len(values) - 1)
</file>
No CLAUDE.md, CI config changes, blame, or previous review comments.
"#,
    );
    llm.warm(Tier::Lens, &system).await;
    let eligibility: Eligibility = llm
        .structured(
            Tier::Verify,
            "eligibility",
            &system,
            &prompts::eligibility_user_message(),
            &prompts::eligibility_schema(),
        )
        .await?;
    assert!(
        !eligibility.skip,
        "substantive code change was skipped: {}",
        eligibility.reason
    );
    // Exercise all lens instructions against the configured provider.
    let reports = futures::future::join_all(prompts::LENSES.iter().map(|lens| {
        let llm = &llm;
        let system = &system;
        async move {
            let report: LensReport = llm
                .structured(
                    Tier::Lens,
                    &format!("lens:{}", lens.key),
                    system,
                    &prompts::lens_user_message(lens),
                    &prompts::findings_schema(),
                )
                .await?;
            anyhow::Ok((lens.key, report))
        }
    }))
    .await;
    let reports: Vec<_> = reports.into_iter().collect::<anyhow::Result<_>>()?;
    let bugs = &reports.iter().find(|(key, _)| *key == "bugs").unwrap().1;
    let finding = bugs
        .findings
        .iter()
        .find(|finding| finding.file == "stats.py")
        .expect("bugs lens should catch the incorrect denominator");
    let verdict: Verdict = llm
        .structured(
            Tier::Verify,
            "verify:stats.py",
            &system,
            &prompts::verify_user_message(
                "bugs",
                &finding.file,
                finding.line,
                &finding.severity,
                &finding.claim,
                &finding.evidence,
            ),
            &prompts::verdict_schema(),
        )
        .await?;
    assert!(
        verdict.status == VerdictStatus::Confirmed,
        "verifier rejected the denominator bug: {}",
        verdict.reason
    );
    assert!(verdict.confidence <= 100);
    eprintln!(
        "Verified finding: {} (confidence {})",
        finding.claim, verdict.confidence
    );
    eprintln!("{}", telemetry.report(started.elapsed()));
    assert!(telemetry.totals().output_tokens > 0);
    Ok(())
}
