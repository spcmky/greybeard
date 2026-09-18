mod support;

use greybeard::pipeline::{verify, Finding, VerdictStatus};
use serde_json::json;

fn finding() -> Finding {
    Finding {
        file: "stats.py".into(),
        line: Some(2),
        claim: "The arithmetic mean uses an incorrect denominator".into(),
        severity: "gap".into(),
        evidence: "The denominator subtracts one".into(),
    }
}

#[tokio::test]
async fn verifier_accepts_a_source_supported_counterexample() {
    let dir = support::repository(&[(
        "stats.py",
        "def average(values):\n    return sum(values) / (len(values) - 1)\n",
    )]);
    let local = support::snapshot(&dir);
    let model = support::model(|_| {
        let mut verdict = support::confirmed();
        verdict["citations"][0]["line"] = json!(1);
        verdict["citations"][0]["quote"] =
            json!("def average(values):\n\treturn sum(values) / (len(values) - 1)");
        verdict
    })
    .await;
    let mut candidate = finding();
    candidate.line = None;
    let verdict = verify::verify(
        &local,
        &local.pack,
        &model.llm,
        &model.cfg,
        "bugs",
        &candidate,
    )
    .await
    .unwrap();
    assert_eq!(verdict.status, VerdictStatus::Confirmed);
    assert!(verify::evidence(&verdict).contains("average([2, 4])"));
    assert_eq!(
        verdict.citations[0].quote,
        "def average(values):\n    return sum(values) / (len(values) - 1)"
    );
}

#[tokio::test]
async fn unsupported_claims_cannot_be_confirmed_or_demoted_to_minor() {
    let dir = support::repository(&[(
        "stats.py",
        "def average(values):\n    return sum(values) / (len(values) - 1)\n",
    )]);
    let local = support::snapshot(&dir);
    for defect in ["quote", "unread", "line", "trigger", "confidence"] {
        let model = support::model(move |_| {
            let mut verdict = support::confirmed();
            match defect {
                "quote" => {
                    verdict["citations"][0]["quote"] = json!("return something_else(values)")
                }
                "unread" => verdict["citations"][0]["file"] = json!("not-read.py"),
                "line" => verdict["citations"][0]["line"] = json!(99),
                "trigger" => verdict["trigger"] = json!(""),
                "confidence" => verdict["confidence"] = json!(60),
                _ => unreachable!(),
            }
            verdict
        })
        .await;
        let result = verify::verify(
            &local,
            &local.pack,
            &model.llm,
            &model.cfg,
            "bugs",
            &finding(),
        )
        .await;
        if defect == "confidence" {
            assert_eq!(result.unwrap().status, VerdictStatus::Unverified);
        } else {
            assert!(result.is_err(), "accepted {defect}");
        }
    }
}

#[tokio::test]
async fn removed_code_is_not_current_evidence() {
    let dir = support::repository(&[(
        "stats.py",
        "def average(values):\n    return sum(values) / (len(values) - 1)\n",
    )]);
    support::git(dir.path(), &["add", "stats.py"]);
    support::git(dir.path(), &["commit", "-m", "old behavior"]);
    support::write(
        dir.path(),
        "stats.py",
        "def average(values):\n    return sum(values) / len(values)\n",
    );
    let local = support::snapshot(&dir);
    let model = support::model(|_| support::confirmed()).await;
    assert!(verify::verify(
        &local,
        &local.pack,
        &model.llm,
        &model.cfg,
        "bugs",
        &finding()
    )
    .await
    .is_err());
}

#[tokio::test]
async fn requested_dependencies_use_the_reviewed_revision() {
    let dir = support::repository(&[(
        "helpers.py",
        "def denominator(values):\n    return len(values) - 1\n",
    )]);
    support::git(dir.path(), &["add", "helpers.py"]);
    support::git(dir.path(), &["commit", "-m", "helper"]);
    support::write(
        dir.path(),
        "stats.py",
        "def average(values):\n    return sum(values) / denominator(values)\n",
    );
    let local = support::snapshot(&dir);
    support::write(
        dir.path(),
        "helpers.py",
        "def denominator(values):\n    return len(values)\n",
    );
    let model = support::model(|request| {
        let context = request["messages"][0]["content"].as_str().unwrap();
        let mut verdict = support::confirmed();
        if !context.contains("return len(values) - 1") {
            verdict["status"] = json!("unverified");
            verdict["requested_files"] = json!(["helpers.py"]);
        } else {
            verdict["citations"] = json!([
                {"file": "stats.py", "line": 2, "quote": "    return sum(values) / denominator(values)"},
                {"file": "helpers.py", "line": 2, "quote": "    return len(values) - 1"}
            ]);
        }
        verdict
    }).await;
    let verdict = verify::verify(
        &local,
        &local.pack,
        &model.llm,
        &model.cfg,
        "bugs",
        &finding(),
    )
    .await
    .unwrap();
    assert_eq!(verdict.status, VerdictStatus::Confirmed);
}

#[tokio::test]
async fn ambiguous_dependency_paths_cannot_bypass_the_snapshot() {
    let dir = support::repository(&[(
        "stats.py",
        "def average(values):\n    return sum(values) / denominator(values)\n",
    )]);
    let local = support::snapshot(&dir);
    for path in [
        "nested//helpers.py",
        "./helpers.py",
        "nested/../helpers.py",
        ".git/config",
    ] {
        let model = support::model(move |_| {
            let mut verdict = support::confirmed();
            verdict["status"] = json!("unverified");
            verdict["requested_files"] = json!([path]);
            verdict
        })
        .await;
        assert!(verify::verify(
            &local,
            &local.pack,
            &model.llm,
            &model.cfg,
            "bugs",
            &finding()
        )
        .await
        .is_err());
    }
}

#[tokio::test]
async fn an_unresolved_dependency_stays_unverified() {
    let dir = support::repository(&[(
        "stats.py",
        "def average(values):\n    return sum(values) / denominator(values)\n",
    )]);
    let local = support::snapshot(&dir);
    let model = support::model(|_| {
        let mut verdict = support::confirmed();
        verdict["status"] = json!("unverified");
        verdict["reason"] = json!("The external denominator implementation is unavailable");
        verdict["citations"] = json!([]);
        verdict
    })
    .await;
    let verdict = verify::verify(
        &local,
        &local.pack,
        &model.llm,
        &model.cfg,
        "bugs",
        &finding(),
    )
    .await
    .unwrap();
    assert_eq!(verdict.status, VerdictStatus::Unverified);
}

#[tokio::test]
async fn verification_reads_source_beyond_discovery_limits() {
    let dir = support::repository(&[]);
    for i in 0..65 {
        support::write(dir.path(), &format!("a{i}.py"), "pass\n");
    }
    support::write(
        dir.path(),
        "stats.py",
        &format!(
            "{}def average(values):\n    return sum(values) / (len(values) - 1)\n",
            "\n".repeat(2_000)
        ),
    );
    let local = support::snapshot(&dir);
    let model = support::model(|_| {
        let mut verdict = support::confirmed();
        verdict["citations"][0]["line"] = json!(2002);
        verdict
    })
    .await;
    let mut candidate = finding();
    candidate.line = Some(2002);
    let verdict = verify::verify(
        &local,
        &local.pack,
        &model.llm,
        &model.cfg,
        "bugs",
        &candidate,
    )
    .await
    .unwrap();
    assert_eq!(verdict.status, VerdictStatus::Confirmed);
}

#[tokio::test]
async fn excess_candidates_make_the_review_incomplete() {
    let dir = support::repository(&[]);
    for i in 0..12 {
        support::write(
            dir.path(),
            &format!("stats{i}.py"),
            "def average(values):\n    return sum(values) / (len(values) - 1)\n",
        );
    }
    let local = support::snapshot(&dir);
    let model = support::model(|request| {
        if request["response_format"]["json_schema"]["schema"]["properties"]
            .get("findings")
            .is_some()
        {
            return json!({"findings": (0..12).map(|i| {
                let mut candidate = finding();
                candidate.file = format!("stats{i}.py");
                candidate
            }).collect::<Vec<_>>()});
        }
        let prompt = request["messages"][1]["content"].as_str().unwrap();
        let path = (0..12)
            .map(|i| format!("stats{i}.py"))
            .find(|path| prompt.contains(path))
            .unwrap();
        let mut verdict = support::confirmed();
        verdict["citations"][0]["file"] = json!(path);
        verdict
    })
    .await;
    let summary = greybeard::pipeline::review::run_local(
        &local,
        &model.llm,
        &model.cfg,
        &greybeard::telemetry::Telemetry::new(),
        true,
    )
    .await
    .unwrap();
    assert_eq!(summary.candidates, 12);
    assert!(summary.unverified > 0);
}

#[tokio::test]
#[ignore = "requires an explicitly configured live local model; evaluates real and false findings"]
async fn local_model_rejects_false_positives_and_detects_mutants() -> anyhow::Result<()> {
    use greybeard::{
        config::{Config, Provider},
        llm::Llm,
        telemetry::Telemetry,
    };
    let cfg = Config::from_env()?;
    assert_eq!(cfg.provider, Provider::OpenAi);
    let telemetry = Telemetry::new();
    let llm = Llm::new(cfg.clone(), telemetry.clone()).await?;
    let started = std::time::Instant::now();
    let fixtures = [
        ("AGENTS.md", include_str!("fixtures/review/AGENTS.md")),
        (
            "reliquary/metadata_cache.go",
            include_str!("fixtures/review/reliquary/metadata_cache.go"),
        ),
        (
            "reliquary/cache/lru.go",
            include_str!("fixtures/review/reliquary/cache/lru.go"),
        ),
        (
            "reliquary/cache/lfu.go",
            include_str!("fixtures/review/reliquary/cache/lfu.go"),
        ),
        (
            "reliquary/cache/cache.go",
            include_str!("fixtures/review/reliquary/cache/cache.go"),
        ),
        (
            "reliquary/sshstream/chunk_cache.go",
            include_str!("fixtures/review/reliquary/sshstream/chunk_cache.go"),
        ),
        (
            "reliquary/sshstream/manager.go",
            include_str!("fixtures/review/reliquary/sshstream/manager.go"),
        ),
        (
            "reliquary/sshstream/host_manager.go",
            include_str!("fixtures/review/reliquary/sshstream/host_manager.go"),
        ),
        (
            "reliquary/sshstream/connection_pool.go",
            include_str!("fixtures/review/reliquary/sshstream/connection_pool.go"),
        ),
        (
            "reliquary/sshstream/options.go",
            include_str!("fixtures/review/reliquary/sshstream/options.go"),
        ),
        (
            "internal/tint/handler_test.go",
            include_str!("fixtures/review/internal/tint/handler_test.go"),
        ),
    ];
    let cases = [
        ("metadata", "reliquary/metadata_cache.go", "if c.generation == generation", "An older in-flight metadata load can repopulate the cache after invalidation", "bugs"),
        ("ssh", "reliquary/sshstream/chunk_cache.go", "namespace: fmt.Sprintf", "Different authenticated pools using a shared backend can reuse each other's cached chunks", "bugs"),
        ("lfu", "reliquary/cache/lfu.go", "func (c *LFUCache) evictLFU", "Eviction can panic when the only remaining item is keep and capacity is exceeded", "bugs"),
        ("context", "internal/tint/handler_test.go", "slog.LevelInfo+1,", "The test uses context.TODO() instead of the required t.Context()", "claude-md"),
    ];
    let mut mismatches = Vec::new();
    for broken in [false, true] {
        for (name, path, needle, claim, lens) in cases {
            let dir = support::repository(&fixtures);
            let original = std::fs::read_to_string(dir.path().join(path))?;
            let code = if broken {
                match name {
                    "metadata" => original.replace("c.generation++", ""),
                    "ssh" => original.replace("\t\"crypto/rand\"\n", "").replace("rand.Text()", "\"shared\""),
                    "lfu" => original.replace("\tif dataCost > c.maxBytes {\n\t\t// Item is too large for cache\n\t\treturn\n\t}\n", ""),
                    "context" => original.replacen("l.Log(t.Context(), slog.LevelInfo+1,", "l.Log(context.TODO(), slog.LevelInfo+1,", 1),
                    _ => unreachable!(),
                }
            } else {
                original
            };
            support::write(dir.path(), path, &code);
            let line = code.lines().position(|line| line.contains(needle)).unwrap() as u32 + 1;
            let finding = Finding {
                file: path.into(),
                line: Some(line),
                claim: claim.into(),
                severity: if name == "context" { "nit" } else { "blocker" }.into(),
                evidence: String::new(),
            };
            let local = support::snapshot(&dir);
            let result = verify::verify(&local, &local.pack, &llm, &cfg, lens, &finding).await;
            let expected = if broken {
                VerdictStatus::Confirmed
            } else {
                VerdictStatus::Refuted
            };
            match result {
                Ok(verdict) => {
                    eprintln!(
                        "CASE {name} broken={broken}: {:?}: {}",
                        verdict.status,
                        verify::evidence(&verdict)
                    );
                    if verdict.status != expected {
                        mismatches.push(format!("{name} broken={broken}: {:?}", verdict.status));
                    }
                }
                Err(error) => {
                    eprintln!("CASE {name} broken={broken}: ERROR {error}");
                    mismatches.push(format!("{name} broken={broken}: {error}"));
                }
            }
        }
    }
    eprintln!("{}", telemetry.report(started.elapsed()));
    assert!(
        mismatches.is_empty(),
        "evaluation failures: {mismatches:#?}"
    );
    Ok(())
}
