# Strict Panng reopen and reconciliation plan

2026-09-28. R1 implemented and verified; R2 revised after RECONCILE-PLAN-REVIEW.md, implementation in progress. Based on NEXT-PERSISTENCE-PLAN, revised PERSISTENCE-CONTRACT, current recorder/synced.rs, native opener, ordinal parser and reverse scanner.

## Decisions

1. Reuse the existing RolloutRecorder command queue and file handle. Add strict state/reader modules; do not build a sidecar journal or second writer.
2. A marked file contains one SessionMeta header followed only by versioned PanngCommit records. Native noncritical events can be effects inside these commits or remain transient. No uncoordinated top-level append after the header. This simplifies sequence, byte bounds and crash recovery; a native AddItems on a strict writer must return a visible error rather than silently bypassing the journal.
3. One unresolved write per writer. On uncertainty, permit only reconciliation of the identical attempt, shutdown/discard, and read-only status. Do not permit another operation or new commit while outcomes are unresolved.
4. Reconcile exact already-written bytes under the retained writer lock. A newline-terminated malformed record is corruption; an unterminated final fragment is the only automatically truncatable case. Never append a newline to it.
5. Strict recovery reads at most 32 MiB and 4096 outer records to the latest joint snapshot, plus a separately bounded first header (256 KiB). Exhaustion is a typed refusal, not a full-history fallback.
6. Native constructors detect marked/unsupported headers **before** any append preparation, timestamp mutation, compression materialization or newline repair. Until complete recovery adapters exist, they reject marked files. A dedicated strict constructor is the only future entry to the new format.

## Required codec interface

The parent-owned history codec should expose equivalent methods, with final naming agreed before implementation:

```rust
impl PanngCommit {
    pub fn commit_id(&self) -> &CommitId;
    pub fn branch(&self) -> &BranchId;
    pub fn sequence(&self) -> u64;
    pub fn previous_sequence(&self) -> Option<u64>;
    pub fn is_checkpoint(&self) -> bool;
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, PanngCodecError>;
}
```

`canonical_bytes` is the serialized **commit payload**, excluding outer timestamp and ordinal. It is deterministic and lossless across encode/decode/encode, including metadata floating-point fields and map ordering. The codec tests prove this property. If lossless float round-trip is unavailable, do not use deserialize/reserialize for identity: retain a bounded original payload byte slice via the strict decoder and compare those bytes instead. Choosing that alternative requires the codec to return raw spans, not a late semantic-equality workaround.

Same ID + different bytes is a conflict, even if the operation transition would otherwise appear equivalent. Do not compare `serde_json::Value` alone: it can erase distinctions the commit-identity contract promised to preserve. Same bytes + different ID is not idempotent. Branch, runtime sequence and outer ordinal are checked independently.

For live uncertain writes, preserve the entire prepared line, including its original timestamp; no determinism assumption is needed to compare that exact attempt. Cold recovery never automatically replays an old intent, so it does not need to reconstruct a lost timestamp.

## Writer state and APIs

Add these private states inside recorder's strict submodule, with opaque public receipts/results. Existing SyncRecordError still differentiates rejected vs uncertain errors.

```rust
struct PreparedAppend {
    commit_id: CommitId,
    sequence: u64,
    ordinal: u64,
    start_offset: u64,
    line: Vec<u8>,       // original complete serialized line including newline
    payload: Vec<u8>,    // deterministic codec bytes, bounded; can use an Arc/range to avoid copying
    candidate: RuntimeSnapshot, // apply against writer-owned prior before any I/O
}

enum StrictWriteState {
    Healthy {
        branch: BranchId,
        runtime: RuntimeSnapshot,
        next_sequence: u64,
        next_ordinal: u64,
        end_offset: u64,
        last: Option<CommittedAppend>,
        bytes_since_checkpoint: u64,
        records_since_checkpoint: u32,
    },
    NeedsResumeFence { recovered: Box<HealthyState> }, // last=None; only ResumeFence admission
    Uncertain { attempt: PreparedAppend, prior: Box<HealthyState> },
    Corrupt,
}
struct CommittedAppend {
    commit_id: CommitId,
    sequence: u64,
    canonical_payload: Vec<u8>,
    receipt: SyncedRecord,
}
```

The code should factor Healthy fields into one `HealthyState` struct instead of duplicating the illustration's fields. Keep only the last committed payload for live lost-ACK retry and one uncertain attempt. This is bounded by at most two record buffers. Before admission, validate projected journal bounds and checkpoint requirements; neither buffer contains full history for ordinary deltas.

Proposed public methods:

```rust
// Dedicated constructor; never calls native open_rollout_for_append.
RolloutRecorder::resume_panng_with_writer_lock(
    config: &impl RolloutConfigView,
    path: PathBuf,
    writer_lock: Arc<WriterLockGuard>,
    expected_branch: BranchId,
) -> io::Result<(RolloutRecorder, RecoveredPanngSuffix)>;

RolloutRecorder::append_panng(
    &self, commit: PanngCommit,
) -> Result<SyncedRecord, SyncRecordError>;

RolloutRecorder::reconcile_panng(
    &self, expected_commit: PanngCommit,
) -> Result<ReconciledAppend, SyncRecordError>;

enum ReconciledAppend {
    Durable(SyncedRecord),
    Absent,
}
```

No public “clear fence” method. Reconcile accepts the full expected commit, not just ID, to prevent a caller from accidentally accepting a different record with the same ID. On a Healthy writer, retry of the **last** identical commit returns the retained receipt after checking file identity/cut remains unchanged; a conflicting last ID rejects. Older IDs are not appended or silently treated as absent: return an explicit unsupported/stale retry error. The coordinator does not advance beyond an unacknowledged commit, so legitimate in-process retries need only this bounded cache. Cold recovery reconstructs state at the verified latest snapshot and fences old execution; it does not rerun old callers.

A successful writer ACK may be dropped by the receiving task. Writer state is Healthy with `last`, so the same request can be acknowledged again without appending. An I/O or synchronization failure is Uncertain and blocks different commits regardless of sequence. A second queued append cannot pass an uncertain attempt. Normal serialized coordinator usage is required; an API client may not deliberately abandon one commit then assume another proves its execution.

The strict constructor retains the supplied existing WriterLockGuard. Before any mutation it must verify guard thread identity and canonical lock namespace against SessionMeta.id and config Codex home, plus expected BranchId. Wrong thread/home/branch rejects without changing bytes. No new lock namespace is introduced.

The writer owns the committed RuntimeSnapshot. Admission runs mutation.apply against that exact prior state inside the serialized queue and stores the candidate before possible I/O. Only a real successful sync or full-line reconciliation promotes candidate, sequence, ordinal, cut and last receipt together. Absent restores the entire prior state.

Every cold reopen enters NeedsResumeFence with an empty last receipt cache. PanngReadCut is diagnostic/read evidence and can never become a durable ACK. Only a valid fresh-epoch ResumeFence matching the recovered state may be appended first. Ordinary deltas and old intent retries are rejected until that fence is durably acknowledged; a second restart requires another fresh fence. Uncertainty while writing this fence must preserve the same admission restriction.

## Live reconcile algorithm

Run within the writer command loop with exclusive ownership. Do not reuse native `write_pending_with_recovery` or `open_log_file`.

1. Confirm commit identity, canonical payload, predecessor and branch match the stored uncertain attempt. Any mismatch returns DefinitelyRejected and leaves the original fence intact.
2. Flush the owned file handle to finish pending async writes before inspecting its length. If that fails, remain Uncertain. Read exact bytes from the known attempt.start_offset, using bounded reads; compare against the cached original line.
3. File length equals attempt.start_offset: no target bytes exist. Sync the current file state and directories if needed; restore prior Healthy state and return Absent. Caller may resubmit the identical commit. No tool has been authorized by this outcome.
4. Tail is a proper byte prefix of the expected line and contains no complete trailing record: truncate to start_offset and sync file and required directory state. Only after that ACK restore prior Healthy state and return Absent. Failure during truncate/sync remains Uncertain. Never retain or complete a partial record by appending newline.
5. Tail equals the complete expected line: re-sync the **existing** line and required directories. On success, advance ordinal/sequence/counters exactly once, retain last payload/receipt and return Durable. Do not append anything.
6. Tail differs, contains extra bytes/records, or complete bytes decode to a conflicting record: set Corrupt, reject automatic recovery and preserve evidence. An uncertain fsync does not justify rewriting a complete record.

An incomplete tail that is not a prefix of the specific live attempted line is corruption, not automatically truncatable. Cold reopen cannot know a lost attempted payload; there it may remove a final incomplete line only after validating all complete suffix records and the latest checkpoint, and recording/reporting that repair occurred. Neither path asserts that arbitrary external side effects are replayable.

## Strict cold reader and bounded tail repair

Do not reuse `ReverseJsonlScanner::with_max_record_bytes` unchanged: current implementation silently drops oversized records, while strict recovery must fail. Do not reuse ordinal_state_for_rollout: it skips Rejected records. A compact new `strict_jsonl.rs` should use bounded `Read + Seek` and return each record's raw span and complete/torn status.

Implemented read-only API (reuse R1 rather than creating a second parser):

```rust
read_panng_suffix(reader: impl Read + Seek) -> io::Result<RecoveredPanngSuffix>;
// RecoveredPanngSuffix: header_bytes, thread_id, branch, runtime,
// commits (checkpoint then suffix), checkpoint_offset, cut, torn_tail_bytes.
// PanngReadCut { ordinal, end_byte_offset } is not a durability receipt.
```

R1 independently verifies the header (256 KiB) and initial joint checkpoint (16 MiB), then a bounded latest-checkpoint suffix (32 MiB / 4096 records). The first and latest checkpoint can be the same physical record. Root commit identity cannot be reused in a later suffix record. Read-only recovery never truncates or returns a repaired indicator; `torn_tail_bytes` reports pending repair.

Read limits are private fixed production policy or validated not-to-exceed constructors. Tests may use smaller limits without exposing arbitrary larger ones to callers. The header read has a separate 256 KiB cap and requires one complete SessionMeta record; an incomplete initial header is refused, never synthesized. Read the header before tail allocation.

Algorithm: capture file length under lock; locate last newline using bounded backward chunks; if file ends mid-line, mark that fragment as tentative torn tail. Scan complete records backwards, rejecting malformed JSON, unknown versions/types, blank complete lines, oversized records and non-contiguous ordinals. Stop only at a valid joint Panng snapshot, never at an ordinary Compacted record. Reverse the collected suffix, validate branch/sequence/predecessor chain and replay runtime transitions from the checkpoint using the runtime journal validator. Only after the entire bounded suffix validates may writable reopen truncate tentative torn bytes and sync; read-only readers return the verified cut and a pending-repair indicator without mutation.

A cut used for fork must land on a complete outer record and match its ordinal. It must not round forward to include parent tail. If the requested cut is before the first child checkpoint and would require following parent lineage, A1 local strict writer refuses it; the future thread-store lineage loader can explicitly provide bounded segments whose thread IDs and cuts are verified. First implementation supports self-contained local snapshots, not transparent arbitrary cross-file migration.

Worst-case memory is bounded by bytes/record caps; check raw record length before serde allocation. Keep all unknown-version and size-limit errors fatal. Do not scan past a newest invalid checkpoint looking for an older “usable” one, as doing so could conceal accepted runtime transitions.

A complete checkpoint is authoritative for state before its cut; bounded reading does not verify the complete historical prefix. This is a normal checkpoint trust boundary, not whole-log bit-rot verification. No checksum/digest-chain integrity claim is made until implemented.

## Reopen before native mutation

Current native resume path is `new_inner(Resume)` → `open_rollout_for_append` → compression materialization → `ensure_rollout_is_newline_terminated` → ordinal_state_for_rollout. Therefore adding a strict scan after construction is too late.

Change native `open_rollout_for_append` preflight to read the first bounded record using a read-only compression-aware reader and inspect raw history_mode before materializing/opening writable. Unknown values, specifically panng_v1, return Unsupported and preserve file bytes. If this fork later parses PanngV1 as a valid enum, the native preflight must still explicitly reject that known-but-wrong mode. It cannot rely solely on `reject_unknown_thread_history_mode` after extending the enum.

The dedicated strict constructor opens the uncompressed existing file directly with read/write/append, validates/reconciles it under the guard and passes the already validated ordinal state and handle into the existing rollout writer task. It never invokes native newline normalization. A1 strict reopen rejects compressed input with Unsupported instead of triggering migration; supporting compressed lineage later requires a separately validated publication path. Native compressed files retain their existing behavior.

Strict creation requires durable SessionMeta + initial joint checkpoint before accepting Delta commits. If crash leaves only a valid header, report IncompleteInitialization and allow explicit recreate/discard while no external execution has happened; do not turn it into an empty healthy session. No Panng code path changes a native existing header in place.

## Marked mode and unknown readers

Wire mode is `panng_v1`, distinct from legacy/paginated. Old baseline `reject_unknown_thread_history_mode` rejects it; test direct load and list discovery using an old-version fixture/parser. Do not promise arbitrary historic binaries reject it, so separate fork CODEX_HOME remains deployment isolation.

Until ThreadHistoryMode gains the supported variant, codec tests may create raw marked fixtures read by the dedicated strict header decoder; no production recorder exposes such sessions. Native append preflight can reject the raw marker before enum decoding immediately. After enum integration, maintain explicit refusal in unsupported thread-store/core/CLI paths until their recovery adapters are implemented. Missing checkpoint/recovery support is Unsupported, not an empty result or native fallback.

## First small implementation and ownership map

Implement in two bounded pieces, neither starts real tools:

**R1 — strict read + non-mutating native refusal**

- `rollout/src/strict_jsonl.rs` and `_tests.rs`: bounded raw spans, complete/torn classification, strict codec parse and checkpoint suffix validation. Keep serializer-owned Panng details in history/runtime validators.
- `rollout/src/recorder.rs::open_rollout_for_append`: insert header mode preflight before compression materialization and newline repair; native legacy/paginated path stays unchanged.
- `rollout/src/lib.rs`: export only a strict suffix reader required by thread-store; raw scanner stays private.
- Do not yet modify ThreadHistoryMode or advertise PanngV1 store capability. All marked sessions remain fail-closed on native construction. R1 tests prove no mutation for rejected marked partial tails, every partial-record cut, malformed complete records, unknown versions and caps.

**R2 — same-writer identified append/reconcile + strict constructor**

- `recorder/strict.rs`, `_tests.rs`: single pending attempt, stable bytes/cut, exact reconciliation, cold repair and healthy state initialization.
- `recorder.rs`: small command variants and strict writer ownership state; dispatch native commands through explicit mode checks. Any native AddItems path must ACK failure, or reject at public API before enqueue using immutable recorder mode, because AddItems currently has no ACK.
- `recorder/synced.rs`: reuse existing file/directory sync functions and receipt/error types; expose narrowly within recorder as needed. Do not create another file descriptor writer/queue and do not clear the generic fence until successful reconciliation establishes a known cut.
- `rollout/src/lib.rs`: public opaque reconcile/result and strict-constructor types only where thread-store needs them.
- Thread-store later maps strict cut to HistoryPosition and stores capability PanngV1 only after recovery/UI initialization paths support it. Do not retrofit commit IDs into arbitrary legacy append_synced calls.

Pre-coding alignment required from parent: actual PanngCommit accessors, canonical payload serializer, runtime replay validator, snapshot detection, branch ID type, and header branch binding. Those are cross-agent API dependencies, not unresolved production behavior. Once names are known, R1 can be implemented without editing history/runtime files or changing their ownership.

## Targeted evidence checklist

- Existing native file + existing constructor retains byte behavior; marked partial tail is rejected with byte-for-byte unchanged file.
- Valid header + initial snapshot + deltas loads; all truncation offsets in last delta yield either prior valid state or complete latest state, never half-state. Header/snapshot truncation cannot start a session.
- Interior complete corruption, unknown version, conflicting ID, predecessor gap, wrong branch, wrong ordinal and oversized line fail; no skip/fallback.
- Exactly 32 MiB/4096 boundaries and checkpoint near bound; no over-allocation or unlimited reverse scan.
- Real locked recorder: partial write then reconcile truncates+syncs and returns Absent; full write/sync failure then reconcile re-syncs and returns the same Durable cut; same ID/different bytes rejects; extra native bytes produce corruption.
- ACK receiver dropped then identical retry appends no extra line. Different commit during uncertainty is rejected. Factory construction count stays zero until Durable confirmation.
- Repeated crashes during tail truncation, file sync and directory sync remain fenced; restart validates before modifying anything.
- Cold resume fences execution epoch rather than treating recovered Prepared as safe to replay.

No full-workspace tests; all compilation is scheduled by the parent agent.
