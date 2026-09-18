use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use axum::{extract::State, http::HeaderMap, http::StatusCode, routing::post, Json, Router};

use super::*;
use crate::config::Forge;
use crate::prompts;

fn config(base: &str) -> Config {
    Config {
        forge: Forge::GitHub,
        forge_base_url: None,
        provider: Provider::OpenAi,
        openai_base_url: Some(base.into()),
        model_max_concurrent: Some(1),
        lens_model: "local-lens".into(),
        verify_model: "local-verify".into(),
        aws_region: "us-east-2".into(),
        confidence_threshold: 80,
        lens_timeout_secs: 1,
        verify_timeout_secs: 1,
        lens_max_tokens: 16_000,
        verify_max_tokens: 1_500,
        max_file_lines: 2_000,
        max_pack_files: 60,
        max_pack_chars: 600_000,
        max_diff_chars: 300_000,
        review_bot_prs: false,
        max_concurrent_reviews: 1,
        daily_review_limit: 50,
        mention_cooldown_secs: 600,
        prices: None,
    }
}

#[derive(Default)]
struct Mock {
    requests: Vec<(HeaderMap, Value)>,
    responses: VecDeque<(StatusCode, String)>,
    delay_ms: u64,
}

struct Server {
    base: String,
    state: Arc<Mutex<Mock>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn server(responses: Vec<(StatusCode, String)>, delay_ms: u64) -> Server {
    let state = Arc::new(Mutex::new(Mock {
        responses: responses.into(),
        delay_ms,
        ..Default::default()
    }));
    let app = Router::new()
        .route(
            "/v1/chat/completions",
            post(
                |State(state): State<Arc<Mutex<Mock>>>,
                 headers: HeaderMap,
                 Json(body): Json<Value>| async move {
                    let (response, delay_ms) = {
                        let mut mock = state.lock().unwrap();
                        mock.requests.push((headers, body));
                        (mock.responses.pop_front().unwrap(), mock.delay_ms)
                    };
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                    response
                },
            ),
        )
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Server { base, state, task }
}

fn completion(content: &str) -> (StatusCode, String) {
    (StatusCode::OK, json!({
        "choices": [{"finish_reason": "stop", "message": {"content": content}}],
        "usage": {"prompt_tokens": 100, "completion_tokens": 12, "prompt_tokens_details": {"cached_tokens": 80}}
    }).to_string())
}

#[tokio::test]
async fn structured_chat_retries_with_history_and_accounts_for_cache() {
    let server = server(
        vec![
            completion("invalid JSON"),
            completion(r#"{"skip":false,"reason":"code change"}"#),
        ],
        0,
    )
    .await;
    let telemetry = Telemetry::new();
    let mut llm = Llm::new(config(&server.base), telemetry.clone())
        .await
        .unwrap();
    llm.openai_key = Some("test-key".into());
    let system = prompts::system_blocks("sample pack");
    llm.warm(Tier::Lens, &system).await;
    assert!(server.state.lock().unwrap().requests.is_empty());
    let result: crate::pipeline::Eligibility = llm
        .structured(
            Tier::Verify,
            "eligibility",
            &system,
            "check this PR",
            &prompts::eligibility_schema(),
        )
        .await
        .unwrap();
    assert!(!result.skip);
    let mock = server.state.lock().unwrap();
    assert_eq!(mock.requests.len(), 2);
    let (headers, first) = &mock.requests[0];
    assert_eq!(headers["authorization"], "Bearer test-key");
    assert_eq!(first["model"], "local-verify");
    assert_eq!(first["max_tokens"], 1_500);
    assert_eq!(first["stream"], false);
    assert_eq!(
        first["messages"][0],
        json!({
            "role": "system", "content": format!("{}\n\n<context_pack>\nsample pack\n</context_pack>", prompts::CORE)
        })
    );
    assert_eq!(
        first["messages"][1],
        json!({"role":"user", "content":"check this PR"})
    );
    assert_eq!(first["response_format"]["type"], "json_schema");
    assert_eq!(
        first["response_format"]["json_schema"]["schema"],
        prompts::eligibility_schema()
    );
    for field in [
        "system",
        "anthropic_version",
        "output_config",
        "reasoning_effort",
    ] {
        assert!(first.get(field).is_none());
    }
    let retry = &mock.requests[1].1;
    assert_eq!(retry["messages"][0], first["messages"][0]);
    assert_eq!(
        retry["messages"][2],
        json!({"role":"assistant", "content":"invalid JSON"})
    );
    assert_eq!(retry["messages"][3]["role"], "user");
    let usage = telemetry.totals();
    assert_eq!(usage.input_tokens, 40);
    assert_eq!(usage.cache_read_input_tokens, 160);
    assert_eq!(usage.output_tokens, 24);
}

#[tokio::test]
async fn local_requests_queue_before_timeout_and_allow_no_key() {
    let server = server(vec![completion(r#"{"findings":[]}"#); 3], 450).await;
    let mut llm = Llm::new(config(&server.base), Telemetry::new())
        .await
        .unwrap();
    llm.openai_key = None;
    let system = prompts::system_blocks("pack");
    let schema = prompts::findings_schema();
    let calls = (0..3).map(|_| {
        llm.structured::<crate::pipeline::LensReport>(
            Tier::Lens,
            "lens",
            &system,
            "review",
            &schema,
        )
    });
    // Total time exceeds the one-second request timeout; every individual
    // request must still succeed because the queue is outside that timeout.
    let start = Instant::now();
    for result in futures::future::join_all(calls).await {
        assert!(result.unwrap().findings.is_empty());
    }
    assert!(start.elapsed() > Duration::from_secs(1));
    let mock = server.state.lock().unwrap();
    assert_eq!(mock.requests.len(), 3);
    for (headers, body) in &mock.requests {
        assert!(!headers.contains_key("authorization"));
        assert!(!headers.contains_key("x-api-key"));
        assert_eq!(body["model"], "local-lens");
        assert_eq!(body["max_tokens"], 16_000);
    }
}

#[tokio::test]
async fn plain_text_server_errors_retry_but_bad_requests_fail() {
    let server = server(
        vec![
            (StatusCode::SERVICE_UNAVAILABLE, "model loading".into()),
            completion(r#"{"skip":false}"#),
            (StatusCode::BAD_REQUEST, "unsupported model".into()),
        ],
        0,
    )
    .await;
    let llm = Llm::new(config(&server.base), Telemetry::new())
        .await
        .unwrap();
    let system = prompts::system_blocks("pack");
    let schema = prompts::eligibility_schema();
    let result: crate::pipeline::Eligibility = llm
        .structured(Tier::Verify, "eligibility", &system, "review", &schema)
        .await
        .unwrap();
    assert!(!result.skip);
    let error = llm
        .structured::<Value>(Tier::Verify, "eligibility", &system, "review", &schema)
        .await
        .unwrap_err();
    assert!(error
        .to_string()
        .contains("400 Bad Request: unsupported model"));
    assert_eq!(server.state.lock().unwrap().requests.len(), 3);
}

#[tokio::test]
async fn hosted_request_shapes_are_preserved() {
    let mut llm = Llm::new(config("http://localhost/v1"), Telemetry::new())
        .await
        .unwrap();
    let system = prompts::system_blocks("pack");
    let messages = [json!({"role":"user", "content":"review"})];
    let schema = prompts::findings_schema();
    llm.cfg.provider = Provider::Anthropic;
    let body = llm.build_body(Tier::Lens, &system, &messages, 16000, Some(&schema));
    assert_eq!(body["system"], json!(system));
    assert_eq!(body["output_config"]["effort"], "high");
    assert_eq!(body["output_config"]["format"]["schema"], schema);
    assert!(body.get("response_format").is_none());
    llm.cfg.provider = Provider::Bedrock;
    let body = llm.build_body(Tier::Verify, &system, &messages, 1500, Some(&schema));
    assert_eq!(body["system"], json!(system));
    assert_eq!(body["anthropic_version"], "bedrock-2023-05-31");
    assert!(body.get("model").is_none());
    assert!(body.get("output_config").is_none());
}

#[test]
fn response_errors_and_missing_usage_are_handled() {
    for resp in [
        json!({}),
        json!({"choices": []}),
        json!({"choices": [{"message": {"content": null}}]}),
        json!({"choices": [{"message": {"content": "  "}}]}),
        json!({"choices": [{"message": {"refusal": "no", "content": "{}"}}]}),
        json!({"choices": [{"finish_reason": "content_filter", "message": {"content": "{}"}}]}),
        json!({"choices": [{"finish_reason": "length", "message": {"content": "{}"}}]}),
    ] {
        assert!(openai_text("test", &resp).is_err());
    }
    assert_eq!(
        openai_text(
            "test",
            &json!({"choices": [{"message": {"content":"{}", "refusal":null}}]})
        )
        .unwrap(),
        "{}"
    );
    assert_eq!(openai_usage(&json!({})).input_tokens, 0);
    let usage = openai_usage(&json!({"usage": {"prompt_tokens": 5, "completion_tokens": 2}}));
    assert_eq!(usage.input_tokens, 5);
    assert_eq!(usage.cache_read_input_tokens, 0);
    let usage = openai_usage(
        &json!({"usage": {"prompt_tokens": 5, "prompt_tokens_details": {"cached_tokens": 10}}}),
    );
    assert_eq!(usage.input_tokens, 0);
    assert_eq!(usage.cache_read_input_tokens, 5);
}
