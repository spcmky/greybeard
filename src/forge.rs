//! Forge selection seam.
//!
//! Every path that needs a code-host client goes through here instead of
//! constructing `github::Github` directly, so adding a backend is a change in
//! one place. Today GitHub is the only implementation; the GitLab backend is
//! specified in `docs/GITLAB.md`.
//!
//! When GitLab lands, `connect*` will return a `Box<dyn Forge>` (the trait is
//! sketched in the design doc) and the pipeline will be generic over it. Until
//! then these return the concrete GitHub client — the dispatch point is what
//! matters for now.

use anyhow::{bail, Result};

use crate::config::{Config, Forge};
use crate::github::Github;

/// Fail early and clearly for a forge that has no backend yet. Pure (no IO), so
/// CLI and serve startup can both gate on it before doing any work — and it's
/// unit-testable without constructing a client.
pub fn ensure_supported(forge: Forge) -> Result<()> {
    match forge {
        Forge::GitHub => Ok(()),
        Forge::GitLab => bail!(
            "GREYBEARD_FORGE=gitlab is not implemented yet — the GitLab backend is \
             specified in docs/GITLAB.md. Set GREYBEARD_FORGE=github (the default) to run."
        ),
    }
}

/// Connect to the configured forge with ambient credentials.
pub async fn connect(cfg: &Config) -> Result<Github> {
    connect_installation(cfg, None).await
}

/// Connect for a specific installation/context (GitHub App installation id;
/// ignored by backends without an app model).
pub async fn connect_installation(cfg: &Config, installation: Option<u64>) -> Result<Github> {
    ensure_supported(cfg.forge)?;
    Github::for_installation(installation, cfg.forge_base_url.as_deref()).await
}
