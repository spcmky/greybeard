use anyhow::Result;
use clap::{Parser, Subcommand};

use greybeard::config::Config;
use greybeard::forge;
use greybeard::github::{self, PrRef};
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
    /// Review a pull request and post (or update) the Greybeard comment.
    Review {
        /// PR URL, e.g. https://github.com/owner/repo/pull/123
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
    /// Verify GitHub credentials (App or user token) and print the auth mode.
    AuthCheck,
    /// Run the webhook service (GitHub App events -> reviews).
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
            let pr = PrRef::parse(&pr_url)?;
            let gh = forge::connect(&cfg).await?;
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
            println!("auth mode: {}", gh.auth_mode);
            match gh.graphql("query{viewer{login}}", serde_json::json!({})).await {
                Ok(d) => println!("authenticated as: {}", d["viewer"]["login"].as_str().unwrap_or("?")),
                // Installation tokens can't resolve `viewer`; the successful
                // token exchange above already proves the App credentials.
                Err(e) => println!("viewer query: {e} (normal for app installation tokens)"),
            }
            Ok(())
        }
        Command::Pack { pr_url } => {
            let cfg = Config::from_env()?;
            let pr = PrRef::parse(&pr_url)?;
            let gh = forge::connect(&cfg).await?;
            let pack = github::pack::build(&gh, &pr, &cfg).await?;
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
