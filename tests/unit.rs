use greybeard::github::comment::{parse_marker, permalink, render_marker};
use greybeard::github::pack::{cap_lines, parse_diff_ranges};
use greybeard::github::PrRef;
use greybeard::llm::parse_json_object;
use greybeard::pipeline::compose::{dedupe, render_comment};
use greybeard::pipeline::{Confirmed, Finding, LensReport};

fn pr() -> PrRef {
    PrRef::parse("https://github.com/REI-Labs/nexus-core/pull/123").unwrap()
}

fn finding(file: &str, line: Option<u32>, severity: &str) -> Finding {
    Finding {
        file: file.into(),
        line,
        claim: "claim".into(),
        severity: severity.into(),
        evidence: "evidence".into(),
    }
}

#[test]
fn pr_url_parses_and_rejects() {
    let p = pr();
    assert_eq!((p.owner.as_str(), p.repo.as_str(), p.number), ("REI-Labs", "nexus-core", 123));
    assert!(PrRef::parse("https://github.com/REI-Labs/nexus-core/issues/9").is_err());
    assert!(PrRef::parse("https://gitlab.com/x/y/pull/1").is_err());
}

#[test]
fn marker_roundtrip() {
    let sha = "0123456789abcdef0123456789abcdef01234567";
    let marker = render_marker(sha);
    assert!(marker.starts_with("<!-- greybeard:"));
    assert_eq!(parse_marker(&format!("## review\nbody text\n{marker}\n")), Some(sha.to_string()));
    assert_eq!(parse_marker("no marker here"), None);
}

#[test]
fn permalink_full_sha_with_context_lines() {
    let sha = "0123456789abcdef0123456789abcdef01234567";
    let link = permalink(&pr(), sha, "backend/api/foo.py", Some(42));
    assert_eq!(
        link,
        format!("https://github.com/REI-Labs/nexus-core/blob/{sha}/backend/api/foo.py#L41-L43")
    );
    // Line 1 must not produce L0.
    assert!(permalink(&pr(), sha, "f.py", Some(1)).ends_with("#L1-L2"));
    // No line → plain file link.
    assert!(!permalink(&pr(), sha, "f.py", None).contains('#'));
}

#[test]
fn diff_ranges_parse_new_side_hunks() {
    let diff = "\
diff --git a/src/a.rs b/src/a.rs
--- a/src/a.rs
+++ b/src/a.rs
@@ -10,3 +12,4 @@ fn x() {
 ctx
+added
 ctx
@@ -30,2 +40,2 @@
 ctx
diff --git a/gone.rs b/gone.rs
--- a/gone.rs
+++ /dev/null
@@ -1,5 +0,0 @@
";
    let ranges = parse_diff_ranges(diff);
    assert_eq!(ranges.len(), 1);
    assert_eq!(ranges[0].0, "src/a.rs");
    assert_eq!(ranges[0].1, vec![(12, 15), (40, 41)]);
}

#[test]
fn cap_lines_truncates_with_note() {
    let text = (1..=10).map(|i| format!("line{i}")).collect::<Vec<_>>().join("\n");
    assert_eq!(cap_lines(&text, 20), text);
    let capped = cap_lines(&text, 3);
    assert!(capped.contains("line3"));
    assert!(!capped.contains("line4\n"));
    assert!(capped.contains("truncated: 3 of 10"));
}

#[test]
fn json_extraction_tolerates_fences_and_prose() {
    let messy = "Here you go:\n```json\n{\"findings\": [{\"file\": \"a.py\", \"claim\": \"c\", \"severity\": \"gap\", \"evidence\": \"e\"}]}\n```\nDone.";
    let report: LensReport = parse_json_object(messy).unwrap();
    assert_eq!(report.findings.len(), 1);
    assert_eq!(report.findings[0].line, None);
    assert!(parse_json_object::<LensReport>("no json at all").is_err());
}

#[test]
fn dedupe_merges_nearby_and_keeps_highest_severity() {
    let confirmed = vec![
        Confirmed { finding: finding("a.py", Some(10), "nit"), lens: "bugs".into(), confidence: 85 },
        Confirmed { finding: finding("a.py", Some(12), "blocker"), lens: "history".into(), confidence: 90 },
        Confirmed { finding: finding("a.py", Some(50), "gap"), lens: "bugs".into(), confidence: 88 },
        Confirmed { finding: finding("b.py", Some(12), "gap"), lens: "bugs".into(), confidence: 88 },
    ];
    let kept = dedupe(confirmed);
    assert_eq!(kept.len(), 3);
    // The a.py:10/12 pair collapses into the blocker, crediting both lenses.
    let merged = kept.iter().find(|c| c.finding.file == "a.py" && c.finding.severity == "blocker").unwrap();
    assert!(merged.lens.contains("history") && merged.lens.contains("bugs"));
}

#[test]
fn comment_render_no_findings_and_marker() {
    let sha = "0123456789abcdef0123456789abcdef01234567";
    let body = render_comment(greybeard::config::Forge::GitHub, None, &pr(), sha, &[], &[], 4, 6, 0);
    assert!(body.contains("No issues found"));
    assert!(body.contains("_You shall pass._"));
    assert!(body.contains("reviewed 0123456"));
    assert!(body.contains("[bugs/ideas](https://github.com/REI-Labs/greybeard/issues)"));
    assert_eq!(parse_marker(&body), Some(sha.to_string()));
    // No AI attribution: referencing the CLAUDE.md *file* is fine, but no
    // generated-with footers, bot emoji, or vendor credits.
    let lower = body.to_lowercase();
    for banned in ["generated with", "claude code", "anthropic", "🤖"] {
        assert!(!lower.contains(banned), "attribution marker found: {banned}");
    }
}

#[test]
fn comment_render_findings_are_numbered_with_permalinks() {
    let sha = "0123456789abcdef0123456789abcdef01234567";
    let confirmed = vec![Confirmed {
        finding: finding("src/x.rs", Some(7), "blocker"),
        lens: "bugs".into(),
        confidence: 95,
    }];
    let body = render_comment(greybeard::config::Forge::GitHub, None, &pr(), sha, &confirmed, &[], 3, 6, 0);
    assert!(body.contains("Found 1 issue:"));
    assert!(body.contains(&format!("blob/{sha}/src/x.rs#L6-L8")));
    assert!(body.contains("1 confirmed"));
    assert!(body.contains("_You shall not pass._"), "blocker gets the hard verdict line");
}

#[test]
fn minor_notes_render_collapsed_with_crack_verdict() {
    let sha = "0123456789abcdef0123456789abcdef01234567";
    let minor = vec![Confirmed {
        finding: finding("src/a.rs", Some(9), "nit"),
        lens: "code-comments".into(),
        confidence: 65,
    }];
    let body = render_comment(greybeard::config::Forge::GitHub, None, &pr(), sha, &[], &minor, 2, 6, 0);
    assert!(body.contains("No blocking issues found"));
    assert!(body.contains("<details>"));
    assert!(body.contains("Minor notes (1)"));
    assert!(body.contains(&format!("blob/{sha}/src/a.rs#L8-L10")));
    assert!(body.contains("_Pass — but mind the cracks in the bridge._"));
    assert!(body.contains("0 confirmed, 1 minor"));
}

#[test]
fn minor_colliding_with_confirmed_is_dropped() {
    use greybeard::pipeline::compose::dedupe_minor;
    let confirmed = vec![Confirmed {
        finding: finding("src/a.rs", Some(10), "blocker"),
        lens: "bugs".into(),
        confidence: 95,
    }];
    let minor = vec![
        Confirmed { finding: finding("src/a.rs", Some(11), "nit"), lens: "history".into(), confidence: 66 },
        Confirmed { finding: finding("src/b.rs", Some(11), "nit"), lens: "history".into(), confidence: 66 },
    ];
    let kept = dedupe_minor(minor, &confirmed);
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].finding.file, "src/b.rs");
}

#[test]
fn degraded_run_suppresses_verdict_and_warns() {
    let sha = "0123456789abcdef0123456789abcdef01234567";
    let body = render_comment(greybeard::config::Forge::GitHub, None, &pr(), sha, &[], &[], 5, 6, 3);
    assert!(body.contains("Verification degraded"));
    assert!(body.contains("3 candidate findings"));
    assert!(!body.contains("You shall pass"), "no verdict line while degraded");
}

#[test]
fn verdict_line_severity_tiers() {
    let sha = "0123456789abcdef0123456789abcdef01234567";
    let gap_only = vec![Confirmed {
        finding: finding("src/x.rs", Some(7), "gap"),
        lens: "bugs".into(),
        confidence: 90,
    }];
    let body = render_comment(greybeard::config::Forge::GitHub, None, &pr(), sha, &gap_only, &[], 1, 6, 0);
    assert!(body.contains("_Pass — but mind the cracks in the bridge._"));
    assert!(!body.contains("_You shall not pass._"));
}

#[test]
fn hmac_sha256_rfc4231_vector() {
    use greybeard::server::{hmac_sha256, verify_signature};
    // RFC 4231 test case 2: key "Jefe", data "what do ya want for nothing?"
    let mac = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
    let hex: String = mac.iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(hex, "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843");

    let header = format!("sha256={hex}");
    assert!(verify_signature("Jefe", b"what do ya want for nothing?", &header));
    assert!(!verify_signature("Jefe", b"tampered body", &header));
    assert!(!verify_signature("wrong-key", b"what do ya want for nothing?", &header));
    assert!(!verify_signature("Jefe", b"what do ya want for nothing?", "sha256=zz"));
    assert!(!verify_signature("Jefe", b"what do ya want for nothing?", "sha1=abcd"));
}

#[test]
fn generated_file_detection() {
    use greybeard::github::pack::is_generated;
    for p in ["Cargo.lock", "web/package-lock.json", "dist/app.min.js", "src/vendor/lib.js", "a/b/go.sum"] {
        assert!(is_generated(p), "{p} should be generated");
    }
    for p in ["src/main.rs", "Cargo.toml", "docs/lockfile-guide.md", "distributed/notes.md"] {
        assert!(!is_generated(p), "{p} should NOT be generated");
    }
}

#[test]
fn diff_budget_stubs_generated_then_largest() {
    use greybeard::github::pack::budget_diff;
    let lock_body = "+lock\n".repeat(400);
    let big_body = "+code\n".repeat(300);
    let small_body = "+ok\n".repeat(5);
    let diff = format!(
        "diff --git a/Cargo.lock b/Cargo.lock\n{lock_body}diff --git a/src/big.rs b/src/big.rs\n{big_body}diff --git a/src/small.rs b/src/small.rs\n{small_body}"
    );
    // Under budget: untouched.
    assert_eq!(budget_diff(&diff, diff.len() + 1), diff);
    // Over budget: the lockfile is stubbed first…
    let capped = budget_diff(&diff, diff.len() - 100);
    assert!(capped.contains("generated file"));
    assert!(capped.contains("+code"));
    // …and order is preserved (lock stub before big.rs content).
    assert!(capped.find("Cargo.lock").unwrap() < capped.find("src/big.rs").unwrap());
    // Tight budget: big.rs goes too, small.rs survives.
    let tight = budget_diff(&diff, small_body.len() + 400);
    assert!(tight.contains("diff budget"));
    assert!(tight.contains("+ok"));
    assert!(!tight.contains("+code"));
}

#[test]
fn marker_v2_carries_findings_and_stays_parseable() {
    let sha = "0123456789abcdef0123456789abcdef01234567";
    // Hostile claim: `-->` must not terminate the HTML marker early.
    let long_claim = format!("a --> b <!-- c {}", "x".repeat(300));
    let mut confirmed = vec![Confirmed {
        finding: Finding { file: "src/a.rs".into(), line: Some(7), claim: long_claim, severity: "blocker".into(), evidence: "e".into() },
        lens: "bugs".into(),
        confidence: 95,
    }];
    // 25 minor findings — payload must cap at 20 total.
    let minor: Vec<Confirmed> = (0..25).map(|i| Confirmed {
        finding: finding(&format!("src/m{i}.rs"), Some(1), "nit"),
        lens: "history".into(),
        confidence: 60 + (i % 20) as u8,
    }).collect();
    let body = render_comment(greybeard::config::Forge::GitHub, None, &pr(), sha, &confirmed, &minor, 30, 6, 0);

    // v1-compatible sha extraction still works on a v2 marker.
    assert_eq!(parse_marker(&body), Some(sha.to_string()));

    // Parse the full payload the way the loop skill does.
    let start = body.find("<!-- greybeard:").unwrap() + "<!-- greybeard:".len();
    let end = body[start..].find("-->").unwrap() + start;
    let payload: serde_json::Value = serde_json::from_str(body[start..end].trim()).unwrap();
    assert_eq!(payload["v"], 2);
    assert_eq!(payload["verdict"], "blocked");
    let findings = payload["findings"].as_array().unwrap();
    assert_eq!(findings.len(), 20, "capped at 20");
    assert_eq!(findings[0]["band"], "confirmed");
    let claim = findings[0]["claim"].as_str().unwrap();
    assert_eq!(claim.chars().count(), 200, "claim truncated");
    assert!(claim.contains("-->") && claim.contains("<!--"), "hostile chars round-trip");
    assert_eq!(findings[1]["band"], "minor");

    // Clean review → verdict pass, empty findings.
    confirmed.clear();
    let clean = render_comment(greybeard::config::Forge::GitHub, None, &pr(), sha, &confirmed, &[], 0, 6, 0);
    let start = clean.find("<!-- greybeard:").unwrap() + "<!-- greybeard:".len();
    let end = clean[start..].find("-->").unwrap() + start;
    let p: serde_json::Value = serde_json::from_str(clean[start..end].trim()).unwrap();
    assert_eq!(p["verdict"], "pass");
    assert_eq!(p["findings"].as_array().unwrap().len(), 0);
}

#[test]
fn metrics_record_render_and_health_degraded() {
    use greybeard::llm::Usage;
    use greybeard::metrics::Metrics;
    let m = Metrics::new();
    let usage = Usage {
        input_tokens: 1000,
        output_tokens: 200,
        cache_creation_input_tokens: 50,
        cache_read_input_tokens: 5000,
    };
    m.record_run("posted", 1500, Some(&usage), Some(0.25));
    let text = m.render();
    assert!(text.contains("greybeard_reviews_total{outcome=\"posted\"} 1"));
    assert!(text.contains("greybeard_review_duration_seconds_sum 1.5"));
    assert!(text.contains("greybeard_tokens_total{kind=\"cache_read\"} 5000"));
    assert!(text.contains("greybeard_cost_microusd_total 250000"));
    assert_eq!(m.health_json("9.9.9")["status"], "ok");

    // Three consecutive failures flip health to degraded (still HTTP 200 —
    // asserted by the handler staying StatusCode::OK); a success resets.
    for _ in 0..3 {
        m.record_run("error", 100, None, None);
    }
    let h = m.health_json("9.9.9");
    assert_eq!(h["status"], "degraded");
    assert_eq!(h["consecutive_failures"], 3);
    assert!(h["last_error_unix"].is_u64());
    // A benign skip must NOT mask the streak (drafts don't prove the LLM
    // path works); only a completed run resets it.
    m.record_run("skipped", 100, None, None);
    assert_eq!(m.health_json("9.9.9")["status"], "degraded");
    m.record_run("posted", 100, None, None);
    assert_eq!(m.health_json("9.9.9")["status"], "ok");
    // daily-limit neither runs nor breaks the streak.
    m.record_run("daily-limit", 0, None, None);
    assert!(m.render().contains("greybeard_reviews_total{outcome=\"daily-limit\"} 1"));
    assert!(m.render().contains("greybeard_review_duration_seconds_count 6"));
}

#[test]
fn run_event_shape_and_single_line() {
    use greybeard::events::{render, RunEvent};
    use greybeard::llm::Usage;
    use greybeard::pipeline::review::RunSummary;
    let s = RunSummary {
        outcome: "posted",
        reason: None,
        confirmed: 2,
        minor: 1,
        candidates: 5,
        unverified: 0,
        lenses_failed: 1,
        duration: std::time::Duration::from_secs(90),
        usage: Usage { input_tokens: 7, output_tokens: 8, cache_creation_input_tokens: 9, cache_read_input_tokens: 10 },
    };
    let v = render(&RunEvent {
        pr: "REI-Labs/greybeard#1",
        action: "synchronize",
        force: false,
        outcome: "posted",
        error: None,
        duration_s: 92.34,
        summary: Some(&s),
        usage: &s.usage,
        cost_usd: None,
    });
    assert_eq!(v["evt"], "review.done");
    assert_eq!(v["duration_s"], 92.3);
    assert_eq!(v["confirmed"], 2);
    assert_eq!(v["tokens"]["cache_read"], 10);
    assert!(v["cost_usd"].is_null(), "no prices configured -> null, tokens still present");
    assert!(!v.to_string().contains('\n'), "must stay one Loki-friendly line");

    let e = render(&RunEvent {
        pr: "o/r#2",
        action: "opened",
        force: true,
        outcome: "error",
        error: Some("boom"),
        duration_s: 1.0,
        summary: None,
        usage: &Usage { input_tokens: 500_000, output_tokens: 40_000, ..Default::default() },
        cost_usd: Some(3.5),
    });
    assert_eq!(e["error"], "boom");
    // Failed runs still report what they burned.
    assert_eq!(e["tokens"]["input"], 500_000);
    assert_eq!(e["cost_usd"], 3.5);
}

#[test]
fn installation_id_from_payload() {
    use greybeard::server::installation_from_payload;
    let v = serde_json::json!({"installation": {"id": 154471131}, "repository": {"name": "x"}});
    assert_eq!(installation_from_payload(&v), Some(154471131));
    assert_eq!(installation_from_payload(&serde_json::json!({"action": "opened"})), None);
}

#[test]
fn price_cost_math() {
    use greybeard::config::Prices;
    use greybeard::llm::Usage;
    let p = Prices { input: 5.0, output: 25.0, cache_read: 0.5, cache_write: 6.25 };
    let u = Usage {
        input_tokens: 1_000_000,
        output_tokens: 100_000,
        cache_read_input_tokens: 2_000_000,
        cache_creation_input_tokens: 400_000,
    };
    assert!((p.cost_usd(&u) - 11.0).abs() < 1e-9);
}

#[test]
fn forge_parses_names_and_rejects_unknown() {
    use greybeard::config::Forge;
    assert_eq!(Forge::parse("github").unwrap(), Forge::GitHub);
    assert_eq!(Forge::parse("GitHub").unwrap(), Forge::GitHub);
    assert_eq!(Forge::parse("gh").unwrap(), Forge::GitHub);
    assert_eq!(Forge::parse("gitlab").unwrap(), Forge::GitLab);
    assert_eq!(Forge::parse(" GL ").unwrap(), Forge::GitLab);
    assert!(Forge::parse("bitbucket").is_err());
}

#[test]
fn ensure_supported_allows_both_forges() {
    use greybeard::config::Forge;
    use greybeard::forge::ensure_supported;
    // Both CLI review and serve mode support both forges now.
    assert!(ensure_supported(Forge::GitHub).is_ok());
    assert!(ensure_supported(Forge::GitLab).is_ok());
}

#[test]
fn gitlab_webhook_token_verify_is_constant_length_exact() {
    use greybeard::webhook::verify_gitlab_token;
    assert!(verify_gitlab_token("s3cret", "s3cret"));
    assert!(!verify_gitlab_token("s3cret", "s3crex"));
    assert!(!verify_gitlab_token("s3cret", "s3cre")); // length mismatch
    assert!(!verify_gitlab_token("s3cret", "")); // empty received never matches
}

#[test]
fn gitlab_webhook_parses_mr_push_and_ignores_label_edit() {
    use greybeard::webhook::{parse_gitlab, Verdict};
    let base = serde_json::json!({
        "user": {"username": "dev"},
        "project": {"path_with_namespace": "group/sub/proj"},
        "object_attributes": {"iid": 7, "draft": false, "action": "update"}
    });
    // `update` with an oldrev is a code push → review.
    let mut push = base.clone();
    push["object_attributes"]["oldrev"] = serde_json::json!("abc123");
    match parse_gitlab("greybeard-bot", "Merge Request Hook", &push) {
        Verdict::Review(t) => {
            assert_eq!((t.pr.owner.as_str(), t.pr.repo.as_str(), t.pr.number), ("group/sub", "proj", 7));
            assert!(!t.force);
        }
        _ => panic!("expected a review trigger for a push"),
    }
    // `update` without oldrev (label/assignee edit) → ignored.
    assert!(matches!(
        parse_gitlab("greybeard-bot", "Merge Request Hook", &base),
        Verdict::Ignore(_)
    ));
    // Draft MR open → ignored.
    let draft = serde_json::json!({
        "project": {"path_with_namespace": "g/p"},
        "object_attributes": {"iid": 1, "draft": true, "action": "open"}
    });
    assert!(matches!(
        parse_gitlab("greybeard-bot", "Merge Request Hook", &draft),
        Verdict::Ignore(_)
    ));
}

#[test]
fn gitlab_webhook_note_command_forces_review() {
    use greybeard::webhook::{parse_gitlab, Verdict};
    let note = serde_json::json!({
        "user": {"username": "dev"},
        "project": {"path_with_namespace": "group/proj"},
        "merge_request": {"iid": 12},
        "object_attributes": {"noteable_type": "MergeRequest", "note": "@greybeard-bot review please"}
    });
    match parse_gitlab("greybeard-bot", "Note Hook", &note) {
        Verdict::Review(t) => {
            assert_eq!((t.pr.owner.as_str(), t.pr.repo.as_str(), t.pr.number), ("group", "proj", 12));
            assert!(t.force);
            assert_eq!(t.action, "command");
            assert_eq!(t.sender, "dev");
        }
        _ => panic!("expected a forced review from the note command"),
    }
    // A note that doesn't mention+review is ignored; a non-MR note is ignored.
    let chatter = serde_json::json!({
        "user": {"username": "dev"},
        "object_attributes": {"noteable_type": "MergeRequest", "note": "looks good"}
    });
    assert!(matches!(parse_gitlab("greybeard-bot", "Note Hook", &chatter), Verdict::Ignore(_)));
}

#[test]
fn github_webhook_parse_pull_request_and_command() {
    use greybeard::webhook::{parse_github, Verdict};
    let bot = "greybeard-bot[bot]";
    let pr_event = serde_json::json!({
        "action": "opened",
        "pull_request": {"draft": false, "number": 5},
        "repository": {"owner": {"login": "o"}, "name": "r"}
    });
    match parse_github(bot, "pull_request", &pr_event) {
        Verdict::Review(t) => {
            assert_eq!((t.pr.owner.as_str(), t.pr.repo.as_str(), t.pr.number), ("o", "r", 5));
            assert!(!t.force);
        }
        _ => panic!("expected a review trigger"),
    }
    // Draft and irrelevant action are ignored; ping answers ping.
    let mut draft = pr_event.clone();
    draft["pull_request"]["draft"] = serde_json::json!(true);
    assert!(matches!(parse_github(bot, "pull_request", &draft), Verdict::Ignore(_)));
    let mut labeled = pr_event.clone();
    labeled["action"] = serde_json::json!("labeled");
    assert!(matches!(parse_github(bot, "pull_request", &labeled), Verdict::Ignore(_)));
    assert!(matches!(parse_github(bot, "ping", &serde_json::json!({})), Verdict::Ping));
    // Mention command on a PR forces a review.
    let cmd = serde_json::json!({
        "action": "created",
        "issue": {"pull_request": {}, "number": 9},
        "comment": {"body": "@greybeard-bot review", "user": {"login": "dev"}},
        "repository": {"full_name": "o/r"}
    });
    match parse_github(bot, "issue_comment", &cmd) {
        Verdict::Review(t) => {
            assert!(t.force);
            assert_eq!(t.action, "command");
            assert_eq!((t.pr.owner.as_str(), t.pr.repo.as_str(), t.pr.number), ("o", "r", 9));
        }
        _ => panic!("expected a forced review from the command"),
    }
}

#[test]
fn gitlab_pr_from_project_path() {
    use greybeard::gitlab::pr_from_project_path;
    let p = pr_from_project_path("group/sub/proj", 5).unwrap();
    assert_eq!((p.owner.as_str(), p.repo.as_str(), p.number), ("group/sub", "proj", 5));
    assert!(pr_from_project_path("lonely", 5).is_none());
    assert!(pr_from_project_path("group/proj", 0).is_none());
}

#[test]
fn gitlab_mr_url_parses_nested_namespace() {
    use greybeard::gitlab::parse_mr_url;
    let p = parse_mr_url("https://gitlab.com/group/subgroup/project/-/merge_requests/42").unwrap();
    assert_eq!((p.owner.as_str(), p.repo.as_str(), p.number), ("group/subgroup", "project", 42));
    assert_eq!(p.project(), "group/subgroup/project");
    // Simple two-segment path, trailing slash tolerated.
    let p2 = parse_mr_url("https://gitlab.com/acme/widgets/-/merge_requests/7/").unwrap();
    assert_eq!((p2.owner.as_str(), p2.repo.as_str(), p2.number), ("acme", "widgets", 7));
    // Self-managed host works; a GitHub PR URL and a bare-namespace URL are rejected.
    assert!(parse_mr_url("https://gitlab.example.com/a/b/-/merge_requests/1").is_ok());
    assert!(parse_mr_url("https://github.com/o/r/pull/1").is_err());
    assert!(parse_mr_url("https://gitlab.com/onlyone/-/merge_requests/1").is_err());
    // Trailing query string, fragment, and the /diffs sub-path all resolve to
    // the same iid (copied/notification links carry these).
    for suffix in ["?tab=diffs", "#note_42", "/diffs", "/pipelines?ref=x"] {
        let p = parse_mr_url(&format!("https://gitlab.com/acme/widgets/-/merge_requests/7{suffix}"))
            .unwrap_or_else(|e| panic!("suffix {suffix:?}: {e}"));
        assert_eq!((p.owner.as_str(), p.repo.as_str(), p.number), ("acme", "widgets", 7));
    }
}

#[test]
fn gitlab_api_endpoints_default_and_self_managed() {
    use greybeard::gitlab::api_endpoints;
    assert_eq!(
        api_endpoints(None),
        ("https://gitlab.com/api/v4".into(), "https://gitlab.com".into())
    );
    assert_eq!(
        api_endpoints(Some("  https://gitlab.example.com/  ")),
        ("https://gitlab.example.com/api/v4".into(), "https://gitlab.example.com".into())
    );
}

#[test]
fn gitlab_permalink_uses_dash_blob_and_short_anchor() {
    use greybeard::config::Forge;
    use greybeard::gitlab::parse_mr_url;
    use greybeard::pack::permalink;
    let pr = parse_mr_url("https://gitlab.com/group/sub/project/-/merge_requests/5").unwrap();
    let sha = "abcdef0";
    // GitLab uses the /-/blob/ prefix and an #L41-43 anchor (no second L).
    let link = permalink(Forge::GitLab, None, &pr, sha, "src/x.rs", Some(42));
    assert_eq!(link, "https://gitlab.com/group/sub/project/-/blob/abcdef0/src/x.rs#L41-43");
    // Self-managed base + no line.
    let link2 = permalink(Forge::GitLab, Some("https://gl.example.com/"), &pr, sha, "f.rs", None);
    assert_eq!(link2, "https://gl.example.com/group/sub/project/-/blob/abcdef0/f.rs");
}

#[test]
fn gitlab_bot_heuristic() {
    use greybeard::gitlab::looks_like_bot;
    // Real bot conventions match.
    assert!(looks_like_bot("project_123_bot_abc"));
    assert!(looks_like_bot("group_9_bot_xyz"));
    assert!(looks_like_bot("release-bot"));
    assert!(looks_like_bot("ci_bot"));
    assert!(looks_like_bot("deploy.bot"));
    assert!(looks_like_bot("greybeard-service-account"));
    // Humans (incl. names that merely end in "bot", and a human-owned
    // project_* account without a _bot segment) are not misclassified.
    assert!(!looks_like_bot("alice"));
    assert!(!looks_like_bot("robot"));
    assert!(!looks_like_bot("talbot"));
    assert!(!looks_like_bot("project_planning"));
}

#[test]
fn github_api_endpoints_default_and_enterprise() {
    use greybeard::github::api_endpoints;
    // Public github.com — REST and GraphQL share the host.
    assert_eq!(
        api_endpoints(None),
        ("https://api.github.com".into(), "https://api.github.com/graphql".into())
    );
    // GitHub Enterprise root — REST under /api/v3, GraphQL under /api/graphql;
    // trailing slash and surrounding whitespace are normalized away.
    assert_eq!(
        api_endpoints(Some("  https://ghe.example.com/  ")),
        ("https://ghe.example.com/api/v3".into(), "https://ghe.example.com/api/graphql".into())
    );
}
