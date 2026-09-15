# GitLab support — design

Status: **design / not implemented.** GitHub is the only working backend today.
This document specifies what a GitLab backend needs and how the code gets there.
The groundwork already in the tree (a forge-neutral config surface and a single
`connect` dispatch point in `src/forge.rs`) exists so this can land without
churning the GitHub path.

## Goal

Run the exact same review pipeline (6 lenses → adversarial verify → banded gate
→ one upserted comment) against GitLab merge requests, selected by
`GREYBEARD_FORGE=gitlab`, in both CLI and service mode. GitHub stays the default
and is unaffected.

## Where GitHub coupling lives today

| Concern | File | GitHub specifics |
| --- | --- | --- |
| Client / auth / URL | `src/github/mod.rs` | `api.github.com`, `PrRef` parses `github.com/.../pull/N`, App JWT → installation token |
| Context pack | `src/github/pack.rs` | one GraphQL query (PR meta + files + comments + CI rollup), blame via GraphQL, diff + file contents via REST |
| Comment upsert | `src/github/comment.rs` | `addComment` / `updateIssueComment` GraphQL mutations; hidden marker carries state |
| Webhook service | `src/server.rs` | `x-hub-signature-256` HMAC, `X-GitHub-Delivery` dedupe, `pull_request` / `issue_comment` payload shapes |
| Config | `src/config.rs` | `GITHUB_TOKEN`, `GREYBEARD_APP_*` |

## The abstraction: a `Forge` trait

The pipeline should depend on a trait, not on `Github`. Proposed shape (native
`async fn` in traits, already available on our toolchain):

```rust
pub struct ChangeRef {      // PR or MR, forge-neutral
    pub project: String,    // "owner/repo" or "group/subgroup/project"
    pub number: u64,        // PR number / MR iid
}

pub struct ForgeEvent {
    pub change: ChangeRef,
    pub action: &'static str, // opened | synchronize | reopened | ready | command
    // Backend-private auth context (e.g. GitHub installation id). Do NOT bake a
    // forge-specific `u64` into this shared type — keep it opaque (a per-forge
    // enum, or let each `parse_event`/connect derive it internally) so a third
    // forge doesn't force a shape change here.
    pub context: ForgeAuthContext,
}

#[async_trait-or-native]
pub trait Forge {
    fn auth_mode(&self) -> &str;
    fn parse_ref(url: &str) -> Result<ChangeRef> where Self: Sized;

    /// Everything the lenses read, rendered byte-deterministically.
    async fn build_pack(&self, c: &ChangeRef, cfg: &Config) -> Result<Pack>;

    /// Deterministic eligibility: state, draft, head sha, already-reviewed marker.
    async fn eligibility(&self, c: &ChangeRef) -> Result<Eligibility>;

    /// Create or update the single greybeard comment (marker = state). Needs
    /// the create-vs-update state today's `comment::upsert` takes — the create
    /// target (GitHub PR node id / GitLab MR ref) and the existing comment id
    /// (GitHub node id / GitLab numeric note id) if one was found. Carry that on
    /// `Pack`/`Eligibility` as a forge-owned `comment_anchor`, not as bare args.
    async fn upsert_comment(&self, c: &ChangeRef, body: &str, anchor: &CommentAnchor) -> Result<String>;

    /// Webhook plumbing (service mode).
    fn verify_webhook(secret: &str, headers: &Headers, body: &[u8]) -> bool where Self: Sized;
    fn parse_event(headers: &Headers, body: &Value) -> Option<ForgeEvent> where Self: Sized;
}
```

`connect*` in `src/forge.rs` becomes `Result<Box<dyn Forge>>`. `pack.rs` splits
into a **forge-agnostic renderer** (structured diff/blame/comment types → the
deterministic pack string) and a **forge-specific fetcher** (the API calls that
fill those types). Most of the rendering is genuinely shared (e.g. `render_blame`
already works over a forge-neutral tuple), but three things in today's `pack.rs`
are GitHub-shaped and **must change**, not "stay shared":

- **`ContextPack.pr_node_id`** — a GraphQL node id needed only for GitHub's
  comment mutations. GitLab uses a numeric note id from a different call. Move it
  off `ContextPack` into a forge-owned `CommentAnchor` (see `upsert_comment`).
- **`render_checks`** — reads raw GraphQL JSON shapes (`__typename`, `CheckRun`,
  `StatusContext`) directly; it's a GraphQL parser, not a renderer over a type.
  GitLab pipelines have a different shape, so it must be rewritten against a
  normalized `Vec<CheckStatus>` both fetchers produce.
- **`ContextPack.pr: PrRef`** — the GitHub ref type, used directly in rendering;
  becomes `ChangeRef`.

`comment.rs`'s marker helpers (`render_marker`/`parse_marker`/`render_marker_v2`)
are already forge-agnostic; only the two mutations are GitHub-specific.

## GitHub → GitLab API mapping

Recommendation: use **GitLab REST v4** for the GitLab backend, for consistency
with the REST diff/contents path we already use on GitHub and because it maps
MRs + notes + blame uniformly. (GitLab GraphQL now *does* expose blame, so this
is a consistency choice, not a GraphQL-capability gap.) Base URL
`https://gitlab.com/api/v4`, or
`<GREYBEARD_FORGE_URL>/api/v4` for self-managed. Project id is the URL-encoded
path (`group%2Fsubgroup%2Fproject`) or numeric id.

| Need | GitHub (today) | GitLab REST v4 |
| --- | --- | --- |
| MR/PR metadata | GraphQL `pullRequest` | `GET /projects/:id/merge_requests/:iid` |
| Changed files + diff | REST `/pulls/:n` (diff media type) | `GET /projects/:id/merge_requests/:iid/diffs` — **paginated** (~20/page), the fetcher must loop pages. (`/changes` is deprecated since 15.7; don't use it) |
| File contents at sha | REST `/contents/:path?ref=` | `GET /projects/:id/repository/files/:path/raw?ref=:sha` |
| Blame | GraphQL `blame` | `GET /projects/:id/repository/files/:path/blame?ref=:sha` |
| CI status rollup | GraphQL `statusCheckRollup` | `GET /projects/:id/merge_requests/:iid/pipelines` (head pipeline + jobs) |
| List comments | GraphQL comments | `GET /projects/:id/merge_requests/:iid/notes` |
| Add comment | GraphQL `addComment` | `POST /projects/:id/merge_requests/:iid/notes` |
| Update comment | GraphQL `updateIssueComment` | `PUT /projects/:id/merge_requests/:iid/notes/:note_id` |
| Prior-PR feedback | GraphQL (commit → associated PRs → threads) | multi-step: `GET .../repository/commits/:sha/merge_requests` → that MR's `/notes` |
| Permalink to a line | `.../blob/:sha/:path#L10-20` | `.../-/blob/:sha/:path#L10-20` (note the **`/-/`** — different from GitHub; anchor form `#L10` / `#L10-20` is the same) |

The single-round-trip GraphQL advantage on GitHub becomes several parallel REST
calls on GitLab — acceptable; they still `join!` under the same latency budget.

## Auth

GitLab has **no GitHub-App / JWT / installation model.** Identity is a token:

- **Personal / project / group access token**, sent as `PRIVATE-TOKEN: <token>`
  (or `Authorization: Bearer <token>`).
- The bot identity is simply whichever user/service-account owns the token — a
  dedicated **project or group access token** posts notes as a bot user.

So `GREYBEARD_APP_ID` / `GREYBEARD_APP_PRIVATE_KEY` / `GREYBEARD_APP_INSTALLATION_ID`
have **no GitLab analog** and are ignored when `GREYBEARD_FORGE=gitlab`. The
token comes from `GREYBEARD_TOKEN` (already the forge-neutral name).

**Bot-author detection differs.** The `review_bot_prs` guard currently keys off
GitHub's GraphQL `__typename == "Bot"` actor type (`pack.rs`). GitLab has no
first-class bot-actor type in the same sense, so the GitLab backend needs a
different heuristic (service-account username convention, or the token's own
identity) — decide this in phase 3.

## Webhooks (service mode)

| Aspect | GitHub | GitLab |
| --- | --- | --- |
| Secret check | HMAC-SHA256 in `X-Hub-Signature-256` | **plain equality** of `X-Gitlab-Token` header vs the configured secret — no HMAC |
| Event type header | `X-GitHub-Event` | `X-Gitlab-Event` (`Merge Request Hook`, `Note Hook`) |
| Dedupe id | `X-GitHub-Delivery` | `X-Gitlab-Event-UUID` |
| MR events | `pull_request`: opened/synchronize/ready_for_review/reopened | `object_kind: merge_request`, `object_attributes.action`: open/update/reopen/merge; a push is `update` with a `oldrev` |
| Comment command | `issue_comment` created, body `@bot review` | `object_kind: note`, `object_attributes.noteable_type: MergeRequest`, body `@bot review` |
| Draft | `draft: true` | `object_attributes.draft: true` (read `draft`, not the deprecated `work_in_progress`) / title `Draft:` |

Implication: `verify_webhook` must be per-forge (HMAC vs plain token compare —
keep the constant-time compare for GitLab too). Payload extraction
(`parse_event`) is per-forge. The debounce / dedupe / circuit-breaker / inflight
machinery in `server.rs` is already forge-agnostic and stays as-is.

## URL parsing

GitLab MR URLs carry **nested namespaces** and a `/-/` separator:

```
https://gitlab.com/group/subgroup/project/-/merge_requests/123
                   └────── project path ──────┘        └ iid ┘
```

`parse_ref` for GitLab: strip host, take everything before `/-/merge_requests/`
as the project path (may contain multiple `/`), and the trailing integer as the
iid. Contrast GitHub's fixed `owner/repo/pull/N`. `ChangeRef.project` holds the
full path either way.

## Config

| Var | GitHub | GitLab |
| --- | --- | --- |
| `GREYBEARD_FORGE` | `github` (default) | `gitlab` |
| `GREYBEARD_TOKEN` | user/App token | PAT / project / group access token |
| `GREYBEARD_FORGE_URL` | GH Enterprise base | self-managed GitLab base (else `https://gitlab.com`) |
| `GREYBEARD_WEBHOOK_SECRET` | HMAC key | `X-Gitlab-Token` value (plain) |
| `GREYBEARD_BOT_LOGIN` | `app-slug[bot]` | bot account username |
| `GREYBEARD_APP_*` | GitHub App identity | **ignored** |

`GITHUB_TOKEN` / `GH_TOKEN` remain accepted aliases for `GREYBEARD_TOKEN`.

## Phased plan

1. **Groundwork (done).** Forge-neutral config (`GREYBEARD_FORGE`,
   `GREYBEARD_TOKEN`, `GREYBEARD_FORGE_URL` — the last now actually threaded into
   the GitHub client's REST/GraphQL base URLs, so GitHub Enterprise works), a
   single `connect` dispatch in `src/forge.rs` with an early `ensure_supported`
   gate, and this doc. GitHub behavior on github.com unchanged.
2. **Extract the `Forge` trait.** Define it; make `Github` implement it; split
   `pack.rs` into shared renderer + GitHub fetcher; make the pipeline generic
   over `&dyn Forge`. Pure refactor, no behavior change — the existing suite
   must stay green.
3. **GitLab backend.** `src/gitlab/` implementing the trait: REST v4 client,
   MR pack fetch, notes upsert, `PRIVATE-TOKEN` auth, `X-Gitlab-Token` +
   `Merge Request Hook` / `Note Hook` webhook, MR-URL parsing.
4. **Docs + verify.** GitLab setup section in `docs/SETUP.md`; verify against a
   real GitLab test project (CLI dry-run, then a live MR webhook).

## Open questions

- **CI rollup fidelity:** GitLab pipelines/jobs vs GitHub check runs — decide
  how much the ci-config lens needs beyond pass/fail per job.
- **Rate limits:** GitLab.com is **2,000 authenticated requests/minute per user**
  (a per-*minute* bucket, vs GitHub's 5,000/*hour*), so a burst fan-out trips it
  faster — the `/diffs` + blame + prior-feedback calls per review want a small
  per-review concurrency cap.
- **Group-level webhooks/tokens:** support installing once at a group (like a
  GitHub org install) vs per-project.
- **Bot-author heuristic** for `review_bot_prs` (see Auth) — pick the convention.

Resolved during design review: GitLab line permalinks are `.../-/blob/:sha/:path#L10`
(and `#L10-20`) — note the `/-/` prefix, folded into the mapping table above.
