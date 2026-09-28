# Persistence adversarial review 1

Date: 2026-09-28. Workspace: `/Users/wpzhang/.codex/worktrees/panng-prototype/Codex-Panng`.
Scope: read-only review of ITERATION-PLAN, PROTOTYPE-RESULTS and actual history/rollout/thread-store/core sources. No runtime changes or tests performed.

## Verdict

Iteration A is necessary, but its current persistence wording is not implementable through the existing Session recording contract. **P1: existing ACKs do not prove fsync; P1: memory publishes before persistence and errors are swallowed; P1: response metadata alone cannot record independent operation transitions; P1: bounded readers silently skip malformed records and can stop before runtime origins.** These must be explicit implementation tasks, not assumptions.

Recommendation: **one new typed `RolloutItem::PanngCommit` record for an atomic runtime delta plus optional model-history append, and a versioned Panng checkpoint inside existing `CompactionResumeMetadata`.** Do not introduce a second database or a general multi-record transaction language in this iteration. Keep ordinary native records unchanged. A PanngCommit is one newline-terminated JSON record, containing a bounded delta and bounded history effects. Compaction remains one existing Compacted record with replacement history and the runtime snapshot at the same cut. Its companion world-state/settings fields need the additional treatment below.

This deliberately adds one enum variant and its exhaustive-match maintenance cost. That is more honest and less fragile than fabricating ResponseItems to carry internal updates, and substantially simpler than Begin/Commit groups that every reader must buffer and validate.

## Verified facts

1. `history/src/lib.rs:54–64`: `ResponseItemEnvelope` always contains a ResponseItem; `CodexHarnessMetadata` is metadata **of that item**. There is no metadata-only append. `history/src/compaction_resume_metadata.rs:15` only supplies state at a Compacted record. Neither can express a tool exit while inference sleeps without creating another record. Updating already-written metadata would break append-only history.
2. `rollout/src/recorder.rs:1035`: `record_canonical_items` returns after sending AddItems to a channel. `flush` at 1074 waits for a writer ACK, but `write_pending_once` at 1890 and `JsonlWriter::write_line` at 2085 use `File::flush`, not `sync_all`/`sync_data`. Source comments call this durable, but it is not a power-loss durability barrier. Buffered items are removed per successful line, so multi-item batches are not atomic.
3. `thread-store/src/local/live_writer.rs:311–377`: a per-thread writer lock serializes write_and_project; `durable_write(AppendItems)` queues then flushes. SQLite projection is explicitly rebuildable and may fail with a warning after canonical write. This is the correct ordering to retain, but not sufficient fsync semantics.
4. `thread-store/src/live_thread.rs:238–290`: append can successfully write canonical items then fail on derived metadata update. An Err is therefore an **ambiguous commit outcome**, not proof nothing persisted. Retry must be idempotent.
5. `core/src/session/mod.rs:3533–3667`: record_prepared_conversation_items mutates history first, then calls persist_rollout_items, then exposes raw items; its return is (). `persist_rollout_items` at 4509 logs errors and returns bool; it returns true when persistence is absent. `flush_rollout` similarly permits no live thread. Neither existing entry point can gate new side effects reliably.
6. `Session::replace_compacted_history` at 4071 mutates memory before persistence and locks only thread-settings persistence. It builds replacement history from caller-supplied items, with no append sequence cut. The Compacted record contains runtime-independent resume metadata, but WorldState, TurnContext and applied-settings are separate subsequent records. A crash between those records can expose an incomplete companion checkpoint.
7. `rollout/src/model_context.rs:32`: ModelContextScan stops at a Compacted with replacement_history and window_number. It does not validate runtime completeness. `thread-store/src/local/model_context.rs:167–187` applies it to reverse lineage reads and skips non-Parsed scan outcomes. Legacy mode uses full-history loading. Thus “bounded” currently means a suffix after compaction, not a hard byte bound, and no compaction can mean an arbitrarily large replay.
8. `RolloutRecorder::load_rollout_items` at 1103 tolerates malformed JSON/decode lines and increments parse_errors. That tolerance is unacceptable for state transitions authorizing execution. An unknown runtime version cannot be silently skipped.
9. Paginated fork uses `local/paginated_fork.rs::history_base_at_boundary`, `local/model_context.rs::load_for_fork` and `RolloutLineage::truncate_at`. The cut is frozen before reverse reading. Legacy last-N fork (`core/src/thread_rollout_truncation.rs:266`) simply splits a vector at a user boundary, which can discard runtime origins. Both require deliberate runtime handling.
10. `core/src/tools/parallel.rs::handle_tool_call_with_source` constructs an AbortOnDropHandle around `tokio::spawn` (line 196). Calling this factory before the durable ACK already starts execution; passing its returned future into a persistence-aware pool is too late.

## Alternatives

| Encoding | Independent transition | Atomic history/runtime | Maintenance / verdict |
|---|---|---|---|
| Only CodexHarnessMetadata + CompactionResumeMetadata | No; requires invented model item | One existing response/checkpoint can carry state, but no autonomous update | Reject as sole protocol |
| Runtime-only RolloutItem + metadata on ordinary outputs | Yes | Requires careful two-record delivery reconciliation or full cumulative metadata on each output | Viable, but more recovery states and harder audit |
| **PanngCommit + CompactionResumeMetadata** | **Yes, empty history effects allowed** | **Delta and appended envelopes share one JSON line; checkpoint shares Compacted line** | **Recommended: one variant, explicit projection/read updates** |
| Begin/items/Commit journal groups | Yes | Can atomically include any upstream record | Larger invasive transaction-aware scan, projection and fork rewrite; defer unless actual multi-record requirements force it |
| Sidecar operation database | Yes | No cross-log atomicity without another protocol | Reject for this stage |

## Proposed protocol (design, not existing behavior)

PanngCommit contains version, engine identity, thread/turn/execution epoch, monotonic runtime sequence, stable commit ID, previous runtime sequence, operation transition(s), and optional bounded `Vec<ResponseItemEnvelope>` history additions. Define this data in a small history-owned module or minimal shared data crate; avoid making history depend on core. Use explicit bounded types/validation rather than unconstrained serde_json::Value.

- **Intent commit:** original call identity, serialized dispatch description, captured permission/environment/catalog provenance, and exactly one running wire-output receipt can be one record. The normal upstream raw call may already exist; the intent must still be self-contained for recovery. After durable confirmation, publish the receipt/history state and only then construct the executor future. A failure before construction must not invoke hooks or external processes.
- **Ready commit:** bounded result or durable artifact reference, execution outcome, and optional completion notification history effects. No model output is necessary. A completion transition can exist with zero history additions while awaiting the next model step.
- **Delivery commit:** stable result IDs and the exact bounded notification envelopes are committed together before the next request snapshot. Distinguish “in context”, “request attempted”, and “response observed”; a canceled/lost request is not model consumption. Exactly-once model processing cannot be established from local HTTP send success.
- **Checkpoint:** version, sequence and execution epoch, active/unknown operations, Ready undelivered results, context delivery ledger and engine marker inside CompactionResumeMetadata. Capture history prefix at c. Under the same commit coordinator at installation s, form compressed prefix + original append suffix (c,s] and runtime@s. Commit before replacing memory. Never hold state/history locks across model inference or tool execution.
- A **single record is a recovery atomic unit**, not a guarantee an arbitrary-sized write is physically atomic. Ignore an unterminated torn tail, reject malformed complete interior records/sequence gaps/unsupported runtime versions, and never skip over one to dispatch. Do not claim checksum protection unless implemented.
- All fields needed for *joint* recovery must be in that single Compacted record. Existing WorldState/TurnContext/settings companion records must either be embedded in an added versioned checkpoint envelope and reconstructed from it, or proven optional/redundant for this mode. Merely appending companions after the checkpoint is not an atomic checkpoint.
- Add an explicit durable append API returning `{commit_id, durable_sequence}` through ThreadStore → LiveThread → LocalThreadStore → RolloutRecorder. The default for unsupported/custom/in-memory production stores is Unsupported, not ordinary append + assumed durability. Keep native best-effort recording API untouched.
- Writer ACK follows complete append, flush, file sync and required directory sync for first creation/rename. Record sync failure as uncertainty and freeze new dispatch until reconciliation. Do not resend the same commit blindly: query/read the canonical tail using commit ID/sequence under writer ownership. ACK loss and post-write metadata errors must not duplicate history effects.
- Existing write retry can reopen after error; verify partial-tail repair and ordinal recovery for this new path. Do not acknowledge a recovered file merely because all pending items were drained before the original fsync failure.

### Checkpoint frequency and boundedness

Runtime outstanding operations, retained undelivered results, notifications and artifact sizes need explicit caps. Existing model compaction frequency is not a bound on runtime-only journal growth. Trigger a joint snapshot at a runtime record/byte threshold even without model inference, preserving current model history unchanged; retain window identity and avoid pretending a new model compaction happened. That may require a PanngCommit checkpoint form recognized by ModelContextScan, rather than abusing Compacted semantics. If not implemented in iteration A, document the replay bound as incomplete and fail safely on configured maximum replay bytes. Do not silently discard records at the cap.

### Fork / rollback

First reconstruct at the exact frozen history cut; then derive child state. Preserve Ready results and delivery facts present at that cut. Strip execution ownership and reconnect capabilities; active parent jobs become inherited unknown/canceled observations. Parent completions after the cut never enter child state. Write a child-local initial joint checkpoint before any child dispatch. For last-N context truncation, derive runtime state before dropping the earlier prefix, then inject the child checkpoint; splitting the original records alone is incorrect. Rollback also cannot undo external side effects: retain outcome facts separately from context visibility and mark excluded active jobs non-owned. Unknown versions must prevent native fallback from silently continuing an active Panng session.

## Required file/method map

| Surface | Required change or verification |
|---|---|
| `history/src/lib.rs`, `rollout_payload.rs` | Add variant and wire conversion both ways; expose typed bounded payload; schema round-trip |
| `history/src/compaction_resume_metadata.rs` | Optional versioned joint runtime/checkpoint field; legacy absence distinct from empty runtime |
| `rollout/src/policy.rs` | Persist commit in both legacy/paginated modes; never leak internal payload into model or memory summaries |
| `rollout/src/recorder.rs` | ACK-bearing sync append; materialization and directory sync; idempotent uncertain outcome recovery |
| `rollout/src/model_context.rs`, `reverse_jsonl_scanner.rs` | Recognize complete runtime boundary; strict engine-aware corruption/version path; bounded scanning contract |
| `thread-store/src/store.rs`, `live_thread.rs` | Explicit capability and durable append API; separate canonical ACK from best-effort metadata projection |
| `thread-store/src/local/live_writer.rs` | Preserve per-thread serialization; sync before projection and success; no projection error masquerading as absent commit |
| `thread-store/src/local/model_context.rs` | Return runtime transitions/checkpoints along with model context; do not skip critical decode failures |
| `thread-store/src/local/paginated_fork.rs`, `rollout_lineage.rs`, `revert_thread.rs` | Frozen cut preservation; runtime-derived child initialization; rollback ownership semantics |
| `thread-store/src/local/thread_history_materialization.rs` | One record advances one ordinal even if internal commit has several model envelopes; projection rebuild equivalence |
| `app-server-protocol/src/protocol/thread_history_projection.rs::project_rollout_line`, `thread_history.rs` | Runtime-only commit produces no UI turn; appended envelopes project like original items with stable IDs; no duplicated raw output |
| `core/src/session/mod.rs` recording and replacement entry points | Serialize snapshot/append/install, propagate errors to Panng coordinator, publish after ACK |
| `core/src/session/rollout_reconstruction.rs` | Replay append effects exactly once and restore runtime at same checkpoint; thread completion and rollback interactions |
| `core/src/thread_rollout_truncation.rs`, `core/src/agent/control.rs` | Legacy and last-N subagent fork derive snapshot before source truncation |
| `core/src/panng/*`, `core/src/tools/parallel.rs`, tool stream dispatcher | Store unexecuted descriptors/factories; durable ACK before constructing executor; generation fencing |
| Migration/compression/delete lineage readers | Audit all exhaustive RolloutItem matches; preserve raw commit and stable offsets during recompression, reject destructive migration of unknown engine |

A new enum variant forces additional mechanical match updates outside this map; compiler errors identify exhaustive matches, but wildcard/serde skip paths need explicit search. Split this into storage codec/barrier, recovery/projection, then core integration changes. The first real diff will determine rebase cost; neither zero-impact protocol changes nor “metadata-only thin adapter” is currently credible.

## Failure sequences to prove

1. Intent append enqueued → process exits before write. No executor factory may have run.
2. Complete intent line written → sync fails. No new dispatch; restart may find intent, but may not infer whether prior execution occurred from it alone.
3. Sync succeeds → ACK lost → caller retries. Exactly one commit/receipt; no duplicate spawn.
4. ACK succeeds → crash before spawn. Resume marks outcome unconfirmed; do not automatically replay arbitrary Shell/MCP.
5. Spawn succeeds → external effect happens → crash before Ready. Resume reports indeterminate; never invent successful completion or rerun.
6. Ready record succeeds → crash before notification; resume retains result. Delivery commit succeeds → crash before request; same context notification is available without duplicating call output.
7. Compaction captures c → hook/result append at c+1 → joint checkpoint sync at s → crash before memory install. Restart and live state both retain suffix exactly once.
8. Checkpoint line complete → companion WorldState/settings write fails. Must still restore an internally complete state or refuse the checkpoint; do not advertise full atomicity from only runtime metadata.
9. Unknown/invalid runtime transition between valid checkpoint and Ready. Strict loader rejects; no silent skip via ScanOutcome/parse_errors.
10. Child cut before parent Ready, parent subsequently finishes. Child sees only its cut and owns no parent execution; a cut after Ready retains the result.

## Focused validation checklist

- Recorder fault injection at byte writes, complete-line flush, file sync, first-create directory sync, ACK send, reopen/tail repair; test all truncation offsets of a compact representative record.
- Replay two identical commit IDs, sequence gap, unsupported version, malformed interior vs torn final record; outputs and operation state compared as whole objects.
- LocalThreadStore legacy and paginated resume, compressed lineage, checkpoint and multi-page suffix, runtime-only transition with no model response, empty replacement history.
- SQLite deletion/rebuild yields same UI history and boundaries; internal transitions never become user messages/authorization or duplicate tool call results.
- Real Session append-during-compaction plus restart equivalence across supported compaction paths; model error/cancellation cannot publish uncommitted state.
- Executor **construction counter** remains zero on storage failure; tool body, process spawn, hooks and UI running status also remain absent. A factory that spawns internally is an essential regression fixture.
- Ready-before-delivery, delivery-before-request, canceled/lost model request, fork before/after Ready, last-N fork, rollback, post-cut parent updates.
- Custom store lacking durable capability rejects before enabling mode; no-live-thread/ephemeral session cannot return a false durable success.
- Target history/rollout/thread-store/core relevant suites and necessary app-server projection tests only; no workspace-wide run.

## What this review does not establish

No code was modified or compiled, so API shape and exact diff size remain proposals. File sync provides the selected local filesystem durability contract, not distributed exactly-once effects or surviving every storage hardware failure. Bounded resume, downgrade safety, and full checkpoint atomicity are unresolved until implemented and tested. Existing host APIs cannot be treated as evidence for these properties.
