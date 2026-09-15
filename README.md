# greybeard

<img src="assets/avatar.png" alt="Greybeard" width="120" align="right" />

[![CI](https://github.com/spcmky/greybeard/actions/workflows/ci.yml/badge.svg)](https://github.com/spcmky/greybeard/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/spcmky/greybeard)](https://github.com/spcmky/greybeard/releases/latest)
[![License: MIT](https://img.shields.io/github/license/spcmky/greybeard)](LICENSE)
[![Built with Rust](https://img.shields.io/badge/built%20with-Rust-orange?logo=rust)](https://www.rust-lang.org)

The mythical senior engineer who's seen every failure mode.

Greybeard is a PR-review service: six specialist review lenses run in parallel over a
pre-fetched context pack, every candidate finding is adversarially verified by an
independent low-effort pass on the same strong model, and the survivors land as
**one** PR comment that is updated in place on re-review. Target wall time: under 5 minutes (typically 2–3.5).

## How it works

```
Stage 0   context pack (one GraphQL query + diff + file contents, ~1-2s)
          deterministic eligibility: closed / draft / already-reviewed-SHA → skip
Stage 0b  cache warm (max_tokens:0 prefill) ∥ trivial-PR check (fast model)
Stage 1   6 lenses in parallel (strong model, zero tools, pack is a cached prefix):
            ci-config · claude-md · bugs · history(blame) · prior-feedback · code-comments
Stage 2   per-finding adversarial verify (lens model at low effort, REFUTE-framed, 0-100 rubric)
          — pipelined per lens, no barrier; banded gate (docs/GATE.md):
            real && ≥80 → findings, real && ≥60 → collapsed Minor notes
Stage 3   dedupe → render → upsert the single Greybeard comment (marker = state)
```

The context pack is rendered **byte-deterministically** — it is the shared
prompt-cache prefix for every model call. Watch `cache_rd` in the telemetry table:
zero on repeat calls means a nondeterminism bug crept into the pack renderer.

## Setup

**[docs/SETUP.md](docs/SETUP.md)** is the full guide. Two ways to run it:

1. **Local review** — run the CLI (or `docker compose`) against a PR from your machine.
2. **Automatic review** — a GitHub App + webhook service reviews every PR on open/update. Deploy it with the bundled [Helm chart](helm/greybeard) or [`docker-compose.yml`](docker-compose.yml).

Runs against **GitHub** today; the forge is selected by `GREYBEARD_FORGE` and **GitLab** support is [in design](docs/GITLAB.md).

## Usage

```sh
greybeard review https://github.com/OWNER/REPO/pull/N [--dry-run] [--force]
greybeard pack   https://github.com/OWNER/REPO/pull/N     # print the pack, no model calls
```

`--dry-run` prints the comment instead of posting. `--force` reviews even if the PR
is closed / draft / already reviewed at this SHA / judged trivial.

### Examples

```sh
# Preview a review without posting — a safe first run, prints the comment to stdout
ANTHROPIC_API_KEY=sk-ant-... \
  greybeard review https://github.com/acme/api/pull/482 --dry-run

# Post it for real (re-running updates the same comment in place, never a second one)
greybeard review https://github.com/acme/api/pull/482

# Re-review a PR that's closed / already reviewed at this SHA / judged trivial
greybeard review https://github.com/acme/api/pull/482 --force

# See exactly what the model sees — no API calls, no cost
greybeard pack https://github.com/acme/api/pull/482 | less

# Check how you're authenticated (user token vs GitHub App bot)
greybeard auth-check

# Use Amazon Bedrock instead of the Anthropic API
GREYBEARD_PROVIDER=bedrock \
GREYBEARD_LENS_MODEL=us.anthropic.claude-opus-5 \
  greybeard review https://github.com/acme/api/pull/482 --dry-run

# No local toolchain? Run it straight from the Docker image
docker run --rm -e ANTHROPIC_API_KEY -e GITHUB_TOKEN=$(gh auth token) \
  greybeard:local review https://github.com/acme/api/pull/482 --dry-run
```

In [service mode](#service-mode), comment `@greybeard-bot review` on any PR to force a re-review.

### Service mode

```sh
greybeard serve --port 8080     # GitHub App webhook -> automatic reviews
```

Subscribed events: `pull_request` (opened / synchronize / ready_for_review /
reopened) and `issue_comment` (`@greybeard-bot review` forces a re-review).
Deliveries are HMAC-verified (`GREYBEARD_WEBHOOK_SECRET`), deduped by delivery
GUID, and debounced per PR (a new push aborts the in-flight review and
restarts on the newest head; the aborted run's spend is still accounted).
The installation ID is taken from each webhook payload, so one deployment
serves every installation of the App.

#### Observability

Every finished run emits one structured `review.done` JSON line to stdout
(outcome, duration, finding counts, tokens, optional cost) — in-cluster,
Promtail ships stdout to Loki with 30-day retention, which makes that line the
durable run log (`GREYBEARD_LOG_FILE` JSONL is a pod-local convenience copy).
`GET /metrics` serves Prometheus text: reviews by outcome
(posted / skipped / error / daily-limit / superseded), duration, tokens by
kind, cost, inflight gauge, consecutive failures. `GET /health` reports
`degraded` after 3 consecutive review failures — always HTTP 200, because
dashboards act on it, not the load balancer; only a completed run resets the
streak (benign skips prove nothing about the model path).

Guardrails: bot-authored PRs are skipped before any model call
(`GREYBEARD_REVIEW_BOT_PRS=true` to opt in); at most `GREYBEARD_MAX_CONCURRENT`
(2) reviews run at once; a daily circuit breaker caps runs at
`GREYBEARD_DAILY_REVIEW_LIMIT` (50) per UTC day; the @-mention channel has a
10-minute per-user cooldown; throttled/transient model errors retry with
backoff. Pack content is treated as untrusted data — the prompts forbid it
from lowering scrutiny (see the trust-boundary paragraph in `src/prompts.rs`).

Deploy is GitOps: CI (`.github/workflows/ci.yml`) tests every PR and pushes a
sha7 image to ECR from main; deploying is a tag bump in the
`helm/tools/greybeard` chart in your infra repo, which ArgoCD rolls out. The
chart also ships the ServiceMonitor for /metrics and path-limits the
internet-facing ingress to `/webhook` + `/health`.

## Configuration (env)

| Var | Meaning | Default |
| --- | --- | --- |
| `GREYBEARD_FORGE` | code host: `github` or `gitlab` | `github` (GitLab is [in design](docs/GITLAB.md), not yet implemented) |
| `GREYBEARD_TOKEN` | forge access token (`GITHUB_TOKEN` / `GH_TOKEN` still accepted) | falls back to `gh auth token` |
| `GREYBEARD_FORGE_URL` | base URL for a self-hosted forge (GH Enterprise / self-managed GitLab) | the forge's public host |
| `GREYBEARD_PROVIDER` | `anthropic` or `bedrock` | `anthropic` if `ANTHROPIC_API_KEY` set, else `bedrock` |
| `ANTHROPIC_API_KEY` | Anthropic API key (provider=anthropic) | — |
| `GREYBEARD_LENS_MODEL` | strong model for the 6 lenses | `claude-opus-5` (anthropic); **required** for bedrock, e.g. `us.anthropic.claude-opus-5` |
| `GREYBEARD_VERIFY_MODEL` | model for eligibility + per-finding verification (runs at low effort) | the lens model — a weak verifier suppresses real findings |
| `AWS_REGION` / `AWS_DEFAULT_REGION` | Bedrock region | `us-east-2` |
| `GITHUB_TOKEN` / `GH_TOKEN` | GitHub token — alias for `GREYBEARD_TOKEN` | falls back to `gh auth token` |
| `GREYBEARD_APP_ID` + `GREYBEARD_APP_PRIVATE_KEY` (pem path) + `GREYBEARD_APP_INSTALLATION_ID` | GitHub App identity — when set it takes precedence and comments post as the app bot; in serve mode the webhook payload's installation ID overrides the env pin | unset (user token) |
| `GREYBEARD_MAX_CONCURRENT` / `GREYBEARD_DAILY_REVIEW_LIMIT` | serve-mode spend guardrails | 2 / 50 per UTC day |
| `GREYBEARD_LOG_FILE` | pod-local JSONL run log | `greybeard-runs.jsonl` |
| `GREYBEARD_PRICE_IN` / `_OUT` / `_CACHE_READ` / `_CACHE_WRITE` | $ per million tokens for cost telemetry — all four required, tokens are reported regardless | unset (cost omitted) |

Provider notes:
- **anthropic**: structured output is schema-enforced server-side
  (`output_config.format`); lenses run at `effort: high`.
- **bedrock**: legacy InvokeModel wire shape (SigV4, service `bedrock`);
  no server-side schema enforcement — JSON shape is enforced by prompt +
  parse-with-retry, and missing optional fields are defaulted rather than fatal.

## GitHub transport note

Greybeard is **GraphQL-first**: one query fetches PR metadata + changed files +
comments + CI rollup; blame and prior-PR feedback are GraphQL; comments are
posted via `addComment`/`updateIssueComment` mutations. Only the unified diff
and file contents use REST. Two reasons: it collapses ~6 REST round-trips into
one query, and it proved the more resilient surface during the 2026-08-17
GitHub partial outage, when several REST sub-resource endpoints returned 404s
for hours (initially misdiagnosed here as proxy filtering — they recovered
with the incident).

## Build & test (no local toolchain needed)

```sh
docker run --rm -v "$PWD":/work -v greybeard-cargo:/usr/local/cargo/registry \
  -w /work rust:1-bookworm cargo test

# run a review from the container (Bedrock via mounted AWS creds):
docker run --rm -v "$PWD":/work -v greybeard-cargo:/usr/local/cargo/registry \
  -v ~/.aws:/root/.aws:ro -w /work \
  -e GREYBEARD_PROVIDER=bedrock \
  -e GREYBEARD_LENS_MODEL=us.anthropic.claude-opus-5 \
  -e GITHUB_TOKEN=$(gh auth token) \
  rust:1-bookworm ./target/debug/greybeard review <pr-url> --dry-run --force
```

## Voice

How comments are written — persona, comment anatomy, severity vocabulary, hard
rules — is specified in [docs/VOICE.md](docs/VOICE.md). `src/prompts.rs` and
`src/pipeline/compose.rs` implement that contract; change them together.

The avatar lives at `assets/avatar.png` —
use it for the GitHub App identity in Phase 3.

## Live

Production instance: `https://greybeard.apps.example.com` (health at `/health`) —
ArgoCD app `greybeard`, chart `helm/tools/greybeard` in your infra repo. The
GitHub App webhook reviews PRs automatically in the repos it is installed on;
comment `@greybeard-bot review` on a PR to force a re-review. The comment's
hidden marker carries machine-readable findings (v2) — the
`greybeard-loop` skill (`.claude/skills/greybeard-loop`) drives a local
Claude Code fix→push→re-review loop off it until the verdict is clean.

## Roadmap

Shipped: CLI + service (1.0.0), marker v2 machine contract + `greybeard-loop`
fix-loop skill (1.1.0), ops & reliability — run events, /metrics, degraded
health, panic-safe accounting (1.2.0). Currently in an observation period on
real traffic across the installed repos.

Next, informed by that data:
- **Trust & learning** — feedback harvesting (reactions + a `wrong:` reply
  channel), optional check-run mode so repos can require Greybeard in branch
  protection (team decision first), per-repo `.greybeard.md` review standards.
- **Review quality** — a bounded exec-check tool for blocker-class
  verification (the known recall ceiling), dedupe window + threshold re-tune
  against feedback data.
- **Performance polish** (if usage demands) — per-lens retry-at-lower-effort,
  SSE streaming for long lens calls, Bedrock Mantle endpoint migration
  (would unlock `effort` on Bedrock).
