use anyhow::{bail, Context, Result};

#[derive(Debug, Clone, PartialEq)]
pub enum Provider {
    Anthropic,
    Bedrock,
    OpenAi,
}

/// The code-host ("forge") a review runs against. GitHub is the only backend
/// implemented today; GitLab is specified in docs/GITLAB.md and dispatched
/// from src/forge.rs. This exists so config, docs, and the connect seam are
/// forge-neutral ahead of that backend landing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Forge {
    GitHub,
    GitLab,
}

impl Forge {
    pub fn parse(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "github" | "gh" => Ok(Forge::GitHub),
            "gitlab" | "gl" => Ok(Forge::GitLab),
            other => bail!("GREYBEARD_FORGE must be 'github' or 'gitlab', got '{other}'"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    /// Which code host to review against (default GitHub).
    pub forge: Forge,
    /// Base URL for a self-hosted forge (GitHub Enterprise / self-managed
    /// GitLab), no trailing slash. None uses the forge's public API host.
    pub forge_base_url: Option<String>,
    pub provider: Provider,
    /// OpenAI-compatible API root, including /v1. Required for provider=openai.
    pub openai_base_url: Option<String>,
    /// Maximum simultaneous model requests per review. Local servers default
    /// to one; hosted providers have no limit unless explicitly configured.
    pub model_max_concurrent: Option<usize>,
    /// Strong model for the review lenses.
    pub lens_model: String,
    /// Model for eligibility + per-finding verification. Defaults to the lens
    /// model (at low effort): the 2026-08-17 bake-off showed a weak verifier
    /// invents wrong rejection rationales and suppresses real findings.
    pub verify_model: String,
    pub aws_region: String,
    /// Verified-true findings at/above this confidence post as numbered findings.
    pub confidence_threshold: u8,
    /// Per-lens hard timeout.
    pub lens_timeout_secs: u64,
    pub verify_timeout_secs: u64,
    pub lens_max_tokens: u32,
    pub verify_max_tokens: u32,
    /// Cap on file content included in the pack, per file.
    pub max_file_lines: usize,
    /// Cap on changed files fully inlined into the pack.
    pub max_pack_files: usize,
    /// Cap on the rendered pack size; past it, remaining file contents are
    /// omitted (their diffs stay) and the pack says so.
    pub max_pack_chars: usize,
    /// Cap on the diff section itself. Over budget, generated-file hunks
    /// (lockfiles, dist/, minified) are stubbed first, then the largest
    /// remaining per-file diffs.
    pub max_diff_chars: usize,
    /// Review PRs authored by bot accounts (dependabot etc.)? Default false —
    /// a hard pre-model skip; --force overrides per run.
    pub review_bot_prs: bool,
    /// Service mode: max reviews running at once.
    pub max_concurrent_reviews: usize,
    /// Service mode: circuit breaker — max reviews per UTC day.
    pub daily_review_limit: u32,
    /// Service mode: per-user cooldown for @-mention forced reviews.
    pub mention_cooldown_secs: u64,
    /// Cost telemetry activates only when all four GREYBEARD_PRICE_* knobs are
    /// set; tokens are always reported either way.
    pub prices: Option<Prices>,
}

/// $ per million tokens. No defaults in code — real prices live in the chart
/// values, where they can be corrected without a rebuild.
#[derive(Debug, Clone, Copy)]
pub struct Prices {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

impl Prices {
    pub fn from_env() -> Option<Self> {
        let get = |k: &str| std::env::var(k).ok().and_then(|v| v.parse::<f64>().ok());
        Some(Self {
            input: get("GREYBEARD_PRICE_IN")?,
            output: get("GREYBEARD_PRICE_OUT")?,
            cache_read: get("GREYBEARD_PRICE_CACHE_READ")?,
            cache_write: get("GREYBEARD_PRICE_CACHE_WRITE")?,
        })
    }

    pub fn cost_usd(&self, u: &crate::llm::Usage) -> f64 {
        (u.input_tokens as f64 * self.input
            + u.output_tokens as f64 * self.output
            + u.cache_read_input_tokens as f64 * self.cache_read
            + u.cache_creation_input_tokens as f64 * self.cache_write)
            / 1e6
    }
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let provider = match std::env::var("GREYBEARD_PROVIDER").ok().as_deref() {
            Some("anthropic") => Provider::Anthropic,
            Some("bedrock") => Provider::Bedrock,
            Some("openai") => Provider::OpenAi,
            Some(other) => bail!(
                "GREYBEARD_PROVIDER must be 'anthropic', 'bedrock', or 'openai', got '{other}'"
            ),
            // Default by what credentials are present.
            None => {
                if std::env::var("ANTHROPIC_API_KEY").is_ok() {
                    Provider::Anthropic
                } else {
                    Provider::Bedrock
                }
            }
        };

        let lens_default = match provider {
            Provider::Anthropic => "claude-opus-5",
            // Bedrock model/inference-profile IDs vary by account setup — require them explicitly.
            Provider::Bedrock | Provider::OpenAi => "",
        };
        let lens_model =
            std::env::var("GREYBEARD_LENS_MODEL").unwrap_or_else(|_| lens_default.to_string());
        if lens_model.trim().is_empty() {
            match provider {
                Provider::Bedrock => bail!(
                    "provider=bedrock requires GREYBEARD_LENS_MODEL \
                     (a Bedrock inference-profile ID, e.g. us.anthropic.claude-...)"
                ),
                _ => bail!(
                    "GREYBEARD_LENS_MODEL is required (e.g. qwen3-coder-next for provider=openai)"
                ),
            }
        }
        let verify_model =
            std::env::var("GREYBEARD_VERIFY_MODEL").unwrap_or_else(|_| lens_model.clone());
        let openai_base_url = if provider == Provider::OpenAi {
            let raw = std::env::var("GREYBEARD_OPENAI_BASE_URL")
                .context("provider=openai requires GREYBEARD_OPENAI_BASE_URL (e.g. http://localhost:8000/v1)")?;
            Some(parse_openai_base_url(&raw)?)
        } else {
            None
        };
        let model_max_concurrent = match std::env::var("GREYBEARD_MODEL_MAX_CONCURRENT") {
            Ok(raw) => {
                let limit = raw
                    .parse::<usize>()
                    .context("GREYBEARD_MODEL_MAX_CONCURRENT must be a positive integer")?;
                if limit == 0 || limit > tokio::sync::Semaphore::MAX_PERMITS {
                    bail!("GREYBEARD_MODEL_MAX_CONCURRENT is outside the supported range");
                }
                Some(limit)
            }
            Err(_) if provider == Provider::OpenAi => Some(1),
            Err(_) => None,
        };

        let forge = match std::env::var("GREYBEARD_FORGE") {
            Ok(v) if !v.is_empty() => Forge::parse(&v)?,
            _ => Forge::GitHub,
        };
        let forge_base_url = std::env::var("GREYBEARD_FORGE_URL")
            .ok()
            .map(|u| u.trim().trim_end_matches('/').to_string())
            .filter(|u| !u.is_empty());

        let defaults = Self::for_pack();
        Ok(Self {
            forge,
            forge_base_url,
            provider,
            openai_base_url,
            model_max_concurrent,
            lens_model,
            verify_model,
            aws_region: std::env::var("AWS_DEFAULT_REGION")
                .or_else(|_| std::env::var("AWS_REGION"))
                .unwrap_or_else(|_| "us-east-2".to_string()),
            review_bot_prs: std::env::var("GREYBEARD_REVIEW_BOT_PRS").as_deref() == Ok("true"),
            max_concurrent_reviews: std::env::var("GREYBEARD_MAX_CONCURRENT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(2),
            daily_review_limit: std::env::var("GREYBEARD_DAILY_REVIEW_LIMIT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(50),
            mention_cooldown_secs: 600,
            // Env-tunable so a weaker (e.g. local) verifier's confidence
            // distribution can be re-calibrated without a rebuild: it clusters
            // its self-scored confidence differently from the strong-model
            // default of 80, and verify.rs demotes anything below this to
            // Unverified rather than posting it.
            confidence_threshold: env_u8_capped(
                "GREYBEARD_CONFIDENCE_THRESHOLD",
                defaults.confidence_threshold,
            )?,
            verify_max_tokens: env_positive(
                "GREYBEARD_VERIFY_MAX_TOKENS",
                defaults.verify_max_tokens,
            )?,
            verify_timeout_secs: u64::from(env_positive(
                "GREYBEARD_VERIFY_TIMEOUT_SECS",
                defaults.verify_timeout_secs as u32,
            )?),
            prices: Prices::from_env(),
            ..defaults
        })
    }

    /// Pack inspection needs limits, but no model or forge credentials.
    pub fn for_pack() -> Self {
        Self {
            forge: Forge::GitHub,
            forge_base_url: None,
            provider: Provider::Anthropic,
            openai_base_url: None,
            model_max_concurrent: None,
            lens_model: String::new(),
            verify_model: String::new(),
            aws_region: String::new(),
            confidence_threshold: 80,
            lens_timeout_secs: 240,
            verify_timeout_secs: 120,
            lens_max_tokens: 16_000,
            verify_max_tokens: 4_000,
            max_file_lines: 2_000,
            max_pack_files: 60,
            max_pack_chars: 600_000,
            max_diff_chars: 300_000,
            review_bot_prs: false,
            max_concurrent_reviews: 2,
            daily_review_limit: 50,
            mention_cooldown_secs: 600,
            prices: None,
        }
    }
}

fn env_positive(name: &str, default: u32) -> Result<u32> {
    let raw = match std::env::var(name) {
        Ok(raw) => raw,
        Err(std::env::VarError::NotPresent) => return Ok(default),
        Err(error) => return Err(error).with_context(|| format!("reading {name}")),
    };
    let value: u32 = raw
        .parse()
        .with_context(|| format!("{name} must be a positive integer"))?;
    if value == 0 {
        bail!("{name} must be a positive integer");
    }
    Ok(value)
}

/// Read a 0-100 threshold env var, falling back to `default` when unset. Unlike
/// [`env_positive`], 0 is legal (post everything the verifier confirms) and
/// values above 100 are rejected so a typo like `800` fails loudly instead of
/// silently disabling the gate.
pub fn env_u8_capped(name: &str, default: u8) -> Result<u8> {
    let raw = match std::env::var(name) {
        Ok(raw) => raw,
        Err(std::env::VarError::NotPresent) => return Ok(default),
        Err(error) => return Err(error).with_context(|| format!("reading {name}")),
    };
    let value: u8 = raw
        .trim()
        .parse()
        .with_context(|| format!("{name} must be an integer 0-100"))?;
    if value > 100 {
        bail!("{name} must be 0-100, got {value}");
    }
    Ok(value)
}

fn parse_openai_base_url(raw: &str) -> Result<String> {
    let base = raw.trim().trim_end_matches('/');
    let url = reqwest::Url::parse(base).context("invalid GREYBEARD_OPENAI_BASE_URL")?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        bail!("GREYBEARD_OPENAI_BASE_URL must be an absolute http:// or https:// URL");
    }
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!("GREYBEARD_OPENAI_BASE_URL must not include credentials, a query, or a fragment");
    }
    Ok(base.to_string())
}

#[cfg(test)]
mod tests {
    use super::{env_u8_capped, parse_openai_base_url};

    #[test]
    fn confidence_threshold_env_defaults_clamps_and_rejects() {
        // A test-unique key so the process-global env is not shared with other
        // parallel tests.
        let key = "GREYBEARD_TEST_CONFIDENCE_THRESHOLD";
        std::env::remove_var(key);
        assert_eq!(env_u8_capped(key, 80).unwrap(), 80); // unset → default
        std::env::set_var(key, "55");
        assert_eq!(env_u8_capped(key, 80).unwrap(), 55);
        std::env::set_var(key, " 0 "); // 0 is legal (post everything), trimmed
        assert_eq!(env_u8_capped(key, 80).unwrap(), 0);
        std::env::set_var(key, "100");
        assert_eq!(env_u8_capped(key, 80).unwrap(), 100);
        std::env::set_var(key, "101"); // >100 fails loudly
        assert!(env_u8_capped(key, 80).is_err());
        std::env::set_var(key, "high"); // non-numeric fails loudly
        assert!(env_u8_capped(key, 80).is_err());
        std::env::remove_var(key);
    }

    #[test]
    fn openai_base_url_normalizes_and_validates() {
        assert_eq!(
            parse_openai_base_url(" http://localhost:8000/v1/ ").unwrap(),
            "http://localhost:8000/v1"
        );
        assert_eq!(
            parse_openai_base_url("https://models.example/api/v1").unwrap(),
            "https://models.example/api/v1"
        );
        for bad in [
            "",
            "localhost:8000/v1",
            "ftp://localhost/v1",
            "http://user:pass@localhost/v1",
            "http://localhost/v1?key=secret",
            "http://localhost/v1#fragment",
        ] {
            assert!(parse_openai_base_url(bad).is_err(), "accepted {bad}");
        }
    }
}
