# R2 identified writer validation

2026-09-28. Implements live identified append/reconciliation and cold reopen on the existing recorder queue/file handle. This stage does not create production sessions or execute real tools.

## Implemented contract

- Immutable Native/Strict recorder mode rejects the wrong public append APIs before enqueue, with a second writer-side check.
- The writer validates each transition against its committed runtime and retains the candidate plus original encoded line before I/O. Durable acknowledgement promotes runtime and physical/sequence cuts together.
- One uncertain attempt fences new commits. Exact absent/prefix tails can return Absent after synchronization; exact full tails are re-synchronized without re-appending. Conflicting/extra bytes poison the writer and remain untouched.
- Cold reopen uses the strict reader and requires a fresh ResumeFence before ordinary admission. A recovered read cut never populates the live durable retry cache.
- Guard thread/home and branch, canonical session path, and actual file ownership are checked before repair. The same file descriptor holds an advisory lock throughout recovery and writer lifetime, including across hardlink aliases.
- Commit-ID checks cover the initial ID and inspected checkpoint suffix; this is not a full historical deduplication database.

## Evidence so far

The final runtime/codec corrections passed **60/60** tests with `just test -p codex-panng-runtime -p codex-history` (`/tmp/panng-r2-codec-regression.log`). The initial R2 writer/guard implementation passed **425/425** with `just test -p codex-rollout -p codex-thread-store` (`/tmp/panng-r2-writer-tests-fixed.log`). The first compile attempt exposed fixture use of ThreadId::FromStr; fixtures now use ThreadId::from_string.

Independent implementation reviews: RUNTIME-CODEC-REVIEW.md and STRICT-WRITER-REVIEW.md. The latter found a real cross-home ownership hole which was fixed with canonical path restriction and a retained actual-file lock before these tests.

Additional fault coverage is complete: a real dropped ACK receiver followed by public retry does not append; same-length tail corruption and an extra complete record poison the writer without rewriting evidence; four truncate/sync fault boundaries retain pending state and cold fencing until a successful retry. Final rollout/thread-store run passed **428/428**, zero skipped, in 5.607 seconds excluding compilation (`/tmp/panng-r2-final-tests-fixed.log`). A nested test-module macro ambiguity was corrected with an explicit pretty_assertions import before this passing run.

Together with runtime/history, **488 relevant tests passed**. Scoped runtime/history Clippy completed successfully. The first rollout/thread-store Clippy pass reported two production unwrap/expect sites. Both now return explicit uncertain errors and poison the writer rather than panic. A new regression cycle after that semantic edit again passed **428/428**, zero skipped, 5.556 seconds excluding compilation (`/tmp/panng-r2-no-panic-tests.log`). Final scoped `just fix -p codex-rollout -p codex-thread-store` completed with no warnings, and repository `just fmt` completed successfully. No tests were repeated merely because of final fix/format. `git diff --check` passed.

## Remaining vertical integration

Only strict resume is available; fixtures currently supply the initial marked checkpoint. Production creation must durably publish header plus initial joint checkpoint. Follow HOST-INTEGRATION-MAP.md to connect store, early Session recovery, host publication and UI pagination before real tool enablement. Current safety caps require lifecycle/backpressure handling for long tasks. Formal usability and performance/cost comparisons remain unproven.

## Patch staging

Source is saved in eight local commits from `08e2bc51f6` through `66ae53fc7d`: bounded types, runtime transitions, host codec, framing/native refusal, checkpoint recovery, guard ownership, strict writer, and its integration/fault tests. Every commit is below 800 changed lines. Research and validation reports are separate commits. No remote push or production activation occurred.
