use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

use greybeard::{config::Config, local::build_pack};

fn git(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(root)
        .args([
            "-c",
            "user.name=Local Test",
            "-c",
            "user.email=local@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

fn repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-b", "main"]);
    dir
}

fn commit(root: &Path) {
    git(root, &["add", "."]);
    git(root, &["commit", "-m", "fixture"]);
}

fn cli(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_greybeard"))
        .current_dir(root)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap())
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn working_tree_pack_includes_net_edits_and_preserves_index() {
    let dir = repo();
    let root = dir.path();
    fs::write(root.join("code.rs"), "old\ncontext\n").unwrap();
    fs::write(root.join("gone.rs"), "delete me\n").unwrap();
    fs::write(root.join(".gitignore"), "secret.env\nignored/\n").unwrap();
    fs::write(root.join("AGENTS.md"), "Preserve the code contract.\n").unwrap();
    commit(root);
    fs::write(root.join("code.rs"), "staged\ncontext\n").unwrap();
    git(root, &["add", "code.rs"]);
    fs::write(root.join("code.rs"), "working\ncontext\n").unwrap();
    fs::remove_file(root.join("gone.rs")).unwrap();
    fs::write(root.join("new café file.rs"), "new code\n").unwrap();
    fs::write(root.join("secret.env"), "DO_NOT_INCLUDE\n").unwrap();
    fs::create_dir(root.join("ignored")).unwrap();
    fs::write(root.join("ignored/hidden"), "DO_NOT_INCLUDE\n").unwrap();
    let index = fs::read(root.join(".git/index")).unwrap();
    let pack = build_pack(root, None, &Config::for_pack()).unwrap().pack;
    assert_eq!(pack.changed_files.len(), 3);
    assert!(pack.rendered.contains("+working"));
    assert!(!pack.rendered.contains("+staged"));
    assert!(pack.rendered.contains("-delete me"));
    assert!(pack.rendered.contains("Preserve the code contract."));
    assert!(!pack.rendered.contains("DO_NOT_INCLUDE"));
    let new = pack
        .changed_files
        .iter()
        .find(|f| f.path == "new café file.rs")
        .unwrap();
    assert_eq!(new.changed_ranges, [(1, 1)]);
    assert_eq!(new.content.as_deref(), Some("new code\n"));
    assert_eq!(fs::read(root.join(".git/index")).unwrap(), index);
    assert_eq!(
        pack.rendered,
        build_pack(root, None, &Config::for_pack())
            .unwrap()
            .pack
            .rendered
    );
    // git rm --cached followed by an untracked replacement is a net modification.
    git(root, &["rm", "--cached", "gone.rs"]);
    fs::write(root.join("gone.rs"), "replacement\n").unwrap();
    let pack = build_pack(root, None, &Config::for_pack()).unwrap().pack;
    assert!(pack.rendered.contains("+replacement"));
    assert_eq!(
        pack.changed_files
            .iter()
            .find(|f| f.path == "gone.rs")
            .unwrap()
            .status,
        "modified"
    );
}

#[test]
fn base_uses_merge_base_and_subdirectory_selects_whole_repo() {
    let dir = repo();
    let root = dir.path();
    fs::write(root.join("base.rs"), "base\n").unwrap();
    commit(root);
    git(root, &["checkout", "-b", "feature"]);
    fs::write(root.join("feature.rs"), "feature\n").unwrap();
    commit(root);
    git(root, &["checkout", "main"]);
    fs::write(root.join("upstream.rs"), "upstream only\n").unwrap();
    commit(root);
    git(root, &["checkout", "feature"]);
    fs::create_dir(root.join("nested")).unwrap();
    fs::write(root.join("nested/dirty.rs"), "dirty\n").unwrap();
    let local = build_pack(&root.join("nested"), Some("main"), &Config::for_pack()).unwrap();
    assert_eq!(local.root, root.canonicalize().unwrap());
    let paths: Vec<_> = local
        .pack
        .changed_files
        .iter()
        .map(|f| f.path.as_str())
        .collect();
    assert_eq!(paths, ["feature.rs", "nested/dirty.rs"]);
    assert!(!local.pack.rendered.contains("upstream.rs"));
    assert_eq!(
        build_pack(root, None, &Config::for_pack())
            .unwrap()
            .pack
            .changed_files
            .len(),
        1
    );
    assert!(build_pack(root, Some("not-a-ref"), &Config::for_pack()).is_err());
}

#[test]
fn handles_unborn_empty_binary_and_oversized_files_without_credentials() {
    let dir = repo();
    let root = dir.path();
    fs::write(root.join("first.rs"), "first\n").unwrap();
    fs::write(root.join("empty.rs"), "").unwrap();
    fs::write(root.join("binary"), b"hidden\0bytes").unwrap();
    fs::File::create(root.join("large"))
        .unwrap()
        .set_len(2_000_001)
        .unwrap();
    git(root, &["add", "first.rs"]);
    let out = cli(root, &["pack"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let pack = String::from_utf8(out.stdout).unwrap();
    assert!(pack.contains("empty tree"));
    assert!(pack.contains("first.rs"));
    assert!(pack.contains("empty.rs"));
    assert!(pack.contains("contents omitted"));
    assert!(!pack.contains("hidden"));
    let invalid = cli(root, &["pack", ".", "--base", "main"]);
    assert!(!invalid.status.success());
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("at least one commit"));
}

#[cfg(unix)]
#[test]
fn symlinks_and_replaced_parent_directories_never_leak_target_contents() {
    use std::os::unix::fs::symlink;
    let dir = repo();
    let root = dir.path();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("secret"), "DO_NOT_READ_TARGET\n").unwrap();
    fs::create_dir(root.join("parent")).unwrap();
    fs::write(root.join("parent/secret"), "old public\n").unwrap();
    commit(root);
    fs::remove_file(root.join("parent/secret")).unwrap();
    fs::remove_dir(root.join("parent")).unwrap();
    symlink(outside.path(), root.join("parent")).unwrap();
    symlink(outside.path().join("secret"), root.join("link")).unwrap();
    symlink(outside.path().join("secret"), root.join("CLAUDE.md")).unwrap();
    let local = build_pack(root, None, &Config::for_pack()).unwrap();
    assert!(!local.pack.rendered.contains("DO_NOT_READ_TARGET"));
    assert!(local.pack.rendered.contains("-old public"));
    assert!(local.pack.rendered.contains("120000"));
}

#[test]
fn empty_reviews_skip_without_model_setup_and_invalid_inputs_fail() {
    let dir = repo();
    let root = dir.path();
    fs::write(root.join("base"), "base\n").unwrap();
    commit(root);
    let out = cli(root, &["review"]);
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8(out.stdout).unwrap().trim(),
        "skipped: no changed files"
    );
    let non_repo = tempfile::tempdir().unwrap();
    assert!(!cli(non_repo.path(), &["pack"]).status.success());
    let out = cli(
        root,
        &["review", "https://github.com/a/b/pull/1", "--base", "main"],
    );
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("--base is only supported"));
}

#[tokio::test]
async fn local_cli_runs_lenses_and_verifies_findings_without_forge_auth() {
    use axum::{extract::State, routing::post, Json, Router};
    use serde_json::{json, Value};
    let dir = repo();
    let root = dir.path();
    fs::write(root.join("code.rs"), "old\n").unwrap();
    commit(root);
    fs::write(root.join("code.rs"), "bug\n").unwrap();
    let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let app = Router::new().route("/v1/chat/completions", post(
        |State(requests): State<Arc<Mutex<Vec<Value>>>>, Json(body): Json<Value>| async move {
            let schema = &body["response_format"]["json_schema"]["schema"]["properties"];
            let content = if !schema["skip"].is_null() {
                json!({"skip": false, "reason": "code changed"})
            } else if !schema["status"].is_null() {
                json!({"status": "confirmed", "confidence": 95, "severity": "blocker", "reason": "The changed value violates the contract", "citations": [{"file": "code.rs", "line": 1, "quote": "bug"}], "trigger": "Read the changed value", "expected": "old", "actual": "bug", "safeguards": "No validation exists", "requested_files": []})
            } else {
                json!({"findings": [{"file": "code.rs", "line": 1, "claim": "A verified defect", "severity": "blocker", "evidence": "code.rs:1 contains bug"}]})
            };
            requests.lock().unwrap().push(body);
            Json(json!({"choices": [{"finish_reason": "stop", "message": {"content": content.to_string()}}], "usage": {"prompt_tokens": 10, "completion_tokens": 5}}))
        }
    )).with_state(requests.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_greybeard"))
        .current_dir(root)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap())
        .env("GREYBEARD_PROVIDER", "openai")
        .env("GREYBEARD_OPENAI_BASE_URL", base)
        .env("GREYBEARD_LENS_MODEL", "test-model")
        // A forge connection would fail on these unusable App credentials.
        .env("GREYBEARD_APP_ID", "123")
        .env("GREYBEARD_APP_PRIVATE_KEY", "/nonexistent")
        .args(["review", "."])
        .output()
        .await
        .unwrap();
    server.abort();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = String::from_utf8(output.stdout).unwrap();
    assert!(report.contains("Greybeard local review"));
    assert!(report.contains("[blocker] code.rs:1"));
    assert!(report.contains("1 confirmed, 0 minor"));
    assert!(report.contains("Trigger: Read the changed value"));
    assert!(report.contains("Expected: old"));
    assert!(report.contains("Actual: bug"));
    assert!(report.contains("You shall not pass."));
    assert!(!report.contains("github.com"));
    assert!(!report.contains("<!-- greybeard:"));
}
