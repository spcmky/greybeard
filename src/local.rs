//! Read-only context collection from a local Git working tree.
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Instant;

use anyhow::{bail, Context, Result};

use crate::config::Config;
use crate::github::PrRef;
use crate::pack::{self, BlameRange, ChangedFile, CheckRollup, ContextPack, PackData};

const MAX_FILE_BYTES: u64 = 2_000_000;

pub struct LocalReview {
    pub root: PathBuf,
    pub comparison: String,
    pub pack: ContextPack,
}

struct Entry {
    mode: String,
    oid: String,
    size: u64,
}

fn git(root: &Path, args: &[&str]) -> Result<Output> {
    Command::new("git")
        .current_dir(root)
        .args(["--no-pager", "-c", "core.quotePath=false"])
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_LITERAL_PATHSPECS", "1")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .context("running git (local reviews require Git on PATH)")
}

fn checked(root: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let output = git(root, args)?;
    if !output.status.success() {
        bail!(
            "git {}: {}",
            args[0],
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output.stdout)
}

fn text(root: &Path, args: &[&str]) -> Result<String> {
    Ok(String::from_utf8(checked(root, args)?)?
        .trim_end_matches('\n')
        .to_string())
}

fn paths(raw: &[u8]) -> Result<BTreeSet<String>> {
    raw.split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| String::from_utf8(p.to_vec()).context("local reviews require UTF-8 file names"))
        .collect()
}

/// The default compares HEAD with the working tree. --base uses the merge base
/// of the requested revision and HEAD, including both branch and uncommitted edits.
/// Neither the real index nor the repository's object database is written.
pub fn build_pack(directory: &Path, base: Option<&str>, cfg: &Config) -> Result<LocalReview> {
    let started = Instant::now();
    let directory = directory
        .canonicalize()
        .context("opening local review directory")?;
    let root = PathBuf::from(
        text(&directory, &["rev-parse", "--show-toplevel"])
            .context("local review target must be inside a non-bare Git repository")?,
    );
    if !checked(&root, &["ls-files", "--unmerged", "-z"])?.is_empty() {
        bail!("resolve Git merge conflicts before running a local review");
    }
    let head_output = git(&root, &["rev-parse", "--verify", "HEAD"])?;
    let head = if head_output.status.success() {
        Some(String::from_utf8(head_output.stdout)?.trim().to_string())
    } else {
        // An unborn branch is valid; other HEAD failures are not.
        checked(&root, &["symbolic-ref", "HEAD"]).context("could not resolve HEAD")?;
        None
    };
    let base_sha = match base {
        Some(base) => {
            let head = head
                .as_deref()
                .context("--base requires at least one commit")?;
            let revision = text(
                &root,
                &[
                    "rev-parse",
                    "--verify",
                    "--end-of-options",
                    &format!("{base}^{{commit}}"),
                ],
            )
            .with_context(|| format!("invalid base revision {base:?}"))?;
            Some(
                text(&root, &["merge-base", &revision, head])
                    .with_context(|| format!("no merge base between {base:?} and HEAD"))?,
            )
        }
        None => head.clone(),
    };
    let comparison = match (&base_sha, base) {
        (Some(sha), Some(base)) => format!("merge base with {base} ({}) → working tree", &sha[..7]),
        (Some(sha), None) => format!("HEAD ({}) → working tree", &sha[..7]),
        (None, _) => "empty tree → working tree (no commits yet)".into(),
    };

    let mut entries = BTreeMap::new();
    let mut changed = if let Some(base_sha) = &base_sha {
        let tree = checked(&root, &["ls-tree", "-rlz", "--full-tree", base_sha])?;
        for row in tree.split(|b| *b == 0).filter(|r| !r.is_empty()) {
            let (meta, path) = row.split_at(
                row.iter()
                    .position(|b| *b == b'\t')
                    .context("invalid ls-tree row")?,
            );
            let fields: Vec<_> = std::str::from_utf8(meta)?.split_whitespace().collect();
            entries.insert(
                String::from_utf8(path[1..].to_vec())?,
                Entry {
                    mode: fields[0].into(),
                    oid: fields[2].into(),
                    size: fields[3].parse().unwrap_or(0),
                },
            );
        }
        paths(&checked(
            &root,
            &[
                "diff",
                "--name-only",
                "-z",
                "--no-renames",
                "--no-ext-diff",
                "--no-textconv",
                "--ignore-submodules=none",
                base_sha,
                "--",
            ],
        )?)?
    } else {
        paths(&checked(&root, &["ls-files", "--cached", "-z"])?)?
    };
    changed.extend(paths(&checked(
        &root,
        &["ls-files", "--others", "--exclude-standard", "-z"],
    )?)?);
    // A separate scratch directory holds only bounded file snapshots for diff/blame.
    let scratch = tempfile::tempdir()?;
    let before_path = scratch.path().join("before");
    let after_path = scratch.path().join("after");
    let mut files = Vec::new();
    let mut diff = String::new();
    let mut blames = Vec::new();
    for path in changed {
        let entry = entries.get(&path);
        let (mode, after) = read_worktree(&root, &path)?;
        let before = match entry {
            Some(e) if e.mode != "160000" && e.size <= MAX_FILE_BYTES => {
                Some(checked(&root, &["cat-file", "blob", &e.oid])?)
            }
            Some(_) => None,
            None => Some(Vec::new()),
        };
        let old_mode = entry.map(|e| e.mode.as_str()).unwrap_or("000000");
        if old_mode == mode && before.is_some() && before == after {
            continue;
        }
        let status = if entry.is_none() {
            "added"
        } else if mode == "000000" {
            "removed"
        } else {
            "modified"
        };
        let mut file = ChangedFile {
            path: path.clone(),
            status: status.into(),
            additions: 0,
            deletions: 0,
            changed_ranges: Vec::new(),
            content: None,
        };
        diff.push_str(&format!("diff --git a/{path} b/{path}\n"));
        if old_mode != mode {
            diff.push_str(&format!("old mode {old_mode}\nnew mode {mode}\n"));
        }
        match (&before, &after) {
            (Some(before), Some(after)) if is_text(before) && is_text(after) => {
                fs::write(&before_path, before)?;
                fs::write(&after_path, after)?;
                let output = git(
                    &root,
                    &[
                        "diff",
                        "--no-index",
                        "--no-ext-diff",
                        "--no-textconv",
                        "--no-color",
                        "--unified=3",
                        "--",
                        before_path.to_str().unwrap(),
                        after_path.to_str().unwrap(),
                    ],
                )?;
                if !matches!(output.status.code(), Some(0 | 1)) {
                    bail!(
                        "diffing {path}: {}",
                        String::from_utf8_lossy(&output.stderr)
                    );
                }
                let patch = String::from_utf8(output.stdout)?;
                let hunks = patch
                    .lines()
                    .skip_while(|line| !line.starts_with("@@ "))
                    .collect::<Vec<_>>()
                    .join("\n");
                let old_path = if entry.is_none() {
                    "/dev/null".into()
                } else {
                    format!("a/{path}")
                };
                let new_path = if mode == "000000" {
                    "/dev/null".into()
                } else {
                    format!("b/{path}")
                };
                diff.push_str(&format!("--- {old_path}\n+++ {new_path}\n{hunks}\n"));
                // Parse only hunk coordinates with a fixed header, independent of
                // whitespace/quoting in the real path.
                file.changed_ranges = pack::parse_diff_ranges(&format!("+++ b/file\n{hunks}"))
                    .into_iter()
                    .next()
                    .map(|(_, ranges)| ranges)
                    .unwrap_or_default();
                file.additions = hunks.lines().filter(|l| l.starts_with('+')).count() as u64;
                file.deletions = hunks.lines().filter(|l| l.starts_with('-')).count() as u64;
                if mode != "000000" {
                    file.content = Some(std::str::from_utf8(after)?.to_string());
                }
                if mode.starts_with("100")
                    && head.is_some()
                    && blames.len() < 6
                    && !file.changed_ranges.is_empty()
                {
                    let output = git(
                        &root,
                        &[
                            "blame",
                            "--line-porcelain",
                            "--contents",
                            after_path.to_str().unwrap(),
                            "--",
                            &path,
                        ],
                    )?;
                    if output.status.success() {
                        blames.push((
                            path.clone(),
                            parse_blame(&String::from_utf8(output.stdout)?),
                        ));
                    }
                }
            }
            _ => diff
                .push_str("[contents omitted: binary, non-UTF-8, submodule, or file over 2 MB]\n"),
        }
        files.push(file);
    }

    // Guidance is read only when tracked or non-ignored, using the same safe
    // reader as source files. Include both common repository guidance names.
    let visible = paths(&checked(
        &root,
        &[
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "-z",
        ],
    )?)?;
    let mut guidance_paths = BTreeSet::new();
    for candidate in pack::claude_md_candidates(&files) {
        guidance_paths.insert(candidate.replace("CLAUDE.md", "AGENTS.md"));
        guidance_paths.insert(candidate);
    }
    let mut guidance = Vec::new();
    for path in guidance_paths.into_iter().filter(|p| visible.contains(p)) {
        let (mode, contents) = read_worktree(&root, &path)?;
        if mode.starts_with("100") {
            if let Some(bytes) = contents.filter(|b| is_text(b)) {
                guidance.push((
                    path,
                    Some(pack::cap_lines(
                        std::str::from_utf8(&bytes)?,
                        cfg.max_file_lines,
                    )),
                ));
            }
        }
    }
    let data = PackData {
        pr: PrRef { owner: "local".into(), repo: root.file_name().unwrap_or_default().to_string_lossy().into(), number: 0 },
        pr_node_id: String::new(), head_sha: head.unwrap_or_else(|| "unborn".into()),
        state: "open".into(), draft: false, title: format!("Local review: {comparison}"),
        body: format!("Local Git working-tree review at {}. {comparison}. Includes staged, unstaged, and non-ignored untracked files. File contents and line numbers describe the working tree, not necessarily HEAD. No live CI status or prior PR feedback is available. Renames appear as deletion plus addition. Omitted content is explicitly marked in the diff.", root.display()),
        base_ref: base_sha.unwrap_or_else(|| "empty tree".into()),
        author: "local user".into(), author_is_bot: false, existing_comment: None,
        changed_files: files, diff, checks: CheckRollup::default(),
        claude_mds: guidance, blames, prior_comments: String::new(),
    };
    Ok(LocalReview {
        root,
        comparison,
        pack: data.finish(cfg, started),
    })
}

impl crate::pipeline::verify::Source for LocalReview {
    async fn read_file(&self, path: &str) -> Result<Option<String>> {
        crate::pipeline::verify::validate_path(path)?;
        if let Some(file) = self.pack.changed_files.iter().find(|f| f.path == path) {
            return Ok(file.content.clone());
        }
        if self.pack.head_sha == "unborn" {
            return Ok(None);
        }
        let tree = checked(
            &self.root,
            &["ls-tree", "-lz", &self.pack.head_sha, "--", path],
        )?;
        let row = tree.split(|b| *b == 0).find(|r| !r.is_empty());
        let Some(row) = row else { return Ok(None) };
        let row = std::str::from_utf8(row)?;
        let Some((meta, _)) = row.split_once('\t') else {
            bail!("invalid ls-tree row")
        };
        let fields: Vec<_> = meta.split_whitespace().collect();
        if !fields[0].starts_with("100") || fields[3].parse::<u64>()? > MAX_FILE_BYTES {
            return Ok(None);
        }
        let bytes = checked(&self.root, &["cat-file", "blob", fields[2]])?;
        Ok(is_text(&bytes).then(|| String::from_utf8(bytes).unwrap()))
    }
}

fn is_text(bytes: &[u8]) -> bool {
    !bytes.contains(&0) && std::str::from_utf8(bytes).is_ok()
}

/// Never dereference a symlink or read a special file. Deleted files have an
/// empty new side; directories (submodules) and oversized files have no contents.
fn read_worktree(root: &Path, path: &str) -> Result<(String, Option<Vec<u8>>)> {
    let relative = Path::new(path);
    if relative
        .components()
        .any(|c| !matches!(c, std::path::Component::Normal(_)))
    {
        bail!("invalid repository path: {path:?}");
    }
    let mut parent = root.to_path_buf();
    let components: Vec<_> = relative.components().collect();
    for component in &components[..components.len().saturating_sub(1)] {
        parent.push(component);
        match fs::symlink_metadata(&parent) {
            Ok(meta) if !meta.is_dir() => return Ok(("000000".into(), Some(Vec::new()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(("000000".into(), Some(Vec::new())))
            }
            Err(e) => return Err(e.into()),
            _ => {}
        }
    }
    let full = root.join(relative);
    let meta = match fs::symlink_metadata(&full) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(("000000".into(), Some(Vec::new())))
        }
        Err(e) => return Err(e).with_context(|| format!("reading {path}")),
    };
    if meta.is_symlink() {
        let target = fs::read_link(full)?;
        return Ok((
            "120000".into(),
            Some(target.as_os_str().as_encoded_bytes().to_vec()),
        ));
    }
    if meta.is_dir() {
        return Ok(("160000".into(), None));
    }
    if !meta.is_file() {
        bail!("cannot review special file {path:?}");
    }
    let mut mode = "100644";
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o111 != 0 {
            mode = "100755";
        }
    }
    if meta.len() > MAX_FILE_BYTES {
        return Ok((mode.into(), None));
    }
    let mut bytes = Vec::new();
    fs::File::open(full)?
        .take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    Ok((
        mode.into(),
        (bytes.len() as u64 <= MAX_FILE_BYTES).then_some(bytes),
    ))
}

fn parse_blame(raw: &str) -> Vec<BlameRange> {
    let mut ranges = Vec::new();
    let (mut oid, mut line, mut author, mut date, mut summary) = (
        String::new(),
        0,
        String::new(),
        String::new(),
        String::new(),
    );
    for row in raw.lines() {
        if row.starts_with('\t') {
            if !oid.chars().all(|c| c == '0') {
                ranges.push((
                    line,
                    line,
                    oid.clone(),
                    date.clone(),
                    author.clone(),
                    summary.clone(),
                ));
            }
        } else if let Some(value) = row.strip_prefix("author ") {
            author = value.into();
        } else if let Some(value) = row.strip_prefix("author-time ") {
            date = value.into();
        } else if let Some(value) = row.strip_prefix("summary ") {
            summary = value.into();
        } else {
            let fields: Vec<_> = row.split_whitespace().collect();
            if fields.len() >= 3
                && fields[0].len() >= 40
                && fields[0].bytes().all(|b| b.is_ascii_hexdigit())
            {
                oid = fields[0].into();
                line = fields[2].parse().unwrap_or(0);
            }
        }
    }
    ranges
}
