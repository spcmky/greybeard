use std::{fs, path::Path, process::Command, sync::Arc};

use axum::{extract::State, routing::post, Json, Router};
use greybeard::{
    config::{Config, Provider},
    llm::Llm,
    local::{build_pack, LocalReview},
    telemetry::Telemetry,
};
use serde_json::{json, Value};

pub fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(root)
        .args([
            "-c",
            "user.name=Review Test",
            "-c",
            "user.email=review@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

pub fn repository(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-b", "main"]);
    git(dir.path(), &["commit", "--allow-empty", "-m", "base"]);
    for (name, text) in files {
        write(dir.path(), name, text);
    }
    dir
}

pub fn write(root: &Path, name: &str, text: &str) {
    let path = root.join(name);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

pub fn snapshot(dir: &tempfile::TempDir) -> LocalReview {
    build_pack(dir.path(), None, &Config::for_pack()).unwrap()
}

pub struct Model {
    pub cfg: Config,
    pub llm: Llm,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Model {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub async fn model(reply: impl Fn(&Value) -> Value + Send + Sync + 'static) -> Model {
    type Reply = Arc<dyn Fn(&Value) -> Value + Send + Sync>;
    let reply: Reply = Arc::new(reply);
    let app = Router::new().route("/v1/chat/completions", post(
        |State(reply): State<Reply>, Json(body): Json<Value>| async move {
            Json(json!({"choices": [{"finish_reason": "stop", "message": {"content": reply(&body).to_string()}}]}))
        }
    )).with_state(reply);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut cfg = Config::for_pack();
    cfg.provider = Provider::OpenAi;
    cfg.openai_base_url = Some(format!("http://{}/v1", listener.local_addr().unwrap()));
    cfg.lens_model = "test".into();
    cfg.verify_model = "test".into();
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let llm = Llm::new(cfg.clone(), Telemetry::new()).await.unwrap();
    Model { cfg, llm, task }
}

pub fn confirmed() -> Value {
    json!({
        "citations": [{"file": "stats.py", "line": 2, "quote": "    return sum(values) / (len(values) - 1)"}],
        "trigger": "average([2, 4])", "expected": "3", "actual": "6",
        "safeguards": "The public helper has no denominator validation", "reason": "The denominator is one too small",
        "requested_files": [], "status": "confirmed", "confidence": 100, "severity": "gap"
    })
}
