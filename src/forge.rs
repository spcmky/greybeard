//! Forge selection seam.
//!
//! Every path that needs a code-host client goes through here instead of
//! constructing `github::Github` directly, so adding a backend is a change in
//! one place. GitHub and GitLab are both implemented; see `docs/GITLAB.md` for
//! the GitLab design.
//!
//! The pipeline is generic over the [`Forge`] trait, so a review runs against
//! any backend that implements it. `connect*` returns the [`ForgeClient`]
//! dispatch enum, which covers both GitHub and GitLab — adding another backend
//! is a change in this one place, with the pipeline untouched.

use anyhow::Result;

use crate::config::{Config, Forge as ForgeKind};
use crate::github::{Github, PrRef};
use crate::pack::ContextPack;

/// A code host the review pipeline can run against. The pipeline never names a
/// concrete backend — it fetches the pack, posts the comment, and re-checks
/// liveness entirely through this seam, so adding GitLab is a new impl, not a
/// pipeline change.
///
/// Native `async fn`s (not `async-trait`): the pipeline is generic over
/// `F: Forge`, never `dyn Forge`, so the concrete futures are monomorphized —
/// no boxing, no extra dependency.
#[allow(async_fn_in_trait)]
pub trait Forge {
    /// How this client authenticated — surfaced by the `auth-check` command.
    fn auth_mode(&self) -> &'static str;

    /// The authenticated identity (login/username), for `auth-check`. May error
    /// for credentials that can't resolve one (e.g. a GitHub App installation
    /// token) — callers treat that as non-fatal.
    async fn whoami(&self) -> Result<String>;

    /// Fetch everything for `pr` and render the deterministic context pack.
    async fn build_pack(&self, pr: &PrRef, cfg: &Config) -> Result<ContextPack>;

    async fn file_contents(&self, pr: &PrRef, path: &str, sha: &str) -> Result<Option<String>>;

    /// Create the single Greybeard comment, or update it in place; returns its
    /// URL. Never stacks a second comment.
    async fn upsert_comment(&self, pack: &ContextPack, body: &str) -> Result<String>;

    /// Cheap "is this change still open?" re-check, run just before posting so
    /// a PR closed mid-review is not commented on.
    async fn still_open(&self, pr: &PrRef) -> Result<bool>;
}

/// Static dispatch over the available backends. `connect*` returns this so the
/// pipeline (generic over `F: Forge`) runs against whichever forge the config
/// selects — with no `dyn`/vtable and no `async-trait` dependency.
pub enum ForgeClient {
    GitHub(Github),
    GitLab(crate::gitlab::Gitlab),
}

impl Forge for ForgeClient {
    fn auth_mode(&self) -> &'static str {
        match self {
            ForgeClient::GitHub(g) => g.auth_mode(),
            ForgeClient::GitLab(g) => g.auth_mode(),
        }
    }
    async fn whoami(&self) -> Result<String> {
        match self {
            ForgeClient::GitHub(g) => g.whoami().await,
            ForgeClient::GitLab(g) => g.whoami().await,
        }
    }
    async fn build_pack(&self, pr: &PrRef, cfg: &Config) -> Result<ContextPack> {
        match self {
            ForgeClient::GitHub(g) => g.build_pack(pr, cfg).await,
            ForgeClient::GitLab(g) => g.build_pack(pr, cfg).await,
        }
    }
    async fn file_contents(&self, pr: &PrRef, path: &str, sha: &str) -> Result<Option<String>> {
        match self {
            ForgeClient::GitHub(g) => g.file_contents(pr, path, sha).await,
            ForgeClient::GitLab(g) => g.file_contents(pr, path, sha).await,
        }
    }
    async fn upsert_comment(&self, pack: &ContextPack, body: &str) -> Result<String> {
        match self {
            ForgeClient::GitHub(g) => g.upsert_comment(pack, body).await,
            ForgeClient::GitLab(g) => g.upsert_comment(pack, body).await,
        }
    }
    async fn still_open(&self, pr: &PrRef) -> Result<bool> {
        match self {
            ForgeClient::GitHub(g) => g.still_open(pr).await,
            ForgeClient::GitLab(g) => g.still_open(pr).await,
        }
    }
}

/// Parse a review target URL for the configured forge into a [`PrRef`]
/// (GitHub PR URL, or GitLab MR URL with its nested namespace + `/-/`).
pub fn parse_ref(cfg: &Config, url: &str) -> Result<PrRef> {
    match cfg.forge {
        ForgeKind::GitHub => PrRef::parse(url),
        ForgeKind::GitLab => crate::gitlab::parse_mr_url(url),
    }
}

/// Gate startup on the configured forge having a backend. Both GitHub and
/// GitLab are supported today; this seam stays so a future forge fails early
/// and clearly. Pure (no IO), so CLI and serve startup can both gate on it
/// before doing any work — and it's unit-testable without constructing a client.
pub fn ensure_supported(forge: ForgeKind) -> Result<()> {
    match forge {
        ForgeKind::GitHub | ForgeKind::GitLab => Ok(()),
    }
}

/// Connect to the configured forge with ambient credentials.
pub async fn connect(cfg: &Config) -> Result<ForgeClient> {
    connect_installation(cfg, None).await
}

/// Connect for a specific installation/context (GitHub App installation id;
/// ignored by backends without an app model).
pub async fn connect_installation(cfg: &Config, installation: Option<u64>) -> Result<ForgeClient> {
    ensure_supported(cfg.forge)?;
    match cfg.forge {
        ForgeKind::GitHub => Ok(ForgeClient::GitHub(
            Github::for_installation(installation, cfg.forge_base_url.as_deref()).await?,
        )),
        ForgeKind::GitLab => Ok(ForgeClient::GitLab(
            crate::gitlab::Gitlab::connect(cfg.forge_base_url.as_deref()).await?,
        )),
    }
}
