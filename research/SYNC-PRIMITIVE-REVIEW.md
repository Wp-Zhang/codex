# A1a.1 单记录同步原语：实际实现对抗审核

日期：2026-09-28。工作区：`/Users/wpzhang/.codex/worktrees/panng-prototype/Codex-Panng`。审核对象为当时已写完的 recorder/synced、ThreadStore/LiveThread/Local 接入及新增测试。行号以本轮读取版本为准，后续修正可能移动。

只读源码与 diff，只写本报告；没有运行测试、格式化或修改实现。测试是否通过由主 agent 的统一运行记录确认。

## Verdict

**首次审核：Request changes（1 项 P1、1 项 P2）。复核：两项已静态确认修复，定向测试待主 agent 统一验证；未发现阻止本小阶段继续的其他实现问题。** 接受范围仅为“Unix 上、拥有独占 writer 的单记录同步原语”。

实现正确地区分了 enqueue/flush 与 file sync，成功路径等待文件和目录同步，存储/投影失败分类也基本正确。这不是 Panng 提交协议：没有稳定 commit ID、幂等 reconcile、严格重开、runtime codec 或执行恢复。当前明确不启用生产调度，这些缺项不阻止本小阶段；后续不能绕过它们直接接真实工具。

## 修复复核 disposition（2026-09-28）

- **S1：静态修复确认。** `recorder/synced.rs:95–101` 在 enqueue 前拒绝 `_writer_lock.is_none()`；公开方法注释要求 guard 保护该 rollout 全生命周期。`public_api_requires_owned_writer_and_returns_synced_position` 使用真实 Resume 入口，验证无 guard 拒绝且文件不变、有 guard 获得 receipt、第二次获取同一 thread 锁失败。这封住了原报告中两个无锁 synced writer 的入口。沿用 `new_with_writer_lock` 对 guard 与 rollout 身份匹配的既有调用方契约，没有声称能防御故意传错 guard 或绕过协作锁的外部写入者。
- **S2：静态修复确认。** `recorder.rs:1814–1824` 的 flush/shutdown 现在在 deferred early return 前调用 ensure_writable。新增 `creation_failure_fences_empty_deferred_flush_and_shutdown` 通过真实目录创建失败保持 deferred+empty，并断言两个收尾调用都报错，覆盖原缺口。
- **测试状态：尚未由本 reviewer 运行或观察新测试结果。** 上述是源码与测试内容复核，不能写成新测试已经通过；主 agent 的统一验证记录决定是否通过本阶段。
- **小型测试覆盖提示：** 原 `lost_receipt_does_not_reexecute_and_closed_channel_rejects` 的 closed-channel 部分构造了无 guard recorder，现在会先被 ownership 检查拒绝，因而不再覆盖 send 到关闭 channel 的错误分支。建议给该部分提供真实 guard，或由有 guard 的 recorder shutdown/discard 后再调用公开方法；不影响当前生产分支正确性判断，但应避免沿用过强的测试名称/覆盖声明。

以下保留初次发现及其原始失败序列，便于审查修复缘由。

## S1 · P1：公开 recorder API 没有要求独占 writer，可返回包含其他未同步写入的 cut

位置：`rollout/src/recorder/synced.rs:88–99,165–176`，`rollout/src/recorder.rs:883–897,1015`。

`RolloutRecorder::new()` 通过 `new_inner(..., writer_lock=None)` 构造无 ownership guard 的 recorder；它与 `new_with_writer_lock()` 一样可以调用公开 `append_synced()`。新方法只检查队列/状态/平台，没有验证 `_writer_lock` 存在。它在 `sync_all()` 后取文件 `metadata().len()` 作为 receipt 的 end_byte_offset。

具体交错：两个无锁 recorder 分别 Resume 同一个文件，各自得到相同 next ordinal。A 写入并 sync；B 写入但未 sync；A 随后读取 metadata 长度，得到包含 B 的 end offset。A 返回的 cut 既可能覆盖尚未持久确认的 B，也有重复 ordinal。O_APPEND 只保护 append 偏移，不提供共享序列号或“sync 后获取的全文件长度属于该提交”的证明。

`LocalThreadStore` 的正常调用持有 cross-process WriterLockGuard 和 per-thread lock，不能证明这个公共低层入口也安全。当前单 writer 测试没有覆盖反例。

**最小修正：** 让公开 `append_synced()` 在未持有效 WriterLockGuard 时明确 DefinitelyRejected，或通过类型/可见性让此能力只出现在持 guard 的 recorder 上。不要为 synced API 改动全部 native recorder 构造语义。文件位置的读取必须发生在该 ownership 覆盖期间。

**最小验证：** 无 guard recorder 调用被拒绝且文件不变；有 guard recorder 正常 ACK；两个实例争用同一 thread ownership 时第二个无法获得 synced capability。若仍允许无 guard，则必须新增真实文件锁与序列号协调，复杂度更大，不推荐。

## S2 · P2：首次创建失败后的 fence 可被 flush/shutdown 的 early return 隐藏

位置：`rollout/src/recorder.rs:1814–1825,1902–1904`，`rollout/src/recorder/synced.rs:133–138`。

fence 的实际检查位于 `write_pending_once()`。但 flush/shutdown 在 `is_deferred() && pending_items.is_empty()` 时直接返回 Ok，未检查 fence。

具体场景：新 recorder 尚未物化，首次 append_synced 创建文件失败，例如 canonical path 被目录占用。append 返回 Uncertain 并设置 fenced，writer 仍 None、deferred 仍 true、pending_items 为空。接下来 flush 或 shutdown 返回 Ok，虽然之前 accepted command 的不确定状态没有消解。这不会让下一次 append_synced 绕过 fence，却会让通用收尾路径把失败隐藏为成功，并与“uncertain fences both paths”的契约不一致。

**最小修正：** flush/shutdown 在 deferred early return 前检查 ensure_writable。保留 Discard 作为显式放弃 writer 的路径。无需新增 reconciliation。

**最小验证：** 用真实首次创建失败，依次断言 append Uncertain、第二次 append DefinitelyRejected、flush/shutdown Err、文件不被后续普通 append 修改。现有 fault Stage 都在 writer 已打开之后，未覆盖这个分支。

## 已核实成立的部分

1. **不是假 fsync。** `synced.rs:162–174` 顺序为 write_all → flush → sync_all → 获取文件长度 → 首次 ancestor directory sync；任一步错误不返回成功 receipt。目录同步失败时保守地返回 Uncertain 是合理的，不能因为文件内容可能已 durable 就称确定未写。
2. **同一 writer 内的顺序清晰。** AppendSynced 进入现有 RolloutCmd 队列；`:144` 先处理此前 pending prefix，再写新项。`recorder.rs:1952–1965` 串行处理后续 native 写入，正常单 writer 下 ordinal 和 cut 顺序成立。
3. **不确定后不自动重写这一行。** `synced.rs:133–138` fences；`:143` 不把新项放入 native retry buffer；普通 `write_pending_once` 同样检查 fence。partial-write 注入后再追加不会在半条 JSON 后写新 synced 记录。
4. **ACK 丢失语义保守。** 公共方法 enqueue 失败为 DefinitelyRejected，等待结果通道断开为 Uncertain；writer 忽略 ACK receiver 消失，不重复执行写命令。`lost_receipt_does_not_reexecute...` 证明的是一条写命令落一行，不是 executor 或稳定 commit ID 的 exactly-once。
5. **Local/LiveThread 不以投影失败否定 canonical ACK。** local live_writer 返回前的 SQLite/path projection 失败只警告；LiveThread 的 metadata 更新失败也只警告。receipt 的 thread_id 使用 rollout_id，exclusive ordinal 进行 checked_add，不伪造 legacy cut。
6. **能力范围没有偷换。** 默认 store Unsupported；Local 仅 Unix 宣布 SingleRecordSyncV1，非 paginated thread 仍拒绝。这个 enum 注释明确不是完整 Panng 支持。Windows 未实现目录同步是当前显式限制；不能据此宣称跨平台生产可用。

## 下一阶段必须保留的边界，不列为本小阶段阻断

### Fence 目前仅存在于当前 writer 实例

新 Resume 会重新构造 SyncState::default；现有 `open_rollout_for_append` 在 `recorder.rs:2048` 调用 `ensure_rollout_is_newline_terminated`，`:2057–2067` 会给非换行尾部补换行；`ordinal.rs:67–72` 会跳过不能解码的记录。因而“关闭再 Resume”不是新协议的 reconcile，可能把 torn tail 变为一条完整坏记录后继续。

本阶段没有声称严格恢复，所以不要求现在实现整套 Panng loader。但 A1a.2 必须先切换 marked mode 的严格重开：保留/核对原 commit ID，合法截断尾按记录边界处理，完整坏记录拒绝，ordinal 与 canonical bytes 同步重建。不能直接通过重新调用 new_with_writer_lock 清除 fence 后继续新副作用。

### 单记录同步原语不提供重试幂等性

receipt 只有位置，无 commit ID。调用方超时或被取消后不得 retry 原 RolloutItem 以为幂等；可以得到第二行。本实现注释已说明这一点，属于合理的小阶段边界。后续 Panng commit/reconcile 必须以稳定 ID 和 exact payload/sequence 验证包住这个原语，不能只依赖“同一位置看起来存在”。

### Capability 表示实现能力，不代表所有文件系统已经 preflight 成功

cfg(unix) 不保证运行目录支持 fsync(directory)；当前实际失败会通过 Uncertain 而不是伪造成功，安全方向正确。后续启用模式要在无副作用阶段建立 initial durable checkpoint，不能把 capability enum 当作文件系统实测结果。同步全部 ancestors 到 root 比只同步新目录保守，但可能产生可用性/延迟成本；无需为本阶段性能优化扩大改动。

## 测试证据质量与建议补充

当前 tests 的有效覆盖：同步顺序观测、pending prefix、移动文件 cursor 后 O_APPEND cut、各阶段失败 fence、ACK receiver 丢失、平台拒绝、persistence policy 拒绝、Local cut 与文件实际内容、metadata/SQLite failure 不抹掉 ACK、Typed Uncertain 跨 LiveThread 保留。

需要准确表述其边界：Stage 在 sync 调用前后注入错误，并不模拟存储设备断电；read_to_string 重开证明内容可读，不单独证明物理持久。实际调用 sync_all 且不吞错误提供源码层面的同步依据，测试验证控制流。未运行本轮测试，因此不提供通过数。

除 S1/S2 回归外，优先增加一个**真实命令队列**顺序测试：AddItems(A) → AppendSynced(B) → AddItems(C) → barrier，B receipt 的 byte/ordinal cut 只到 B，整个文件顺序 A/B/C；现有 pending_prefix test 直接操作 state，不能单独覆盖队列接线。此项是验证增强，未找到当前队列实现违反该顺序的源码证据。

本轮只需 rollout/thread-store 的相关测试；无需 core 或工作区全量测试。修复 S1/S2 并通过定向验证后，可接受该原语阶段，继续 codec/reconcile/strict-reopen 工作。

## 主 agent 验证回填

- S1/S2 及 fixture 修正后，`just test -p codex-rollout -p codex-thread-store`：405/405 通过（/tmp/panng-sync-tests-fixed.log）。
- closed-channel 用例已补真实 guard，并验证拒绝来自关闭通道，避免只命中 ownership preflight。
- 首轮 402 项中 1 个新 fixture 误用 paginated policy 不保留的 UserMessage event，已改为 ItemCompleted(UserMessage)，未放宽 persistence policy。
- 同步原语阶段可以进入整理；这里的通过不替代下一阶段 strict reopen / commit ID / reconciliation / 真实 runtime 恢复验证。
