# Panng A1 persistence contract

Design revision 1, 2026-09-28. This is an implementation contract, not implemented behavior. It refines PERSISTENCE-REVIEW-1 after inspecting canonical UI projection and history-mode rejection. No tests were run.

## Decision and scope

Use **one outer `RolloutItem::PanngCommit` JSONL record**, containing a bounded runtime transition and a restricted list of existing host records. That list can atomically include ResponseItems, ItemCompleted presentation events, and a Compacted record with its WorldState/TurnContext/settings companions. Do not recursively nest commits. No Begin/Commit multi-line groups and no second database.

This is a refinement of the preceding review's split encoding: **place the runtime checkpoint on the outer PanngCommit alongside Compacted, rather than duplicate it in CompactionResumeMetadata.** The latter remains the source of existing Codex resume fields. This keeps complete host and runtime recovery in one line and avoids modifying two independently authoritative runtime snapshots. Existing history metadata may identify individual notifications, but never serves as the operation journal.

A1 means storage, replay, fork and no-dispatch-on-storage-failure are production contracts. It does not yet grant production readiness to real tools: A2 supplies execution lifecycle integration. CLI activation stays unavailable until A2 validation. All unsupported stores fail capability preflight before an async turn begins.

## Dependency ownership

- `codex-panng-runtime`: runtime identities, bounded operation descriptors/state, transition validation. Depends on serde and standard library, not codex-core/history/protocol. Remove prototype history strings from production state.
- `codex-history`: outer commit and constrained existing host effects, using runtime data types. This adds one dependency history → panng-runtime. `CompactedItem` remains here.
- `codex-rollout`: byte framing, strict decode, sync writer, ordinal handling; depends on history as today.
- `codex-thread-store`: durable capability/API, branch acquisition, lineage and child checkpoint creation.
- `codex-core`: builds host effects through existing preparation/truncation machinery, owns commit coordinator and publishes only after ACK. The core adapter is the only layer converting tool descriptors and runtime records into model fragments.

If dependency review shows panng-runtime acquires any host/storage dependencies, split its serializable data into `codex-panng-protocol`; do not invert history → core or add a cycle. The current small pure runtime crate is an acceptable home only while that direction remains strict.

## Rust data contract

These are intended field/API shapes. Newtypes have private fields and validated constructors; derive Serialize plus validated custom Deserialize. Explanations below define validations completely enough to implement; do not deserialize then assume constructor invariants survived.

```rust
// codex-panng-runtime (private modules, explicit public exports)
pub struct BranchId(String);       // UUID string, <= 36 ASCII bytes
pub struct ExecutionEpoch(String); // UUID string, <= 36 ASCII bytes
pub struct CommitId(String);       // UUID string, <= 36 ASCII bytes
pub struct OperationId(String);    // UUID string, <= 36 ASCII bytes
pub struct ResultId(String);       // UUID string, <= 36 ASCII bytes
pub struct TurnId(String);         // nonempty, <= 128 UTF-8 bytes
pub struct LogicalStepId(String); // UUID; stable across transport retries of one sampling request
pub struct ProviderResponseId(String); // nonempty, <= 256 UTF-8 bytes
pub struct CallId(String);         // nonempty, <= 256 UTF-8 bytes
pub struct ToolName(String);       // nonempty, <= 256 UTF-8 bytes
pub struct BoundedJson(String);    // valid JSON object, <= 64 KiB UTF-8
pub struct Digest(String);         // 64 lowercase hex SHA-256 chars

pub struct DispatchDescriptor {
    pub tool: ToolName,
    pub arguments: BoundedJson,
    pub environment_digest: Digest,
    pub authorization_digest: Digest,
    pub catalog_digest: Digest,
}
// Digests identify the captured invocation, never substitute for fresh permission checks.
// A1 never reconnects or replays descriptors; no secret/token material is journaled.

pub enum ExecutionOutcome { Succeeded, Failed, Canceled, Indeterminate }
pub enum OperationState {
    Prepared,
    Ready { result_id: ResultId, outcome: ExecutionOutcome, result: StoredResult },
    InContext { result_id: ResultId, outcome: ExecutionOutcome, result: StoredResult,
                delivery_commit: CommitId },
    InheritedUnknown,
}
pub enum ResultEncoding { CodexResponseEnvelopeV1 }
pub enum StoredResult {
    Inline { encoding: ResultEncoding, payload: String }, // validated JSON, <= 4 KiB
    Artifact { encoding: ResultEncoding, sha256: Digest, bytes: u64 }, // <= 16 MiB
}
pub struct OperationRecord {
    pub id: OperationId,
    pub origin_branch: BranchId,
    pub owner_epoch: ExecutionEpoch,
    pub turn_id: TurnId,
    pub logical_step_id: LogicalStepId,
    pub call_id: CallId,
    pub provider_response_id: Option<ProviderResponseId>,
    pub descriptor: DispatchDescriptor,
    pub state: OperationState,
}
pub struct OperationChange {
    pub expected_revision: Option<u64>, // None means this operation must not exist
    pub revision: u64,                 // create = 0; otherwise previous + 1
    pub operation: OperationRecord,
}
pub struct RuntimeSnapshot {
    pub execution_epoch: ExecutionEpoch,
    pub operations: Vec<(u64, OperationRecord)>, // sorted unique OperationId
}
pub enum RuntimeMutation {
    Delta { changes: Vec<OperationChange> },
    Snapshot { state: RuntimeSnapshot, reason: SnapshotReason },
}
pub enum SnapshotReason { Initial, Compaction, ReplayBound, Fork, ResumeFence }
```

Prepared is an intent whose external outcome is unknown after a crash; the protocol intentionally does not infer dispatch from a “Started” record. There is no persisted “safe to rerun” flag for arbitrary Shell/MCP. Ready persists actual observed outcome. InContext means notification committed to history, **not model consumption**. InheritedUnknown owns no execution; owner_epoch is retained solely as provenance and never authorizes a child.

Terminal records may be pruned only in a joint snapshot after their complete notification is in retained context, or after an explicit outcome summary survives context compaction. Keep deduplication identity for the current execution epoch until it is fenced and drained. No late update may resurrect a pruned operation: coordinator accepts only registered pending operation IDs and matching epoch; duplicate result IDs are ignored after comparing content digest. Unknown IDs are an error, never a fresh create.

```rust
// codex-history
pub struct PanngCommitV1 {
    pub branch: BranchId,
    pub commit_id: CommitId,
    pub sequence: u64,
    pub previous_sequence: Option<u64>,
    pub mutation: RuntimeMutation,
    pub host_effects: PanngHostEffects,
    pub checkpoint: Option<PanngHostCheckpoint>,
}

pub enum PanngCommit { V1(PanngCommitV1) }

// Private Vec; custom Deserialize validates whitelist and limits before publication.
pub struct PanngHostEffects(Vec<RolloutItem>);

// Add to existing RolloutItem:
// PanngCommit(PanngCommit)
```

Wire representation has `type: "panng_commit"`, payload `{ "version": 1, ... }`. Unknown version yields a dedicated error, not a deserializer fallback. RolloutItem already has custom serde wire mapping; reuse it for host effects, avoiding new duplicate definitions of ResponseItemEnvelope or CompactedItem. `Vec` breaks recursive type size; constructor rejects any nested PanngCommit and SessionMeta. A streaming raw-size/depth guard precedes serde, so recursive malicious JSON cannot allocate arbitrarily before validation.

**Whitelist:** ResponseItem, Compacted, WorldState, TurnContext, TokenUsageRecord, RetainedContext, SecurityRiskScore; and EventMsg variants ItemCompleted, TurnStarted, TurnComplete, TurnAborted, ThreadSettingsApplied, ContextCompacted. Other events stay on native append path only when they do not alter async history/runtime recovery. Effects use existing upstream validation and history preparation. Runtime-only transitions have an empty list. InterAgentCommunication records affecting current model context must either be admitted as a paired bounded effect using existing provenance logic in A2, or async mode must reject their activation before executing them; they cannot bypass the coordinator.

Limits: <= 64 effects/commit, <= 32 changed operations/delta, <= 64 retained operations/snapshot, <= 256 KiB serialized delta commit, <= 16 MiB snapshot commit. Every model fragment retains existing stricter limits (Panng text body currently 768 bytes), independent of journal limits. Snapshot overflow stops new dispatch and reports a storage-capacity error; it does not silently truncate history. Large tool results become artifacts, with file and containing directory synced **before** committing the digest reference. Missing artifact causes explicit indeterminate/error recovery, never an empty successful result.

Host-effect ordering is canonical. A Delta has checkpoint None and only append effects. A Snapshot has checkpoint Some, an empty append list, and a complete runtime snapshot. Compacted is forbidden in append effects and companions; it appears only in the explicit checkpoint field. A snapshot's companions contain the full WorldState baseline when present, optional TurnContext and current ThreadSettingsApplied. No checkpoint constituent is also emitted outside the commit.

```rust
pub struct PanngHostCheckpoint {
    pub compacted: CompactedItem,
    pub companions: PanngHostEffects,
    pub parent_cut: Option<codex_protocol::protocol::HistoryPosition>,
}
```

CompactedItem remains unchanged. Its replacement_history and complete existing resume metadata, together with companions in the same outer line, cover the host snapshot. At compaction install, replacement_history contains compressed <= c plus preserved append suffix through s; runtime is captured at s.

## Ordinary deltas versus snapshots

A normal tool intent/exit/delivery serializes only changed operations and newly appended history/UI items, never full history. A snapshot serializes full current retained host context and runtime only on initial activation, compaction, resume fencing, fork creation or replay bound.

ReplayBound snapshots preserve current window identity. The CompactedItem field is a persisted host checkpoint representation, not a command to run compaction. Reconstructing it must not call compaction hooks, increment window number or emit ContextCompacted events. That is why it is nested inside an explicit checkpoint instead of emitted as an ordinary top-level Compacted item.

Trigger ReplayBound at the earlier of 256 Panng commits or 8 MiB canonical bytes since last joint snapshot. Maintain a maximum suffix read of 32 MiB and 4096 outer records including native events; if the bound would be crossed, checkpoint before admitting more writes. Readers exceeding these caps fail explicitly, never scan indefinitely or drop records. Native best-effort events for an active Panng session must count toward this byte/record budget under the same coordinator. Legacy sessions have no such bound and remain native; enabling Panng creates a new marked branch with an initial checkpoint.

## Shared projection boundary

Provide one history-owned borrowed expansion API so consumers reuse upstream behavior without copying Panng semantics:

```rust
pub enum HostReplayEffect<'a> {
    Append(&'a RolloutItem),
    Checkpoint(&'a PanngHostCheckpoint),
}
// PanngCommit::host_replay_effects() -> impl Iterator<Item = HostReplayEffect<'_>>
// Delta yields each append; Snapshot yields one Checkpoint.
```

A small core reconstruction adapter expands Checkpoint into the existing replacement-history + companion replay path, while restoring runtime from the same outer commit. It does not invoke live compaction APIs. App-server UI projection uses one wrapper around its existing projector: Delta effects are projected in order into a single change set, validated to affect at most one visible item and at most one turn boundary; checkpoint has no synthesized UI compaction event. Runtime-only deltas produce an empty change set. Do not universally flatten a checkpoint into model appends, and do not give inner effects fabricated independent ordinals.

Reader/storage consumers retain the outer record so strict version/sequence validation and atomic fork cuts remain available. `ModelContextScan` recognizes validated Snapshot commits as complete boundaries; the core adapter alone handles runtime restoration. Migration/compression preserve the outer bytes/record rather than interpreting runtime. Generic consumers not concerned with model reconstruction or UI simply retain/ignore the recognized variant according to their role, without implementing state transitions.

## Durable API and store preflight

Do not change the semantics of upstream append_items/flush_thread for native consumers. Add narrowly scoped optional methods whose default returns Unsupported.

```rust
pub enum DurableCommitCapability { Unsupported, PanngV1 }
pub struct DurableCommitReceipt {
    pub commit_id: CommitId,
    pub sequence: u64,
    pub end: codex_protocol::protocol::HistoryPosition,
}
pub enum DurableCommitFailure {
    Rejected { message: String }, // definitely not accepted; no sequence consumed
    Uncertain { commit_id: CommitId, message: String },
}
pub enum ReconcileOutcome {
    Durable(DurableCommitReceipt),
    Absent,
    Corrupt { message: String },
}
// New ThreadStore methods (existing ThreadStoreFuture alias remains):
// fn durable_commit_capability(&self) -> DurableCommitCapability;
// fn commit_panng(&self, thread_id: ThreadId, commit: PanngCommit)
//    -> ThreadStoreFuture<'_, DurableCommitReceipt>;
// fn reconcile_panng(&self, thread_id: ThreadId, commit_id: CommitId)
//    -> ThreadStoreFuture<'_, ReconcileOutcome>;
```

The repository's generic ThreadStoreError must gain typed rejected/uncertain variants or embed DurableCommitFailure; do not reduce uncertainty to string matching. Local store returns PanngV1 only for canonical file storage with exclusive writer ownership and supported sync semantics. Memory/test stores may provide a separately named simulated capability available only to tests, never production. A1 officially supports LocalThreadStore only. Remote/custom stores return Unsupported; later support requires explicitly implementing the complete API and fault suite, because transport request success/flush is insufficient. Ephemeral/no-live-thread sessions fail preflight. Capability is checked before creating the marked branch and again when reopening its writer; loss of capability is terminal for new dispatch.

The receipt's HistoryPosition points after the **outer** record. Inner effects do not have independent ordinals or fork cut positions. A1 requires each outer commit to affect at most one visible item and at most one turn boundary. Validate this using the actual centralized projection semantics, not by counting host effects: multiple effects for the same item may collapse to one, while one event can affect a boundary. Current item cursors contain only rollout ordinal and use strict inequalities, so allowing two distinct visible items in one ordinal would lose one when page_size=1. Multiple ready results may share one bounded notification, or be committed as separate records with corresponding runtime deltas. Checkpoints synthesize no visible items. Reject invalid multi-item commits before append. Future support for multiple visible items requires a versioned composite cursor/subindex, not merely a projection wrapper. Fork cuts use complete outer boundaries; ambiguous intra-record cuts are unsupported.

## Write/publish and failure state machine

1. Coordinator serializes all history-affecting mutations; prepares prospective runtime/history values without exposing them, assigning commit ID and next sequence. It holds its own async commit mutex, never the general Session state mutex across I/O.
2. Writer accepts complete validated PanngCommit, serializes one JSON line, appends under the existing per-thread writer lock, flushes, sync_all on the file; first creation also syncs parent directory and any newly created directory ancestors as required. Platform-specific unsupported durable creation fails preflight/commit rather than silently weakening the promise.
3. Canonical ACK is separate from SQLite/UI/metadata projection. Projection may lag and be rebuilt; its failure does not erase or change a successful canonical receipt.
4. Only after ACK, publish prepared history/runtime atomically under Session state ownership, then emit raw/UI events and construct execution futures. Failed in-memory publication after ACK fences dispatch and reloads the canonical state; it never retries the side effect.
5. Rejected: discard prospective values, retain old published state, return visible failure and start no tool. Uncertain: freeze new dispatch and model requests; retain old published state; reconcile using the same commit ID while holding writer authority. Durable reconciled outcome publishes once; Absent allows resubmitting the **same** commit; Corrupt closes the async turn and requires explicit repair.
6. Crash after durable intent but before factory call is indistinguishable from later crash without Ready. Resume fences epoch, publishes Prepared as indeterminate, and does not replay. ACK loss within the same uninterrupted coordinator can reconcile then dispatch once because the factory has provably not been called; after restart that proof no longer exists.
7. Tool finishes while storage is failed: retain bounded result in pending memory and stop new dispatch. If failure cannot be reconciled, report indeterminate on resume. Do not display a durable successful receipt before storing the result. The task finalizer reports storage uncertainty separately from tool outcome.

Append idempotency requires writer to remember/validate the last commit ID and sequence, persisted within the record, and to reconcile the bounded suffix under lock. Same ID/different bytes is corruption. On partial write, truncate to last complete record under exclusive ownership and sync before retry. On sync failure after a complete line, do not automatically append again; re-sync the existing exact record and return its receipt if successful. An operation result cannot be accepted in a different branch or fenced epoch.

## Unsupported versions and old native readers

A new optional engine field alone is insufficient: old readers can ignore it and skip unknown PanngCommit records. Use explicit `ThreadHistoryMode::PanngV1` serialized `"panng_v1"`; preserve paginated storage semantics through one centralized `uses_paginated_storage()` helper and update every mode match by actual behavior. Update schema/generated types when externally exposed. This is a real compatibility footprint, not a zero-cost option.

The current baseline's `rollout/src/recorder.rs::reject_unknown_thread_history_mode` rejects this unknown mode before canonical history load; list readers also invoke it. Verify old-baseline resume/read rejection with an actual fixture. Do not promise every historic release behaves identically: deploy fork data under a separate CODEX_HOME by default, and never auto-export it as native. Directly importing marked records through an API must validate the header before any tolerant decoder.

Updated readers reject unsupported commit version, missing initial branch snapshot, duplicate conflicting commit ID, runtime sequence gaps, nested commits, malformed complete critical records, invalid artifact digests and branch mismatches. The strict check happens at recorder reopen, local model-context loading, lineage/fork loading, migration/materialization, and any direct rollout import. It precedes the current `ScanOutcome::Rejected => continue` and parse_errors tolerance. Only an unterminated final line may be repaired; an invalid complete interior line cannot be skipped.

Native mode in the new fork refuses to resume a PanngV1 branch. A deliberate native export requires a new thread with resolved terminal outcome context, no inherited execution authority, and a standard native header; it is not an in-place toggle. Existing native sessions keep legacy/paginated formats and behavior.

## Branch, resume, fork and rollback identity

BranchId is a fresh UUID per independently writable thread; parent operation IDs retain origin_branch. A fork has a new BranchId and ExecutionEpoch. The host checkpoint includes parent frozen HistoryPosition as provenance; runtime types do not depend on that host type. Parent live tail is never followed after cut resolution.

A resume keeps BranchId, acquires exclusive writer ownership and creates a new ExecutionEpoch using a ResumeFence snapshot before any dispatch. Prepared operations from older epoch become Ready/Indeterminate with explicit bounded notification; observed Ready/InContext results survive. No stale execution update is accepted in the new epoch. The process writer lock is required, not a replacement for epoch checks.

For forks, reconstruct runtime at exact parent cut before last-N context trimming. Ready outcomes remain transferable, InContext stays represented in copied retained context, and Prepared becomes InheritedUnknown with no execution ownership. Commit one initial child snapshot and sync before child dispatch. Parent changes after cut have no route to the child. Forking a child recursively repeats this rule. Rollback creates a new branch/snapshot derived at the requested cut; it must carry an explicit outcome summary for any real effects that cannot be undone and cannot resume excluded executions. In-place truncation while jobs run is rejected.

## Implementation stages and acceptance

**A1a — storage primitives:** new typed record/codec, strict decoding, durable append/reconcile API, explicit mode rejection, raw writer fault tests. No core runtime activation; a synthetic record fixture is sufficient. Core and model input are unaffected. This stage can land independently and must prove no fake fsync ACK.

**A1b — host checkpoint/recovery:** commit coordinator integration, checkpoint expansion and UI projection, bounded scans, resume/fork epoch fences, real Session compaction race and factory-construction-zero regression. Core activation remains test-gated until all effects bypassing the coordinator are identified and routed. Test native behavior with existing paths unchanged.

Only after A1b may A2 enable real tool dispatch. Neither stage is complete with a sidecar fallback, an unbounded reverse scan, an ignored codec version, or an ordinary flush standing in for the explicit barrier. Split further into codec/API/projection/core commits if actual diff exceeds repository review limits; maintain one documented data contract across those commits.

### Smallest first implementation that exercises the real store

Do not introduce all OperationRecord/DispatchDescriptor types before anything consumes them. First implement a reusable **single-record sync primitive**, tested through real LocalThreadStore using an existing persisted RolloutItem. This is A1a.1, an internal storage commit with no CLI activation and no Panng history-mode changes yet.

- Recorder: add `append_synced(item: RolloutItem) -> io::Result<DurableRecordPosition>` as one queued command that appends one canonical line and performs file/directory sync. Earlier queued writes must be ordered before it. This is not a multi-record transaction.
- ThreadStore: optional `append_synced_record(thread_id, item) -> ThreadStoreFuture<'_, DurableRecordPosition>` with default Unsupported. Local implementation holds its existing per-thread writer lock, calls recorder, then performs best-effort projection. LiveThread exposes this narrow method without appending ordinary best-effort metadata errors to the canonical ACK.
- `DurableRecordPosition` contains canonical `HistoryPosition`; keep it storage-owned and independent of runtime. During the final Panng commit wrapper, combine this position with the already validated commit ID/sequence to produce DurableCommitReceipt. An uncertain sync error is typed from day one.
- Capability at this stage is `SingleRecordSyncV1`, distinct from final `PanngV1`; production async activation requires the latter after codec/recovery integration. Existing/custom stores are Unsupported by default. Final `commit_panng` is a coordinator wrapper over this primitive, not a second writer implementation.
- Real-store tests: create deferred local thread; append an existing model-invisible record; confirm disk/reopen visibility and returned cut; inject file-sync and first-directory-sync errors; ensure error propagates through LiveThread; fail projection after successful sync and still return canonical receipt; verify unsupported custom store rejects. Test fault hooks observe the actual sync call, because reopen visibility by itself cannot prove fsync.

This first commit intentionally makes no claim that batches are atomic, no claim of operation deduplication, and no production feature switch. The primitive remains useful and unchanged when the next small commit adds PanngCommit, strict decoding and reconciliation. Third, wire it to bounded recovery/checkpoint projection and only then construct runtime descriptors at real call sites. The parent agent schedules all builds and tests; this design author has run none.

## Independent review disposition (2026-09-28)

- Cursor atomicity P1 accepted: A1 permits at most one distinct visible item / one turn boundary per outer commit; page_size=1 forward and reverse paging must cover every item exactly once. Runtime/checkpoint atomicity does not imply existing UI cursor compatibility.
- This is a design restriction pending codec/projector enforcement and tests; it is not yet a production guarantee.

- Early-read P1 accepted: unified borrowed history traversal/accessors must also feed pre-reconstruction settings (disabled plugins, ThreadSettingsApplied and TurnContext), token usage and thread-store resume_multi_agent_version. Expanding only inside rollout_reconstruction is insufficient. Restore-equivalence tests compare these fields before and after checkpoint, including settings absent by intent. Native top-level records remain supported by the same accessors.

- Delivery scope clarified: A1 persists result facts and InContext delivery only. Request-attempt/response-observed telemetry is diagnostic; it is not a consumption acknowledgement or permission to garbage-collect a result. No exactly-once model-processing claim. Canonical context restoration is the recovery contract.
- Origin identity P1 accepted: the admission key is (origin branch, owner epoch, logical step ID, call ID). Transport retries keep the logical step; a new sampling step receives a new ID. Same-key different descriptor is corruption; same-key existing intent never constructs another executor. A provider response ID, when available, is provenance rather than the sole deduplication key. These invariants need retry/reconnect tests before real tools are enabled.

- Result provenance disposition: persist the complete response envelope (including metadata) through a versioned host codec, inline or by content-addressed artifact. Pure runtime stores opaque payload/reference and never interprets Codex fields. The host decoder validates allowed tool-output shapes and preserves provenance before deriving a bounded notification. CodexResponseEnvelopeV1 needs a fixed codec and migration fixtures; tagging unversioned upstream serde output is insufficient. This closes the design gap only; codec and artifact delivery remain future implementation work.
