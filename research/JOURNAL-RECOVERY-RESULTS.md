# A1a.2 typed journal and strict read recovery

2026-09-28. Scope: runtime transition validation, atomic host/runtime codec, bounded read-only recovery, and native writer refusal. This is not production tool enablement.

## Verified evidence

`CARGO_BUILD_JOBS=4 just test -p codex-panng-runtime -p codex-history -p codex-rollout -p codex-thread-store` completed with **475 passed, zero skipped**, nextest execution 5.768 seconds (compilation excluded). Exact log: `/tmp/panng-r1-regression-fixed.log`. This run includes the final R1 settings-owner, initial-commit-ID and fork/resume anchor fixes. It predates R2 writer/guard changes.

Behavior covered includes immutable operation identity and revision transitions, cross-epoch logical-call deduplication, conservative fork/resume state, exact canonical codec round trips, rejection of non-restorable host fields, complete joint checkpoints, bounded suffix validation, every truncation offset of the last delta, corruption and gap refusal, settings ownership, initial commit ID reuse, and native marked-file refusal before compression materialization/newline repair. Native large headers retain their prior behavior.

`just bazel-lock-update` completed successfully after dependency changes. Cargo dependencies use existing workspace versions.

## Limits

The read cut proves only the location of a validated complete record; it is not a durability ACK. Latest-checkpoint recovery deliberately does not audit all historical prefix bytes. No execution may begin from recovered Prepared records. No real CLI feature, host commit coordinator, complete UI/history recovery, or job/resource lifecycle is enabled by these primitives.

R2 same-writer append/reconcile and cold fencing is the next implementation. Its reviewed changes are recorded in STRICT-REOPEN-PLAN.md and RECONCILE-PLAN-REVIEW.md. Final scoped lint/format and R2 test results must be recorded separately, without treating the 475-test run as evidence for later edits.
