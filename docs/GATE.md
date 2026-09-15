# The gate — what posts and why

Every candidate finding ends its life at the gate. A lens proposed it, the
adversarial verifier returned `{real, confidence 0-100, reason}`, and the gate
decides whether it reaches the PR comment. The verifier's rubric (inherited
from a battle-tested review pipeline, kept verbatim in `src/prompts.rs`)
deliberately blends two questions into the one confidence number: *is this
real?* and *does it matter?* — 50 means "verified real but might be a nitpick",
75 means "real and important". Any gate on that number is therefore a **posting
policy**, not a correctness check.

A bake-off against a suite of real PRs measured three policies:

## A — flat ≥80 ("only speak when it matters")

Post only `real && confidence >= 80`. Maximum signal; the brand stays "the
reviewer that never wastes your time." The measured cost: ~90% of verified-TRUE
findings are silently discarded (committed junk files at 60, an nginx gzip gap
at 78, dead-code-with-false-comments at 60-68 — all things human reviewers do
flag), and "No issues found" overclaims when the pipeline verified several true
issues and chose silence. If this policy is ever restored, change the
empty-result copy to "No blocking issues found."

## B — confidence bands (CURRENT)

- `real && confidence >= confidence_threshold` (80) → numbered findings, as ever.
- `real && confidence >= minor_threshold` (60) → a collapsed **Minor notes**
  `<details>` section: one line + permalink each, top-5 by confidence shown.
- below 60 → dropped (stderr log only).

Banding is by **verifier confidence**, not by the lens's blocker/gap/nit label —
in the bake-off the junk-files find was labeled *gap* but scored 60; a
severity-label rule would still suppress it. Dedupe runs within each band, then
minor drops anything colliding with a confirmed finding (higher band wins).
Verdict line: minor-only reviews close with "Pass — but mind the cracks in the
bridge", not "You shall pass."

Measured on the five-PR suite: posts ~8 additional real findings, still 0
false positives. The headline section is unchanged; the minor section is
folded shut by default.

## C — flat ≥60

Everything B posts, but promoted into the main numbered list. Rejected: it
dilutes the headline with minor items, lets nits drive the verdict line, and
walks straight toward the noise reputation this tool exists to avoid.

## Re-tuning

The thresholds live in `src/config.rs` (`confidence_threshold`,
`minor_threshold`). To re-measure a change: run the bake-off PR suite in
`--dry-run --force` and fan the
outputs through judge agents comparing against the recorded baselines. One
command each way — don't tune by anecdote.
