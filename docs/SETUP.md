# Setup

Greybeard runs in two modes. Pick the one that matches how you want reviews to happen:

| Mode | What it does | When to use |
| --- | --- | --- |
| **1. Local review** | You run the CLI against a specific PR from your machine. Posts (or dry-runs) one review comment. | Ad-hoc reviews, trying it out, reviewing a PR on demand, driving a local fix loop. |
| **2. Automatic PR review** | A GitHub App + webhook service reviews every PR automatically on open and on each push. | Team-wide, hands-off review on every PR in a repo. |

Both modes share the same pipeline and the same [model provider](#model-provider) and [GitHub App](#github-app-comments-post-as-the-bot) configuration — only the trigger differs.

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

3. **GitHub access** — a user token for local mode, or a GitHub App for automatic mode.

---

## Model provider

Greybeard talks to Claude either directly (Anthropic API) or through Amazon Bedrock. The provider is auto-detected: if `ANTHROPIC_API_KEY` is set it uses Anthropic, otherwise Bedrock. Force it with `GREYBEARD_PROVIDER=anthropic|bedrock`.

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

---

## Mode 1 — Local review

Review a single PR from your machine.

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
| `POST /webhook` | GitHub webhook receiver (HMAC-verified). |
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

## Configuration reference

| Var | Meaning | Default |
| --- | --- | --- |
| `GREYBEARD_FORGE` | code host: `github` or `gitlab` | `github` (GitLab is [in design](GITLAB.md), not yet implemented) |
| `GREYBEARD_TOKEN` | forge access token (`GITHUB_TOKEN` / `GH_TOKEN` still accepted) | falls back to `gh auth token` |
| `GREYBEARD_FORGE_URL` | base URL for a self-hosted forge (GH Enterprise / self-managed GitLab) | the forge's public host |
| `GREYBEARD_PROVIDER` | `anthropic` or `bedrock` | `anthropic` if `ANTHROPIC_API_KEY` set, else `bedrock` |
| `ANTHROPIC_API_KEY` | Anthropic API key (provider=anthropic) | — |
| `GREYBEARD_LENS_MODEL` | strong model for the 6 lenses | `claude-opus-5` (anthropic); **required** for bedrock, e.g. `us.anthropic.claude-opus-5` |
| `GREYBEARD_VERIFY_MODEL` | model for eligibility + per-finding verification (runs at low effort) | the lens model — a weak verifier suppresses real findings |
| `AWS_REGION` / `AWS_DEFAULT_REGION` | Bedrock region | `us-east-2` |
| `GITHUB_TOKEN` / `GH_TOKEN` | GitHub token — alias for `GREYBEARD_TOKEN` | falls back to `gh auth token` |
| `GREYBEARD_APP_ID` + `GREYBEARD_APP_PRIVATE_KEY` (pem path) + `GREYBEARD_APP_INSTALLATION_ID` | GitHub App identity — when set it takes precedence and comments post as the app bot; in serve mode the webhook payload's installation ID overrides the env pin | unset (user token) |
| `GREYBEARD_WEBHOOK_SECRET` | HMAC secret for webhook verification — **required for `serve`** | — |
| `GREYBEARD_BOT_LOGIN` | the bot's login, for @-mention matching and self-comment filtering | `greybeard-bot[bot]` |
| `GREYBEARD_MAX_CONCURRENT` / `GREYBEARD_DAILY_REVIEW_LIMIT` | serve-mode spend guardrails | 2 / 50 per UTC day |
| `GREYBEARD_REVIEW_BOT_PRS` | review PRs authored by bots (dependabot etc.) | `false` |
| `GREYBEARD_LOG_FILE` | pod-local JSONL run log | `greybeard-runs.jsonl` |
| `GREYBEARD_PRICE_IN` / `_OUT` / `_CACHE_READ` / `_CACHE_WRITE` | $ per million tokens for cost telemetry — all four required, tokens are reported regardless | unset (cost omitted) |
