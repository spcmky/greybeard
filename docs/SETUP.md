# Setup

Greybeard runs in two modes. Pick the one that matches how you want reviews to happen:

| Mode | What it does | When to use |
| --- | --- | --- |
| **1. CLI review** | Review a local Git directory in the terminal, or a PR/MR URL with optional posting. | Uncommitted edits, branch reviews, ad-hoc PR reviews. |
| **2. Automatic PR review** | A GitHub App + webhook service reviews every PR automatically on open and on each push. | Team-wide, hands-off review on every PR in a repo. |

All reviews share the same pipeline and [model provider](#model-provider). Local directory reviews need Git but no forge credentials. URL and automatic reviews also need forge access; [GitHub App](#github-app-comments-post-as-the-bot) authentication is optional for CLI URL reviews.

Greybeard supports both **GitHub** and **GitLab**, selected with `GREYBEARD_FORGE` (default `github`). The sections below are written for GitHub; for GitLab merge requests, read [GitLab](#gitlab) — the pipeline is identical, only the coordinates and webhook wiring differ.

---

## Prerequisites (both modes)

1. **A build of the binary.** Either a local Rust toolchain or Docker:

   ```sh
   # Native
   cargo build --release          # -> target/release/greybeard

   # Or via Docker (no local toolchain)
   docker build -t greybeard .
   ```

2. **A model provider** — see [Model provider](#model-provider) below.

3. **Git** on PATH for local directory reviews. For URL and automatic reviews, **forge access** — GitHub (a user token for local mode, or a GitHub App for automatic mode), or GitLab (an access token); see [GitLab](#gitlab).

---

## Model provider

Greybeard talks to Claude through Anthropic or Amazon Bedrock, and to local models through an OpenAI-compatible Chat Completions API. The provider is auto-detected: if `ANTHROPIC_API_KEY` is set it uses Anthropic, otherwise Bedrock. Select a provider explicitly with `GREYBEARD_PROVIDER=anthropic|bedrock|openai`.

### Anthropic (simplest)

```sh
export ANTHROPIC_API_KEY=sk-ant-...
# GREYBEARD_LENS_MODEL defaults to claude-opus-5
```

### Bedrock

Uses the standard AWS credential chain (env vars, `~/.aws`, or instance role). The lens model is **required** and must be a Bedrock inference-profile ID:

```sh
export GREYBEARD_PROVIDER=bedrock
export GREYBEARD_LENS_MODEL=us.anthropic.claude-opus-5
export AWS_REGION=us-east-2          # default if unset
# plus normal AWS creds (AWS_ACCESS_KEY_ID/…, or a mounted ~/.aws, or a role)
```

See the full [configuration table](#configuration-reference) for model, threshold, and cost knobs.

### Local model (OpenAI-compatible)

The DGX Spark server runs qwen3-coder-next:

```sh
export GREYBEARD_PROVIDER=openai
export GREYBEARD_OPENAI_BASE_URL=http://dgx-spark1.fiber.house:8000/v1
export GREYBEARD_LENS_MODEL=qwen3-coder-next
# Verification uses the same model unless GREYBEARD_VERIFY_MODEL is set.
# This server needs no API key. For authenticated servers, optionally set:
# export GREYBEARD_OPENAI_API_KEY=...
export GREYBEARD_MODEL_MAX_CONCURRENT=1
export GREYBEARD_MAX_CONCURRENT=1    # one review at a time in service mode
```

Both the URL (including `http://` or `https://` and `/v1`) and model are required.
The server must support `response_format.type=json_schema`; this works with
[llama.cpp's Chat Completions endpoint](https://github.com/ggml-org/llama.cpp/tree/master/tools/server#post-v1chatcompletions-openai-compatible-chat-completions-api).
No Anthropic or AWS credentials are needed. Prompt caching is managed by the
server: Greybeard skips the Anthropic prefill-only warmup and reports cached
tokens when the server returns them.

The DGX server has one inference slot. `GREYBEARD_MODEL_MAX_CONCURRENT=1`
(the default for `openai`) queues requests in Greybeard before their HTTP
timeouts begin. Increase it only when the model server has more capacity.
Relevant lenses run over batches of related files. Serial generation can take longer than the
hosted-provider review times quoted in the README.

To save these settings, copy the exports into `greybeard.env` (git-ignored).
Docker Compose reads this file automatically. For the native CLI, load it first:

```sh
set -a
. ./greybeard.env
set +a
cargo run -- review https://github.com/OWNER/REPO/pull/N --dry-run
```

Forge authentication is required for PR/MR URLs, but not local directory reviews. To check just the model
connection and all three review schemas, without forge access or posting:

```sh
cargo test --test openai_live -- --ignored --nocapture
```

---

## Mode 1 — CLI review

### Review a local directory

Configure a model provider above, then run:

```sh
# Current working tree: staged, unstaged, and non-ignored untracked files
greybeard review /path/to/repo

# Include branch commits since the merge base with main
greybeard review /path/to/repo --base main

# Inspect the pack without model configuration or credentials
greybeard pack /path/to/repo --base main
```

Omit the path to use the current directory. The target must be in a Git working
tree, and selecting a subdirectory reviews the whole repository. The default
comparison is HEAD; `--base` uses a merge base, including local working-tree edits.
Refs are resolved locally without fetching. New repositories work before their
first commit when `--base` is omitted. Resolve merge conflicts before reviewing.

Reports print locally with `file:line` coordinates, and never post to a forge.
No remote or forge authentication is needed. `--dry-run` is accepted but redundant;
`--force` overrides the trivial-change skip. An empty comparison skips without
model setup. The index and working files are not modified.

Tracked and non-ignored guidance files (`CLAUDE.md` and `AGENTS.md`) and local
blame are included when available. Live CI and prior PR feedback are unavailable.
Binary files, submodules, and files larger than 2 MB are marked as omitted;
renames appear as deletion plus addition. Normal pack size limits also apply.
The code is sent to the configured model provider, including hosted providers.

### Review a PR/MR URL

The following steps review a PR from your machine.

### 1. Authenticate to GitHub

Greybeard uses, in order: `GITHUB_TOKEN`, then `GH_TOKEN`, then whatever `gh auth token` returns. The simplest path is the GitHub CLI:

```sh
gh auth login          # once
```

The token needs `repo` scope (read the PR + diff + file contents, and post the comment). Comments post as **you** unless you configure the GitHub App (see [below](#github-app-comments-post-as-the-bot)).

### 2. Set your provider creds

```sh
export ANTHROPIC_API_KEY=sk-ant-...        # or configure Bedrock, see above
```

### 3. Run a review

```sh
# Print the review — makes no changes on GitHub:
greybeard review https://github.com/OWNER/REPO/pull/N --dry-run

# Post (or update in place) the single Greybeard comment on the PR:
greybeard review https://github.com/OWNER/REPO/pull/N
```

Useful flags and subcommands:

| Command | Purpose |
| --- | --- |
| `review [directory] [--base REV]` | Review local Git changes in the terminal. |
| `pack [directory] [--base REV]` | Print local context without model calls or credentials. |
| `review <pr-url> --dry-run` | Print the comment instead of posting. |
| `review <pr-url> --force` | Review even if the PR is closed / draft / already reviewed at this SHA / judged trivial. |
| `pack <pr-url>` | Print the context pack only — no model calls. For debugging/timing. |
| `auth-check` | Verify GitHub credentials and print the auth mode (`app` vs `user token`). |

### Running from Docker

```sh
docker run --rm \
  -e ANTHROPIC_API_KEY \
  -e GITHUB_TOKEN=$(gh auth token) \
  greybeard review https://github.com/OWNER/REPO/pull/N --dry-run
```

> The default container command is `serve`; pass `review …` (as above) to override it for a one-shot review.

---

## Mode 2 — Automatic review on PR creation and updates

A long-running service (`greybeard serve`) receives GitHub App webhooks and reviews PRs as they open and change. One deployment serves every repo the App is installed on — the installation ID is read from each webhook payload.

**Which events trigger a review:**
- `pull_request`: `opened`, `synchronize` (new push), `ready_for_review`, `reopened`. Drafts are skipped until marked ready.
- `issue_comment`: a comment starting with `@greybeard-bot review` on a PR forces a re-review (per-user 10-minute cooldown).

Rapid pushes are debounced — a new push aborts the in-flight review and restarts on the newest head.

### 1. Create the GitHub App

Repo/org **Settings → Developer settings → GitHub Apps → New GitHub App**.

- **Webhook URL:** `https://<your-host>/webhook`
- **Webhook secret:** a strong random string — you'll pass the same value as `GREYBEARD_WEBHOOK_SECRET`. Deliveries are HMAC-verified; without a matching secret every delivery is rejected.
- **Repository permissions:**

  | Permission | Access | Why |
  | --- | --- | --- |
  | Pull requests | Read & write | Read the PR; post/update the review comment. |
  | Contents | Read-only | Fetch changed-file contents and blame. |
  | Checks / Commit statuses | Read-only | Include the CI rollup in the pack (the ci-config lens). |
  | Metadata | Read-only | Mandatory baseline. |

- **Subscribe to events:** **Pull request** and **Issue comment**.
- **Where can it be installed:** your choice (this account only, or any).

After creating it:
1. **Generate a private key** — downloads a `.pem`. Keep it secret; you'll point `GREYBEARD_APP_PRIVATE_KEY` at it.
2. Note the **App ID** (`GREYBEARD_APP_ID`).
3. **Install the App** on the target repo(s). The bot comments will post as `<app-slug>[bot]`.

### 2. Configure and run the service

Required environment:

```sh
# GitHub App identity
export GREYBEARD_APP_ID=123456
export GREYBEARD_APP_PRIVATE_KEY=/secrets/greybeard.pem   # path to the .pem
export GREYBEARD_WEBHOOK_SECRET=<same secret as the App webhook>

# Provider (Anthropic shown; Bedrock also supported — see above)
export ANTHROPIC_API_KEY=sk-ant-...

# Only if your App slug isn't "greybeard-bot": set so the @-mention channel
# and self-comment filtering match your bot's login.
export GREYBEARD_BOT_LOGIN='<app-slug>[bot]'
```

Run it — three ways:

```sh
# a) Binary directly
greybeard serve --port 8080

# b) docker-compose (local / single host) — see docker-compose.yml
cp greybeard.env.example greybeard.env    # fill it in
docker compose up --build

# c) Kubernetes — the bundled Helm chart (helm/greybeard); see below
```

#### Deploy on Kubernetes (Helm)

The chart is at [`helm/greybeard`](../helm/greybeard). It ships a Deployment, Service, optional Ingress (path-limited to `/webhook` + `/health`), and an optional Prometheus `ServiceMonitor` for `/metrics`. Sensitive values are read from a Secret you create out of band — they never live in `values.yaml`.

```sh
# 1. Secrets (webhook secret + provider creds)
kubectl create secret generic greybeard-secrets \
  --from-literal=GREYBEARD_WEBHOOK_SECRET=<secret> \
  --from-literal=ANTHROPIC_API_KEY=sk-ant-...

# 2. (App auth) the GitHub App private key
kubectl create secret generic greybeard-app-key \
  --from-file=app-private-key=greybeard.pem

# 3. Install
helm install greybeard ./helm/greybeard \
  --set image.repository=<your-registry>/greybeard \
  --set image.tag=<tag> \
  --set githubApp.enabled=true --set githubApp.appId=<app-id> \
  --set ingress.enabled=true --set ingress.host=greybeard.example.com \
  --set ingress.className=nginx \
  --set serviceMonitor.enabled=true
```

Key `values.yaml` knobs: `existingSecret`, `githubApp.*`, `ingress.*`, `serviceMonitor.enabled`, `resources`, `nodeSelector` (defaults to `arm64` — the released image is Graviton-built; set to your arch). Non-secret env goes under `env:`.

Endpoints:

| Path | Purpose |
| --- | --- |
| `POST /webhook` | Webhook receiver — GitHub (HMAC) or GitLab (`X-Gitlab-Token`). |
| `GET /health` | Always HTTP 200; body reports `degraded` after 3 consecutive review failures. Use it as your load-balancer target check. |
| `GET /metrics` | Prometheus text: reviews by outcome, duration, tokens, cost, inflight, failure streak. |

`GREYBEARD_APP_INSTALLATION_ID` is **not** required for serve mode — each webhook payload carries its own installation ID, so one deployment serves every installation. (It's only needed when using App auth for a one-off local `review`.)

### 3. Expose the webhook endpoint

GitHub must be able to reach `/webhook` over HTTPS.

- **Production:** put the service behind a TLS-terminating reverse proxy / load balancer and set the App's webhook URL to the public host. For safety, restrict the public surface to `/webhook` and `/health`.
- **Local testing:** tunnel it — e.g. [`smee.io`](https://smee.io), `cloudflared tunnel`, or `ngrok http 8080` — and point the App's webhook URL at the tunnel.

### 4. Verify

Open (or reopen) a PR in an installed repo, or comment `@greybeard-bot review`. Within a few seconds the service logs a `review.done` line and the Greybeard comment appears on the PR. Check `GET /metrics` and the App's **Advanced → Recent Deliveries** page if nothing shows up.

**Spend guardrails** (serve mode): at most `GREYBEARD_MAX_CONCURRENT` (2) reviews run at once, a daily circuit breaker caps `GREYBEARD_DAILY_REVIEW_LIMIT` (50) reviews per UTC day, and bot-authored PRs are skipped unless `GREYBEARD_REVIEW_BOT_PRS=true`.

---

## GitHub App: comments post as the bot

By default local runs post as the authenticated **user**. To make comments post as the App bot in *either* mode, set all three App env vars — they take precedence over the user token:

```sh
export GREYBEARD_APP_ID=123456
export GREYBEARD_APP_PRIVATE_KEY=/secrets/greybeard.pem
export GREYBEARD_APP_INSTALLATION_ID=987654   # required for one-off `review`; optional for `serve`
```

Confirm with `greybeard auth-check` — it prints `auth mode: app` when the App identity is picked up.

---

## GitLab

Greybeard reviews GitLab merge requests too — set `GREYBEARD_FORGE=gitlab`. GitLab has no App / installation model: identity is a single **access token** (personal, project, or group), sent as `PRIVATE-TOKEN`. A dedicated **project or group access token** makes reviews post as a bot user. For self-managed GitLab set `GREYBEARD_FORGE_URL` to your instance root (e.g. `https://gitlab.example.com`) — the REST v4 API base is derived as `<root>/api/v4`; public gitlab.com needs no URL. Nested namespaces (`group/subgroup/project`) are supported. `GITLAB_TOKEN` is accepted as an alias for `GREYBEARD_TOKEN`.

The two modes and the whole pipeline are identical to GitHub — only the coordinates differ (a merge-request URL and iid, notes instead of PR comments).

### Local review (Mode 1)

The token needs **`api`** scope (read the MR, diffs, file contents, and CI pipeline; post/update the review note).

```sh
export GREYBEARD_FORGE=gitlab
export GREYBEARD_TOKEN=glpat-...                          # PAT / project / group token, api scope
# export GREYBEARD_FORGE_URL=https://gitlab.example.com   # self-managed only
export ANTHROPIC_API_KEY=sk-ant-...                       # or configure Bedrock

greybeard auth-check                                      # -> auth mode: access token / authenticated as: <user>
greybeard review https://gitlab.com/GROUP/PROJECT/-/merge_requests/N --dry-run
greybeard review https://gitlab.com/GROUP/PROJECT/-/merge_requests/N
```

`pack <mr-url>` (context pack only, no model calls) and `--force` work the same as GitHub.

### Automatic review (Mode 2)

`greybeard serve` handles GitLab webhooks natively — no App to create, just a project or group webhook.

1. **Create the token** the service posts with — a **project** or **group access token** with `api` scope (project/group **Settings → Access tokens**). Set it as `GREYBEARD_TOKEN`, and set `GREYBEARD_BOT_LOGIN` to that token user's username so the `@mention` command channel and self-note filtering match.

2. **Add a webhook** (project **Settings → Webhooks**):
   - **URL:** `https://<your-host>/webhook`
   - **Secret token:** a strong random string; pass the same value as `GREYBEARD_WEBHOOK_SECRET`. GitLab sends it verbatim in the `X-Gitlab-Token` header; greybeard compares it in constant time (no HMAC). Deliveries without a matching token are rejected.
   - **Triggers:** **Merge request events** and **Comments** (note events).

3. **Run it** like the GitHub service, minus the App vars:

   ```sh
   export GREYBEARD_FORGE=gitlab
   export GREYBEARD_TOKEN=glpat-...                          # project/group token
   export GREYBEARD_WEBHOOK_SECRET=<same as the webhook's secret token>
   export GREYBEARD_BOT_LOGIN=<the token user's username>
   export ANTHROPIC_API_KEY=sk-ant-...
   # export GREYBEARD_FORGE_URL=https://gitlab.example.com   # self-managed only
   greybeard serve --port 8080
   ```

**Which events trigger a review:**
- **Merge request** hooks: `open`, `reopen`, and `update` **only when it carries a code push** (an `oldrev`) — label / assignee / description edits do not trigger a review. Draft MRs are skipped.
- **Note** hooks: a comment starting with `@<bot> review` on an MR forces a re-review (same per-user cooldown as GitHub).

Deliveries are deduped on `X-Gitlab-Event-UUID`; the debounce, concurrency cap, daily circuit breaker, and spend guardrails are identical to GitHub. The Helm deployment is the same — only the env vars above change.

> **v1 limits:** the GitLab pack does not yet include git-blame or prior-review-comment context (those sections stay empty), and bot-author detection uses a username heuristic. See [docs/GITLAB.md](GITLAB.md).

---

## Configuration reference

| Var | Meaning | Default |
| --- | --- | --- |
| `GREYBEARD_FORGE` | code host: `github` or `gitlab` (see [GitLab](#gitlab)) | `github` |
| `GREYBEARD_TOKEN` | forge access token — GitHub (`GITHUB_TOKEN` / `GH_TOKEN` accepted) or GitLab (`GITLAB_TOKEN` accepted; sent as `PRIVATE-TOKEN`) | GitHub falls back to `gh auth token` |
| `GREYBEARD_FORGE_URL` | base URL for a self-hosted forge (GH Enterprise / self-managed GitLab) | the forge's public host |
| `GREYBEARD_PROVIDER` | `anthropic`, `bedrock`, or `openai` (OpenAI-compatible local server) | `anthropic` if `ANTHROPIC_API_KEY` set, else `bedrock` |
| `ANTHROPIC_API_KEY` | Anthropic API key (provider=anthropic) | — |
| `GREYBEARD_OPENAI_BASE_URL` | API root including `/v1`; **required** for openai | — |
| `GREYBEARD_OPENAI_API_KEY` | optional bearer token for openai | unset (no auth header) |
| `GREYBEARD_MODEL_MAX_CONCURRENT` | simultaneous model requests per review; positive integer | 1 for openai; unlimited for other providers |
| `GREYBEARD_LENS_MODEL` | strong model for the 6 lenses | `claude-opus-5` (anthropic); **required** for bedrock and openai |
| `GREYBEARD_VERIFY_MAX_TOKENS` | output budget for verification, including reasoning where the provider counts it | `4000` |
| `GREYBEARD_VERIFY_TIMEOUT_SECS` | timeout after the verification request starts, in seconds | `120` |
| `GREYBEARD_VERIFY_MODEL` | model for eligibility + per-finding verification (runs at low effort) | the lens model — a weak verifier suppresses real findings |
| `AWS_REGION` / `AWS_DEFAULT_REGION` | Bedrock region | `us-east-2` |
| `GITHUB_TOKEN` / `GH_TOKEN` | GitHub token — alias for `GREYBEARD_TOKEN` | falls back to `gh auth token` |
| `GREYBEARD_APP_ID` + `GREYBEARD_APP_PRIVATE_KEY` (pem path) + `GREYBEARD_APP_INSTALLATION_ID` | GitHub App identity — when set it takes precedence and comments post as the app bot; in serve mode the webhook payload's installation ID overrides the env pin | unset (user token) |
| `GREYBEARD_WEBHOOK_SECRET` | webhook verification secret — **required for `serve`**. GitHub: HMAC key (`X-Hub-Signature-256`). GitLab: the plain `X-Gitlab-Token` value (constant-time compared) | — |
| `GREYBEARD_BOT_LOGIN` | the bot's login, for @-mention matching and self-comment filtering | `greybeard-bot[bot]` |
| `GREYBEARD_MAX_CONCURRENT` / `GREYBEARD_DAILY_REVIEW_LIMIT` | serve-mode spend guardrails | 2 / 50 per UTC day |
| `GREYBEARD_REVIEW_BOT_PRS` | review PRs authored by bots (dependabot etc.) | `false` |
| `GREYBEARD_LOG_FILE` | pod-local JSONL run log | `greybeard-runs.jsonl` |
| `GREYBEARD_PRICE_IN` / `_OUT` / `_CACHE_READ` / `_CACHE_WRITE` | $ per million tokens for cost telemetry — all four required, tokens are reported regardless | unset (cost omitted) |

## Review verification

Discovery now runs in directory/size batches. The verifier receives focused
current source and can request additional repository files. Greybeard checks its
quotes and requires a concrete failure case before reporting a finding. Missing
evidence is shown as degraded verification. See [GATE.md](GATE.md).

The live regression evaluation is separate from normal `cargo test` because it
requires a configured local model and consumes inference time:

```sh
cargo test --test review_verification local_model_rejects_false_positives_and_detects_mutants -- --ignored --nocapture
```

Set `GREYBEARD_PROVIDER=openai`, `GREYBEARD_OPENAI_BASE_URL`, and
`GREYBEARD_LENS_MODEL` as for a local review. The client inherits server sampling
parameters. For qwen3-coder-next, record those effective settings when comparing
runs; Unsloth's GGUF guide currently recommends temperature 1.0, top-p 0.95,
top-k 40, min-p 0.01, and repetition penalty 1.0:
<https://unsloth.ai/docs/models/qwen3-coder-next>.
