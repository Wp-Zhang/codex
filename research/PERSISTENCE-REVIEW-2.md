# A1a.2 提交协议执行方案：第二轮对抗审核

日期：2026-09-28。工作区：`/Users/wpzhang/.codex/worktrees/panng-prototype/Codex-Panng`。审核 `NEXT-PERSISTENCE-PLAN.md`（基于 `9bf3d32764`）及最新 `PERSISTENCE-CONTRACT.md`，对照现有 scanner、model-context reader、recorder 重开路径。只写本报告，不修改计划或源码，不运行测试。计划中的新 codec/恢复实现尚不能作为已完成证据。

## Verdict

**初审 Request changes；下列三项已完成设计裁决，可按修订方案实施，关闭设计层阻断，保留实现/验证门槛。** 本轮确实推进提交身份/恢复协议，没有把用户最终目标缩小为单独 codec。发现及原始失败序列保留在后文，不能将接受设计误报为代码已满足。

现有 Unix 单记录同步原语是可复用基础，不应因新增协议再实现第二套 writer。`SingleRecordSyncV1` 仍不等于完整 `PanngV1`；所有真实执行与正式入口继续保持未启用，直至联合恢复、宿主初始化及最终生命周期门槛通过。

## 设计处置（父 agent 已确认）

1. **P1-1：A1 不允许 current-epoch pruning。** snapshot 保留该 epoch 的操作/准入依据，容量 64 是当前 codec 的 fail-closed 安全界限。正式宿主仍须实现背压、可证明安全的静止 epoch 轮换或 tombstone ledger；不能把超过 64 次工具调用无法继续当成正式长任务支持。本轮不提前宣称解决吞吐/长期容量。
2. **P1-2：snapshot reason 采用严格 prior→next 验证。** Compaction/ReplayBound 不得删除必需状态或改写来源、revision、epoch；Initial/Fork/ResumeFence 分别走受限转移。bounded reader 仅验证自包含锚点及其后 suffix，不承诺 checkpoint 前完整日志已经重新检查。完整重放与锚点重放等价、非法快照迁移拒绝仍必须由代码和测试证明。
3. **P1-3：strict reader 超限必须报错。** 不复用现有 silently-skip 语义；mode 分流、未换行尾截断与完整坏行拒绝必须保持区分，尤其不能先走 native 补换行再判断。该决定尚待 reader/writer 实现验收。

这些处置不降低用户的最终正式可用目标，只确定本轮安全且可审查的实现边界。

## P1-1：snapshot 的 operations 列表不足以同时允许 pruning 和保留当前 epoch 的去重身份

契约的 RuntimeSnapshot 只含 execution_epoch 与最多 64 个 OperationRecord；同时允许已通知/已被摘要覆盖的 terminal records 在 joint snapshot 中 pruning，又要求当前 epoch 的去重身份保留到 fenced/drained。两者之间没有持久字段。

失败序列：调用键 K=(origin_branch, epoch, logical_step_id, call_id) 创建 O1并完成 → O1 成为 InContext，在 ReplayBound/Compaction snapshot 中被删掉 → bounded resume 或状态重建只读这个 snapshot → 同逻辑 step 的重试再次出现 K，adapter 分配 O2 → 当前 operations 看不到 O1，允许第二次 intent/执行。改变 OperationId 不改变逻辑调用身份，因此只检查 OperationId/revision 无法阻止。

`rollout/src/model_context.rs:32–47` 的实际模式就是在最新 checkpoint 停止；`thread-store/src/local/model_context.rs:167–185` 会据此丢弃更早来源。这是使用有界快照的正常行为，不能通过偷偷扫全日志补救。

**本阶段最小可选修正：** 禁止当前 execution epoch 的 terminal operation pruning。snapshot 必须完整保留它们，达到容量上限明确拒绝新 admission，不能静默清空。该选择只解决 A1a.2 正确性；真实任务常超过 64 次工具调用，最终正式使用前必须处理这个容量限制，而不是长期用过早容量错误缩小目标。

**长期推荐：** snapshot 增加独立、紧凑的准入去重状态（key→operation identity/descriptor digest），或引入持久的逻辑 step 封闭/退休协议，使退休 step 的任何新 admission 都被拒绝。不能只存可显示结果而丢掉去重依据。若 ledger 也有上限，明确 fence/retire 条件或 fail-closed，不能靠无限增长回避有界要求。

最小验证：完整 replay 与从 snapshot 起的 replay 对同一 K 得到相同准入结论；terminal pruning后同 K+同 descriptor 不重启，同 K+不同 descriptor 拒绝；不同 logical step 复用 call_id 合法；超过上限失败时内存和磁盘都不前移。

## P1-2：SnapshotReason 是标签，不是转移约束；需要按原因验证 prior→next

计划第 1 项承诺不可变来源/revision/跨 epoch 校验，但契约只列出了 `Initial / Compaction / ReplayBound / Fork / ResumeFence`，没有完整的 snapshot 转移规则。只验证快照内部唯一 ID、长度和 epoch 格式，不等于验证它没有删掉 Prepared/Ready、回退 revision 或改写来源。

建议固定以下规则，writer 在提交 candidate 前对旧状态验证，不能让 Snapshot 成为绕过 Delta 的通道：

| Snapshot 原因 | 必须约束 |
| --- | --- |
| Initial | 只允许新 branch 的首个 runtime sequence；previous_sequence=None；不得拿它覆盖已有日志 |
| Compaction / ReplayBound | 同 branch、同 execution epoch；保留所有仍必需的操作、不可变来源、Ready/InContext结果事实及准入ledger；revision不回退；只允许明确已获准的pruning；history改变不能篡改runtime事实 |
| ResumeFence | 同 branch、新 epoch；旧 Prepared保守变为未知结果，既有Ready/InContext事实保持；旧owner_epoch仅为来源，不给新执行授权；一次fence后不能继续接受旧epoch更新 |
| Fork | 新branch、新epoch且有冻结parent_cut；Prepared无执行权，Ready事实可保留；子快照不允许继续承认父operation为可执行任务 |

数据类型构造校验和有状态转移校验要分开。先对所有 OperationChange/snapshot 验证并构造 candidate，整体通过后才发布；同一 delta 中不能靠前一个 change 部分成功后再在后项出错留下半更新。

### 有界读取不能同时证明 checkpoint 以前的整个日志完整

从完整 snapshot N 开始，reader 可以验证 snapshot 自包含、与所读 header/branch一致，以及 N之后逐项 sequence/revision/epoch；它无法同时证明 N以前没有坏行或来源曾被偷偷改写，除非读取此前日志或有额外可信校验链。这不是必须扩大全量扫描的理由，而是应精确描述信任边界。

建议定义：**正常 writer只产生从已验证前态计算出的snapshot；bounded恢复将有效且自包含的snapshot作为锚点，只严格验证锚点与后缀。** 全日志审计/导入属于另一接口，不宣称每次 bounded resume 已重新验证全部历史。不得用“需要验证missing initial snapshot”又从最新snapshot止步的互相矛盾规则实现。

初始root存在性/branch身份需由header可核对的信息证明：最小方案是 BranchId 与 canonical thread ID建立固定映射；若二者独立，header必须保存对应branch identity/初始模式。不能只拿snapshot自己声明的branch与自己比对。祖先lineage和子线程必须按冻结cut解释，不能错误套用子header验证所有父记录。

最小验证：尝试以Compaction/ReplayBound删除Prepared、删除未投递Ready、改变descriptor/origin、重置revision/epoch，writer均拒绝；合法ResumeFence/Fork按规则通过；后缀缺序/分支不匹配拒绝。合法日志的完整重放与有界重放最终对象一致。不要以“手工改了完整snapshot却无法检测”测试要求没有承诺的防篡改能力。

## P1-3：不能直接复用原生 scanner 的“有界”选项及尾部补换行

源码有两个容易被误用的现成工具：

1. `rollout/src/reverse_jsonl_scanner.rs:65–69` 的 `with_max_record_bytes` 文档明确说 **skip** 大记录；`:101–113,120–129` 超限后清空buffer并继续扫描，没有给调用者Oversized错误。默认scanner又没有单行上限。给Panng打开这个选项，不会自动获得fail-closed读取。
2. `rollout/src/recorder.rs:2059–2071` 的 `ensure_rollout_is_newline_terminated` 会给未换行尾部直接补`\n`；它在现有`open_rollout_for_append`过程中被调用。若先走native Resume再做strict检查，torn tail已被改成完整坏行，原始截断判据被破坏。

失败场景：关键intent/result行大于reader cap被静默跳过，reader找到更早snapshot后继续；或崩溃半行先被native补换行，strict流程无法再区分安全截断和完整损坏，甚至沿用原生ordinal扫描跳过它。

**选定要求：** marked-mode分流必须发生在任何native修复写入之前。strict framing需明确报错Oversized/InvalidCompleteRecord/UnsupportedVersion，记录真实行起止与是否有终止换行，并在buffer扩张前实施单行、累计字节和记录数限制。不能仅在serde得到对象后检查大小。raw writer必须验证header mode后再决定使用strict或native reopen。

尾部判据精确到字节：最终未换行片段可在独占ownership下截回最后完整记录并sync；即使该片段恰好是合法JSON，也不能当已提交行。最终**已换行**但无效的JSON/关键record必须报错，不能因为位于EOF就截掉；完整内部坏行同样拒绝。未知版本属于完整不支持的数据，不是“可修复尾部”。

最小验证矩阵应包括：最后一个换行被截掉但JSON完整、最后一行完整JSON但错误version、完整坏JSON带换行、跨64KiB scanner chunk的超限行、有界后缀内坏行、native已有坏行行为不变。只枚举正常frame的截断位置不足以覆盖这些区别。

## 请求确认、结果来源与能力激活：沿用已裁决边界，不重新加码

- A1只承诺result facts和InContext通知恢复；不以HTTP发送成功声称模型消费，不按消费确认回收结果。新增logical_step_id必须跨同一次transport retry稳定，provider response ID仅为来源信息。该裁决合理，本审核不要求重新加入完整消费事务。
- `CodexResponseEnvelopeV1` 保存完整envelope/provenance的选择合理；解析与shape验证留宿主codec，runtime存opaque引用。需要固定schema/迁移fixture而不是未版本化上游serde随版本漂移。float往返断言应依据真实payload语义，不强求字节重排后与原文本完全相同；但**commit ID核对必须定义唯一比较方式**，例如保存实际canonical bytes digest，而不是重新序列化后偶然变化。
- 单未决commit限制使ACK丢失reconcile保持小而可实现。当前未决ID只能核对该commit对应sequence与内容；若API收到任意历史ID且bounded suffix找不到，不可回报Absent诱导重复append，应拒绝超出支持范围。
- `SingleRecordSyncV1`保持现有窄能力。新enum/codec被编译进仓库，不代表允许创建或续跑Panng会话。所有真实宿主入口尚未具备joint恢复时应拒绝marked mode，而非给新variant的match加空分支后默默继续。

计划第 2 项“先codec及真实恢复投影入口、再mode”可分阶段落地，但建议把**拒绝marked mode的gate**与格式接入同一可审查阶段，随后按完整链路测试显式启用。使用被测的synthetic marked fixture不等于生产功能开启。LocalThreadStore的最终PanngV1能力应只在codec、strict reopen/reconcile、checkpoint恢复及必要投影全部可用时返回。

## 最小实施顺序建议

1. 先书面定下snapshot转移表和pruning/去重策略；runtime worker与history/recovery worker共用这些规则，不能各自解释。
2. 实现有界validated类型与原子candidate transition；先测试来源/revision/duplicate key/snapshot等价，不接executor。
3. history外层commit codec与host视图，保留outer身份；实际projection cardinality及早期settings/token accessor同时验证，unsupported入口明确拒绝。
4. strict字节framing与mode分流，保证检查发生在native补换行前；将现有sync原语包成单未决commit/reconcile，不另造writer。
5. 最新joint snapshot + suffix的真实有界恢复、fork/ResumeFence、ACK丢失重开；最后才讨论真实工具接入。

本轮可接受成果是可审查、可故障注入的提交协议。真实工具、宿主全出口收尾和模拟真实任务效率/token对比仍是用户目标中的后续工作，不能以本轮通过替代正式可用结论。
