# Finding verification

Discovery produces candidates. A separate verifier receives the current source
for one candidate, applicable repository guidance, and a list of changed paths.
It can request additional repository files, including unchanged dependencies.
It has no execution tool and must not claim to have run tests.

Every verdict has one of three outcomes:

- `confirmed`: current-source citations support a reachable failure or explicit
  rule violation, with confidence of at least 80.
- `refuted`: current-source citations contradict the claim, with confidence of
  at least 80.
- `unverified`: evidence is missing or insufficient. This degrades the report
  and suppresses the final pass/fail verdict.

Confidence measures evidentiary certainty only. Severity describes impact and is
reassessed by the verifier. Confirmed `blocker` and `gap` findings enter the main
report; confirmed `nit` findings enter Minor notes. Uncertainty never becomes a
minor finding.

Before accepting a factual verdict, Greybeard checks that each quote matches
whole current-source lines at the supplied path and location. Leading and
trailing whitespace on each quoted line may differ; the report uses the exact
source text after validation. For confirmation,
the verifier may supply or correct the discovery location, and its first citation must anchor the defect in the candidate file's new-side diff
ranges. A surviving line next to a deletion may anchor a missing-case finding.
A removed line cannot be evidence of current behavior. Confirmation also requires
a trigger, expected and actual behavior, and an explanation of why existing
safeguards do not prevent the failure. These fields appear in the report.

A failed citation check is returned to the verifier for correction. Low confidence
produces an unverified result immediately; it is never retried to raise the score. Verification
has at most three rounds, eight source files, and 80 KB of source context. Missing
files, exhausted budgets, model failures, and unsupported final verdicts remain
unverified. Matching quotations establish source provenance; they do not prove
that the model's causal argument is correct.

Discovery groups files by directory and size, with an 80 KB context budget per
call. Applicable lens instructions share one discovery call per batch.
CI/config, guidance, history, and prior-feedback checks run only when their
inputs are present. Each batch verifies at most eight candidates; any additional
candidates count as unverified and make the report incomplete. Large individual files or diffs may still be omitted from
discovery, with omission notes in the context. Source needed to verify a candidate
is fetched independently of those discovery limits.

Use the opt-in evaluation documented in README.md to compare workflow or model
changes. Its negative cases are four false positives from a Reliquary review;
its positive cases remove the corresponding protections. Report both missed
bugs and false alarms, and retain unknown results as unknown. A confidence
score is a model assessment, not a calibrated probability.
