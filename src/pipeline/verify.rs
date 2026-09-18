use std::collections::BTreeMap;
use std::path::{Component, Path};

use anyhow::{bail, Context, Result};

use crate::config::Config;
use crate::forge::Forge;
use crate::llm::{Llm, Tier};
use crate::pack::ContextPack;
use crate::prompts;

use super::{Finding, Verdict, VerdictStatus};

const MAX_CONTEXT_BYTES: usize = 80_000;
const MAX_FILES: usize = 8;
const MAX_ROUNDS: usize = 3;

#[allow(async_fn_in_trait)]
pub trait Source {
    async fn read_file(&self, path: &str) -> Result<Option<String>>;
}

pub struct RemoteSource<'a, F> {
    pub forge: &'a F,
    pub pack: &'a ContextPack,
}

impl<F: Forge> Source for RemoteSource<'_, F> {
    async fn read_file(&self, path: &str) -> Result<Option<String>> {
        validate_path(path)?;
        self.forge
            .file_contents(&self.pack.pr, path, &self.pack.head_sha)
            .await
    }
}

pub fn validate_path(path: &str) -> Result<()> {
    if path
        .split('/')
        .any(|part| matches!(part, "" | "." | ".." | ".git"))
        || Path::new(path)
            .components()
            .any(|c| !matches!(c, Component::Normal(name) if name != ".git"))
    {
        bail!("invalid repository path: {path:?}");
    }
    Ok(())
}

fn numbered(path: &str, text: &str) -> String {
    let mut out = format!("<file path={}>\n", serde_json::to_string(path).unwrap());
    for (i, line) in text.lines().enumerate() {
        out.push_str(&format!("{:>5}| {line}\n", i + 1));
    }
    out.push_str("</file>\n");
    out
}

fn validate_citations(verdict: &mut Verdict, files: &BTreeMap<String, String>) -> Result<()> {
    if verdict.citations.is_empty() {
        bail!("a factual verdict requires exact source citations");
    }
    for citation in &mut verdict.citations {
        let text = files
            .get(&citation.file)
            .context("citation names an unread file")?;
        if citation.line == 0 || citation.quote.trim().is_empty() {
            bail!("citation needs a positive line number and a nonempty quote");
        }
        let count = citation.quote.lines().count();
        let actual = text
            .lines()
            .skip(citation.line as usize - 1)
            .take(count)
            .collect::<Vec<_>>()
            .join("\n");
        if actual.lines().map(str::trim).collect::<Vec<_>>()
            != citation.quote.lines().map(str::trim).collect::<Vec<_>>()
        {
            bail!(
                "quote does not match current source at {}:{}: supplied {:?}, current {:?}",
                citation.file,
                citation.line,
                citation.quote,
                actual
            );
        }
        citation.quote = actual;
    }
    Ok(())
}

fn validate_verdict(
    verdict: &mut Verdict,
    files: &BTreeMap<String, String>,
    finding: &Finding,
    pack: &ContextPack,
    cfg: &Config,
) -> Result<()> {
    if verdict.status == VerdictStatus::Unverified {
        return Ok(());
    }
    if verdict.reason.trim().is_empty() || verdict.confidence > 100 {
        bail!("verdict needs a reason and confidence between 0 and 100");
    }
    if verdict.confidence < cfg.confidence_threshold {
        verdict.status = VerdictStatus::Unverified;
        verdict.reason = format!("Insufficient evidence confidence: {}", verdict.reason);
        return Ok(());
    }
    validate_citations(verdict, files)?;
    if verdict.status == VerdictStatus::Refuted {
        return Ok(());
    }
    if !matches!(verdict.severity.as_str(), "blocker" | "gap" | "nit") {
        bail!("invalid verified severity");
    }
    if [
        &verdict.trigger,
        &verdict.expected,
        &verdict.actual,
        &verdict.safeguards,
    ]
    .iter()
    .any(|s| s.trim().is_empty())
    {
        bail!("confirmation requires a trigger, expected/actual behavior, and safeguard analysis");
    }
    let anchor = &verdict.citations[0];
    let file = pack
        .changed_files
        .iter()
        .find(|f| f.path == finding.file)
        .context("finding names an unchanged file")?;
    if anchor.file != finding.file
        || !file
            .changed_ranges
            .iter()
            .any(|(start, end)| (*start..=*end).contains(&anchor.line))
    {
        bail!("first citation must anchor the defect in the reviewed file's new-side diff");
    }
    Ok(())
}

pub async fn verify<S: Source>(
    source: &S,
    pack: &ContextPack,
    llm: &Llm,
    cfg: &Config,
    lens: &str,
    finding: &Finding,
) -> Result<Verdict> {
    validate_path(&finding.file)?;
    let file = pack
        .changed_files
        .iter()
        .find(|f| f.path == finding.file)
        .context("finding names an unchanged file")?;
    let text = source
        .read_file(&finding.file)
        .await?
        .context("current source is unavailable")?;
    let mut files = BTreeMap::from([(finding.file.clone(), text)]);
    let mut notes = String::new();
    for (path, content) in &pack.source.claude_mds {
        if content.is_some() && applicable_guidance(path, &finding.file) {
            if let Some(content) = source.read_file(path).await? {
                files.insert(path.clone(), content);
            }
        }
    }
    if files.len() > MAX_FILES {
        bail!("verification exceeded its file budget");
    }
    let mut requested: Vec<String> = Vec::new();
    for round in 0..MAX_ROUNDS {
        for path in requested.drain(..) {
            validate_path(&path)?;
            if files.contains_key(&path) {
                continue;
            }
            if files.len() >= MAX_FILES {
                bail!("verification exceeded its file budget");
            }
            match source.read_file(&path).await? {
                Some(content) => {
                    files.insert(path, content);
                }
                None => notes.push_str(&format!("Source unavailable: {path}\n")),
            }
        }
        let mut context = format!("Reviewed revision: {}. File contents are the current side of the review. Deleted diff lines are historical, not current code.\n", pack.head_sha);
        context.push_str("Changed files available for requested_files:\n");
        for file in &pack.changed_files {
            context.push_str(&format!("{}\n", file.path));
        }
        context.push_str("\nRequest other repository paths when callers, helpers, or tests are needed. No execution tools are available; never claim to have run a test.\n");
        for (path, content) in &files {
            context.push_str(&numbered(path, content));
        }
        context.push_str(&format!(
            "\nNew-side changed ranges for {}: {:?}\n",
            file.path, file.changed_ranges
        ));
        context.push_str(&notes);
        if context.len() > MAX_CONTEXT_BYTES {
            bail!("verification source exceeds the focused context budget");
        }
        let mut verdict: Verdict = llm
            .structured(
                Tier::Verify,
                &format!("verify:{}:{}", finding.file, round + 1),
                &prompts::verification_blocks(&context),
                &prompts::verify_user_message(
                    lens,
                    &finding.file,
                    finding.line,
                    &finding.severity,
                    &finding.claim,
                    &finding.evidence,
                ),
                &prompts::verdict_schema(),
            )
            .await?;
        if !verdict.requested_files.is_empty() {
            requested = verdict.requested_files;
            if requested.len() > MAX_FILES {
                bail!("verifier requested too many files");
            }
            continue;
        }
        match validate_verdict(&mut verdict, &files, finding, pack, cfg) {
            Ok(()) => return Ok(verdict),
            Err(error) => notes.push_str(&format!("Previous verdict failed source validation: {error}. Correct the evidence or return unverified.\n")),
        }
    }
    bail!("verification did not produce a supported verdict within {MAX_ROUNDS} rounds: {notes}")
}

pub fn applicable_guidance(guidance: &str, file: &str) -> bool {
    Path::new(guidance)
        .parent()
        .is_some_and(|parent| Path::new(file).starts_with(parent))
}

pub fn evidence(verdict: &Verdict) -> String {
    let citations = verdict
        .citations
        .iter()
        .map(|c| format!("{}:{}: {}", c.file, c.line, c.quote.trim()))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "{}\nTrigger: {}\nExpected: {}\nActual: {}\nSafeguards: {}\n{}",
        verdict.reason,
        verdict.trigger,
        verdict.expected,
        verdict.actual,
        verdict.safeguards,
        citations
    )
}
