use std::collections::BTreeMap;
use std::path::Path;

use crate::config::Config;
use crate::pack::{self, ContextPack, PackData};
use crate::prompts;

pub struct Job {
    pub key: String,
    pub instruction: String,
    pub context: String,
}

pub fn jobs(pack: &ContextPack, cfg: &Config) -> Vec<Job> {
    let mut diffs: BTreeMap<String, String> = BTreeMap::new();
    let mut path = String::new();
    for line in pack.source.diff.lines() {
        if line.starts_with("diff --git ") {
            path = line.rsplit(" b/").next().unwrap_or("").to_string();
        }
        diffs
            .entry(path.clone())
            .or_default()
            .push_str(&format!("{line}\n"));
    }
    let mut batches: Vec<Vec<crate::pack::ChangedFile>> = Vec::new();
    let mut size = 0;
    for file in &pack.changed_files {
        let cost = diffs.get(&file.path).map_or(0, String::len)
            + file
                .content
                .as_ref()
                .map_or(0, |s| s.len() + 8 * s.lines().count());
        let new_batch = batches.last().is_none_or(|batch| {
            size + cost > 60_000
                || Path::new(&batch[0].path).parent() != Path::new(&file.path).parent()
        });
        if new_batch {
            batches.push(Vec::new());
            size = 0;
        }
        batches.last_mut().unwrap().push(file.clone());
        size += cost;
    }
    let mut limits = cfg.clone();
    limits.max_pack_chars = 80_000;
    limits.max_diff_chars = 40_000;
    let mut jobs = Vec::new();
    for files in batches {
        let mut data: PackData = (*pack.source).clone();
        data.diff = files
            .iter()
            .filter_map(|f| diffs.get(&f.path))
            .cloned()
            .collect();
        data.claude_mds.retain(|(path, _)| {
            files
                .iter()
                .any(|f| super::verify::applicable_guidance(path, &f.path))
        });
        data.blames
            .retain(|(path, _)| files.iter().any(|f| f.path == *path));
        data.changed_files = files;
        let context = pack::render(&data, &limits);
        let mut keys = Vec::new();
        let mut instructions = Vec::new();
        for lens in &prompts::LENSES {
            let relevant = match lens.key {
                "claude-md" => data.claude_mds.iter().any(|(_, text)| text.is_some()),
                "history" => !data.blames.is_empty(),
                "prior-feedback" => !data.prior_comments.trim().is_empty(),
                "ci-config" => data.changed_files.iter().any(|f| is_config(&f.path)),
                _ => true,
            };
            if relevant {
                keys.push(lens.key);
                instructions.push(lens.instruction);
            }
        }
        jobs.push(Job {
            key: keys.join(","),
            instruction: instructions.join("\n\n"),
            context,
        });
    }
    jobs
}

fn is_config(path: &str) -> bool {
    let name = Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    matches!(
        Path::new(path).extension().and_then(|s| s.to_str()),
        Some("yml" | "yaml" | "sh" | "bash" | "tf" | "conf" | "toml" | "ini" | "bazel" | "bzl")
    ) || name.starts_with("Dockerfile")
        || name.starts_with("Caddyfile")
}
