use anyhow::Result;
use clap::{Parser, Subcommand};

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
    /// Review a pull/merge request and post (or update) the Greybeard comment.
    Review {
        /// PR/MR URL — https://github.com/owner/repo/pull/123 or
        /// https://gitlab.com/group/project/-/merge_requests/123 (GREYBEARD_FORGE).
        pr_url: String,
        /// Print the comment instead of posting it.
        #[arg(long)]
        dry_run: bool,
        /// Review even if closed/draft/already-reviewed/judged-trivial.
        #[arg(long)]
        force: bool,
    },
    /// Build and print the context pack (no model calls) — for debugging and timing.
    Pack {
        pr_url: String,
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
        Command::Review { pr_url, dry_run, force } => {
            let cfg = Config::from_env()?;
            // Connect first so an unimplemented forge fails with the friendly
            // seam error before the GitHub-specific URL parse.
            let gh = forge::connect(&cfg).await?;
            let pr = forge::parse_ref(&cfg, &pr_url)?;
            let telemetry = Telemetry::new();
            let llm = Llm::new(cfg.clone(), telemetry.clone()).await?;
            review::run(&gh, &llm, &cfg, &telemetry, &pr, &ReviewArgs { dry_run, force })
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
        Command::Pack { pr_url } => {
            let cfg = Config::from_env()?;
            let gh = forge::connect(&cfg).await?;
            let pr = forge::parse_ref(&cfg, &pr_url)?;
            let pack = gh.build_pack(&pr, &cfg).await?;
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
