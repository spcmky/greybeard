use std::time::{Duration, Instant, SystemTime};

use anyhow::{anyhow, bail, Context, Result};
use aws_credential_types::provider::ProvideCredentials;
use aws_sigv4::http_request::{sign, SignableBody, SignableRequest, SigningSettings};
use aws_sigv4::sign::v4;
use serde::de::DeserializeOwned;
use serde_json::{json, Value};

use crate::config::{Config, Provider};
use crate::telemetry::Telemetry;

const ANTHROPIC_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Which model tier a call runs on.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Tier {
    /// Strong model at high effort: the review lenses.
    Lens,
    /// Verification + eligibility: the verify model at low effort. Defaults to
    /// the lens model, so these calls read the same prompt cache.
    Verify,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
    #[serde(default)]
    pub cache_read_input_tokens: u64,
}

pub struct Llm {
    http: reqwest::Client,
    cfg: Config,
    anthropic_key: Option<String>,
    aws_creds: Option<aws_credential_types::provider::SharedCredentialsProvider>,
    telemetry: Telemetry,
}

impl Llm {
    pub async fn new(cfg: Config, telemetry: Telemetry) -> Result<Self> {
        let (anthropic_key, aws_creds) = match cfg.provider {
            Provider::Anthropic => {
                let key = std::env::var("ANTHROPIC_API_KEY")
                    .context("ANTHROPIC_API_KEY is required for provider=anthropic")?;
                (Some(key), None)
            }
            Provider::Bedrock => {
                let aws = aws_config::defaults(aws_config::BehaviorVersion::latest())
                    .region(aws_config::Region::new(cfg.aws_region.clone()))
                    .load()
                    .await;
                let provider = aws
                    .credentials_provider()
                    .ok_or_else(|| anyhow!("no AWS credentials resolved for provider=bedrock"))?;
                (None, Some(provider))
            }
        };
        Ok(Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(cfg.lens_timeout_secs + 30))
                .build()?,
            cfg,
            anthropic_key,
            aws_creds,
            telemetry,
        })
    }

    fn model(&self, tier: Tier) -> &str {
        match tier {
            Tier::Lens => &self.cfg.lens_model,
            Tier::Verify => &self.cfg.verify_model,
        }
    }

    /// One structured call: system blocks (cached prefix) + user instruction,
    /// expecting a JSON object matching `T`. Retries once on unparseable output.
    pub async fn structured<T: DeserializeOwned>(
        &self,
        tier: Tier,
        label: &str,
        system_blocks: &[Value],
        user: &str,
        schema: &Value,
    ) -> Result<T> {
        let max_tokens = match tier {
            Tier::Lens => self.cfg.lens_max_tokens,
            Tier::Verify => self.cfg.verify_max_tokens,
        };
        let mut messages = vec![json!({"role": "user", "content": user})];
        for attempt in 0..2 {
            let body = self.build_body(tier, system_blocks, &messages, max_tokens, Some(schema));
            let text = self.post(tier, label, body).await?;
            match parse_json_object::<T>(&text) {
                Ok(v) => return Ok(v),
                Err(e) if attempt == 0 => {
                    messages.push(json!({"role": "assistant", "content": text}));
                    messages.push(json!({
                        "role": "user",
                        "content": "Your previous output was not a valid JSON object matching the required schema. Respond with ONLY the JSON object, no prose, no code fences."
                    }));
                    eprintln!("greybeard: {label}: retrying after parse failure: {e}");
                }
                Err(e) => bail!("{label}: unparseable model output after retry: {e}"),
            }
        }
        unreachable!()
    }

    /// Prefill-only cache warm: `max_tokens: 0` writes the shared prefix into the
    /// prompt cache and returns immediately. Best-effort — failures are logged
    /// and ignored (the first real call then pays the cache write instead).
    pub async fn warm(&self, tier: Tier, system_blocks: &[Value]) {
        let messages = vec![json!({"role": "user", "content": "warmup"})];
        let body = self.build_body(tier, system_blocks, &messages, 0, None);
        if let Err(e) = self.post(tier, "cache-warm", body).await {
            eprintln!("greybeard: cache warm skipped ({e})");
        }
    }

    fn build_body(
        &self,
        tier: Tier,
        system_blocks: &[Value],
        messages: &[Value],
        max_tokens: u32,
        schema: Option<&Value>,
    ) -> Value {
        let mut body = json!({
            "max_tokens": max_tokens,
            "system": system_blocks,
            "messages": messages,
        });
        match self.cfg.provider {
            Provider::Anthropic => {
                body["model"] = json!(self.model(tier));
                // max_tokens:0 (warm) rejects output_config.format, so schema
                // is None there. Verify runs at low effort: short adversarial
                // checks, not deep exploration.
                let mut output_config = serde_json::Map::new();
                output_config.insert(
                    "effort".into(),
                    json!(match tier {
                        Tier::Lens => "high",
                        Tier::Verify => "low",
                    }),
                );
                if let Some(s) = schema {
                    output_config
                        .insert("format".into(), json!({"type": "json_schema", "schema": s}));
                }
                if !output_config.is_empty() {
                    body["output_config"] = Value::Object(output_config);
                }
            }
            Provider::Bedrock => {
                // Legacy InvokeModel wire shape: version marker instead of model
                // (model rides in the URL path). output_config is not sent —
                // JSON shape is enforced by prompt + parse-retry instead.
                body["anthropic_version"] = json!("bedrock-2023-05-31");
            }
        }
        body
    }

    /// Post with retry: throttling (429) and transient server errors (5xx/529)
    /// back off 2s/8s/20s before failing — a burst of parallel reviews must
    /// degrade gracefully, not drop lenses.
    async fn post(&self, tier: Tier, label: &str, body: Value) -> Result<String> {
        const BACKOFF_SECS: [u64; 3] = [2, 8, 20];
        let mut attempt = 0;
        loop {
            match self.post_once(tier, label, body.clone()).await {
                Err(e) if attempt < BACKOFF_SECS.len() && is_retryable(&e) => {
                    eprintln!("greybeard: {label}: retryable ({e}); backing off {}s", BACKOFF_SECS[attempt]);
                    tokio::time::sleep(Duration::from_secs(BACKOFF_SECS[attempt])).await;
                    attempt += 1;
                }
                other => return other,
            }
        }
    }

    async fn post_once(&self, tier: Tier, label: &str, body: Value) -> Result<String> {
        let started = Instant::now();
        let timeout = Duration::from_secs(match tier {
            Tier::Lens => self.cfg.lens_timeout_secs,
            Tier::Verify => self.cfg.verify_timeout_secs,
        });
        let resp: Value = match self.cfg.provider {
            Provider::Anthropic => {
                let r = self
                    .http
                    .post(ANTHROPIC_URL)
                    .timeout(timeout)
                    .header("x-api-key", self.anthropic_key.as_deref().unwrap())
                    .header("anthropic-version", ANTHROPIC_VERSION)
                    .json(&body)
                    .send()
                    .await
                    .with_context(|| format!("{label}: request failed"))?;
                let status = r.status();
                let v: Value = r.json().await.with_context(|| format!("{label}: bad response body"))?;
                if !status.is_success() {
                    bail!("{label}: API error {status}: {}", v["error"]["message"].as_str().unwrap_or("?"));
                }
                v
            }
            Provider::Bedrock => self.post_bedrock(tier, label, &body, timeout).await?,
        };

        let usage: Usage = serde_json::from_value(resp["usage"].clone()).unwrap_or_default();
        self.telemetry
            .record(label, started.elapsed(), &usage);

        if resp["stop_reason"] == "refusal" {
            bail!("{label}: model refused (stop_reason=refusal)");
        }
        let text = resp["content"]
            .as_array()
            .map(|blocks| {
                blocks
                    .iter()
                    .filter(|b| b["type"] == "text")
                    .filter_map(|b| b["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("")
            })
            .unwrap_or_default();
        Ok(text)
    }

    async fn post_bedrock(
        &self,
        tier: Tier,
        label: &str,
        body: &Value,
        timeout: Duration,
    ) -> Result<Value> {
        let model = self.model(tier);
        // ':' in inference-profile ids must be percent-encoded in the path.
        let encoded_model = model.replace(':', "%3A");
        let url = format!(
            "https://bedrock-runtime.{}.amazonaws.com/model/{}/invoke",
            self.cfg.aws_region, encoded_model
        );
        let payload = serde_json::to_vec(body)?;

        let creds = self
            .aws_creds
            .as_ref()
            .unwrap()
            .provide_credentials()
            .await
            .context("resolving AWS credentials")?;
        let identity = creds.into();
        let params = v4::SigningParams::builder()
            .identity(&identity)
            .region(&self.cfg.aws_region)
            .name("bedrock")
            .time(SystemTime::now())
            .settings(SigningSettings::default())
            .build()
            .map_err(|e| anyhow!("sigv4 params: {e}"))?;
        let headers = [("content-type", "application/json"), ("accept", "application/json")];
        let signable = SignableRequest::new(
            "POST",
            &url,
            headers.iter().map(|(k, v)| (*k, *v)),
            SignableBody::Bytes(&payload),
        )
        .map_err(|e| anyhow!("sigv4 signable: {e}"))?;
        let (instructions, _sig) = sign(signable, &params.into())
            .map_err(|e| anyhow!("sigv4 sign: {e}"))?
            .into_parts();

        let mut http_req = http::Request::builder()
            .method("POST")
            .uri(&url)
            .header("content-type", "application/json")
            .header("accept", "application/json")
            .body(())?;
        instructions.apply_to_request_http1x(&mut http_req);

        let mut req = self.http.post(&url).timeout(timeout).body(payload);
        for (name, value) in http_req.headers() {
            req = req.header(name, value);
        }
        let r = req.send().await.with_context(|| format!("{label}: bedrock request failed"))?;
        let status = r.status();
        let v: Value = r.json().await.with_context(|| format!("{label}: bad bedrock body"))?;
        if !status.is_success() {
            bail!("{label}: bedrock error {status}: {}", v["message"].as_str().unwrap_or("?"));
        }
        Ok(v)
    }
}

/// Throttle / transient-server errors carry their HTTP status in the message.
fn is_retryable(e: &anyhow::Error) -> bool {
    let msg = e.to_string();
    ["429", "529", "500", "502", "503", "throttl", "Throttl", "overloaded"]
        .iter()
        .any(|m| msg.contains(m))
}

/// Extract and parse the first JSON object from model text output —
/// tolerates code fences and surrounding prose.
pub fn parse_json_object<T: DeserializeOwned>(text: &str) -> Result<T> {
    let start = text.find('{').ok_or_else(|| anyhow!("no '{{' in output"))?;
    let end = text.rfind('}').ok_or_else(|| anyhow!("no '}}' in output"))?;
    if end < start {
        bail!("malformed JSON bounds");
    }
    Ok(serde_json::from_str(&text[start..=end])?)
}
