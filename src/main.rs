use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use std::path::Path;

use greybeard::config::Config;
use greybeard::forge::{self, Forge};
use greybeard::llm::Llm;
use greybeard::pipeline::review::{self, ReviewArgs};
use greybeard::telemetry::Telemetry;

/// Greybeard — the mythical senior engineer who's seen every failure mode.
#[derive(Parser)]
#[command(name = "greybeard", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Review a local Git directory or a pull/merge request URL.
    Review {
        /// Local Git directory (default: .) or PR/MR URL.
        #[arg(default_value = ".")]
        target: String,
        /// Compare local changes from the merge base with this revision.
        #[arg(long)]
        base: Option<String>,
        /// Print a remote review instead of posting (local reviews always print).
        #[arg(long)]
        dry_run: bool,
        /// Review even if closed/draft/already-reviewed/judged-trivial.
        #[arg(long)]
        force: bool,
    },
    /// Build and print the context pack (no model calls) — for debugging and timing.
    Pack {
        /// Local Git directory (default: .) or PR/MR URL.
        #[arg(default_value = ".")]
        target: String,
        /// Compare local changes from the merge base with this revision.
        #[arg(long)]
        base: Option<String>,
    },
    /// Verify forge credentials and print the auth mode + identity.
    AuthCheck,
    /// Run the webhook service (GitHub or GitLab events -> reviews).
    Serve {
        #[arg(long, default_value_t = 8080)]
        port: u16,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Review {
            target,
            base,
            dry_run,
            force,
        } => {
            if !is_remote(&target) {
                let local = greybeard::local::build_pack(
                    Path::new(&target),
                    base.as_deref(),
                    &Config::for_pack(),
                )?;
                if local.pack.changed_files.is_empty() {
                    println!("skipped: no changed files");
                    return Ok(());
                }
                let cfg = Config::from_env()?;
                let telemetry = Telemetry::new();
                let llm = Llm::new(cfg.clone(), telemetry.clone()).await?;
                return review::run_local(&local, &llm, &cfg, &telemetry, force)
                    .await
                    .map(|_| ());
            }
            if base.is_some() {
                bail!("--base is only supported for local Git reviews");
            }
            let cfg = Config::from_env()?;
            // Connect first so an unimplemented forge fails with the friendly
            // seam error before the GitHub-specific URL parse.
            let gh = forge::connect(&cfg).await?;
            let pr = forge::parse_ref(&cfg, &target)?;
            let telemetry = Telemetry::new();
            let llm = Llm::new(cfg.clone(), telemetry.clone()).await?;
            review::run(
                &gh,
                &llm,
                &cfg,
                &telemetry,
                &pr,
                &ReviewArgs { dry_run, force },
            )
            .await
            .map(|_| ())
        }
        Command::Serve { port } => {
            let cfg = Config::from_env()?;
            greybeard::server::serve(cfg, port).await
        }
        Command::AuthCheck => {
            let cfg = Config::from_env()?;
            let gh = forge::connect(&cfg).await?;
            println!("auth mode: {}", gh.auth_mode());
            match gh.whoami().await {
                Ok(login) => println!("authenticated as: {login}"),
                // Installation tokens can't resolve an identity; the successful
                // connect above already proves the credentials.
                Err(e) => println!("identity query: {e} (normal for app installation tokens)"),
            }
            Ok(())
        }
        Command::Pack { target, base } => {
            let pack = if is_remote(&target) {
                if base.is_some() {
                    bail!("--base is only supported for local Git reviews");
                }
                let cfg = Config::from_env()?;
                let gh = forge::connect(&cfg).await?;
                let pr = forge::parse_ref(&cfg, &target)?;
                gh.build_pack(&pr, &cfg).await?
            } else {
                greybeard::local::build_pack(
                    Path::new(&target),
                    base.as_deref(),
                    &Config::for_pack(),
                )?
                .pack
            };
            eprintln!(
                "pack: {} chars, {} files, fetched in {}ms",
                pack.rendered.len(),
                pack.changed_files.len(),
                pack.fetch_ms
            );
            println!("{}", pack.rendered);
            Ok(())
        }
    }
}

fn is_remote(target: &str) -> bool {
    target.starts_with("https://") || target.starts_with("http://")
}
