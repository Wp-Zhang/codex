# Panng A1 持久化契约独立复审

日期：2026-09-28。工作区：`/Users/wpzhang/.codex/worktrees/panng-prototype/Codex-Panng`。
被审版本：`PERSISTENCE-CONTRACT.md` Design revision 1（本报告行号以读取时版本为准）。对照本地 reader、SQLite/UI projection、Session 初始化与 compaction/reconstruction 源码。只写本报告；不修改契约或源码，不为本审核启动测试。此前启动的 A0 定向测试在审核期间结束：5 passed、2661 filtered；这不是持久化契约已实现或通过的证据。

## Verdict

**单条外层 PanngCommit 是可行的恢复原子单位，方向通过；revision 1 尚需修订才能冻结为生产 schema。** 当前 recorder/thread-store 的“单记录明确同步 ACK”原语可以独立继续实现，不必等待所有高层格式问题；不能因此开启真实工具。

两个阻断必须先明确：一条 commit 产生多个可见 item 时，现有 ordinal-only 分页会漏项；将 settings 封入 checkpoint 后，只在最终 model-history 重建处展开太晚，Session 初始化已经读取过它们。两者都有准确的现有调用路径。另有请求确认/调用去重、结果来源持久化等数据契约缺口，必须在真实执行启用前关闭。

对齐父 agent 后已选定分页处置：**A1 每条 outer commit 至多一个可见 item 变化，至多一个 turn 边界；按统一 projection 的实际结果验证，不按 effects Vec 长度推断。** 本报告记录该决策，不把决策当作代码已修复。

## 1. P1：多个内层 UI 项共用 outer ordinal 会破坏分页

契约位置：第 140–153、185 行。源码证据：

- `thread-store/src/local/thread_history.rs:288–295,430–470`：change set 中所有 item 都取得同一个 `rollout_ordinal` 和 `updated_at_ordinal`。
- `thread-store/src/local/thread_history/segment_paging.rs:267–288`：item anchor 最终只保留 rollout_ordinal，不保留 item_id 作为 tie-breaker。
- 同文件 `:455–471`：后续页面严格使用 `ordinal > cursor` 或 `< cursor`；`:476–485` 按该 ordinal 排序并 LIMIT。

反例：一个 commit 含 ItemCompleted(A)、ItemCompleted(B)，page_size=1。第一页返回 A 后，下一页按 `ordinal > A.ordinal` 查询，B 永远遗漏。原地依次执行 projector、把结果合并进一个 SQLite transaction 并不能解决。相同问题也影响更新水位分页及同 ordinal 多个 turn。

**选择较小而正确的修正：** A1 限制每个 commit 的统一投影至多一个可见 item 变化和一个 turn 边界，且不得跨不相容 turn 边界打包。多结果可以合成一条有界通知，或分成连续多个 commit；runtime 变化随各自通知原子提交。checkpoint 不制造新 UI 项。若某个宿主 effect 在投影时派生多项，仍必须被 validator 拒绝，不能绕过限制。

未来需要真正批量 UI 项时再升级复合 `(outer_ordinal, effect_index)` 游标、排序和 watermark。A1 不引入这组长期 rebase 负担。验证须包含 page_size=1 的创建/更新分页、SQLite 删除后重建、升序/降序及同条 item 更新。

## 2. P1：checkpoint 包含完整 settings，不等于所有消费者能在需要时读到

契约位置：第 118–128、151–153 行。将 WorldState、TurnContext、ThreadSettingsApplied 和完整 Compacted 放进同一外层记录，确实解决 companion 写一半的问题。但“core reconstruction adapter 负责展开”还不足够：

- `core/src/session/mod.rs:759–763`：初始化先调用 `history::latest_disabled_plugin_ids`；`:769–793` 在初始化时直接反向查顶层 ThreadSettingsApplied/TurnContext 获取 collaboration mode。
- `history/src/lib.rs:550–569`：disabled plugins 的扫描只识别顶层记录，并刻意区分最新空值和旧值，避免复活先前选择。
- `core/src/session/mod.rs:1818–1828`：token usage 恢复同样只识别顶层 TokenUsageRecord/Compacted。
- `thread-store/src/local/model_context.rs:100–123`：fork 的 early runtime-version 恢复在进入 core 前调用 `resume_multi_agent_version`。
- `history/src/compaction_resume_metadata.rs:35–51`：该 helper 当前只识别顶层 TurnContext/Compacted。
- `core/src/session/rollout_reconstruction.rs:191–218,232–255,292–323`：随后才处理上下文基线、窗口与 last_started_turn_id。

反例：最新联合 checkpoint 内已明确清空 disabled plugins/变更 collaboration mode，旧顶层记录含旧值。若 early scan 看不见嵌套数据，会读取旧值或默认值，然后创建错误的 runtime/tool catalog；后面的历史展开无法回溯纠正所有初始化副作用。

**选定修正：** 使用 history-owned 借用视图/访问器，且在 **Session 初始化前** 供 latest-settings、token-usage、multi-agent-version、window/baseline 等恢复查询共用。记录仍保留 outer envelope；访问器提供语义读取，不再往 canonical log 写第二份 companions，也不让 checkpoint.replacement_history 被当作新消息追加。

更小的实现边界是两个明确视图：一是 `HostReplayEffect` 用于上下文重建；二是 checkpoint/append 的只读恢复状态访问器，用于初始化查询。不要把所有 reader 都强制转换为展开后 Vec。必须让“最新权威值为 None/空集合”终止旧值回退；full WorldState 缺失也必须表达明确的无基线，而不是默默沿用旧 baseline。

验收对象应是完整恢复结果：settings、tool catalog 选择、previous_turn_settings、window、token usage、world baseline、last_started_turn_id 和模型输入；不能只比较 replacement_history 文本。至少覆盖有/无 TurnContext、配置清空、模型切换、ReplayBound 不递增窗口、compaction 前后追加。

## 3. 调用来源 P1 与请求确认范围：已选择较小 A1 语义，仍待实现验证

契约位置：第 40–84、87–89、132、213 行。`RuntimeSnapshot` 只有 operations，`OperationRecord` 只有外部 turn_id/call_id；没有模型 step/response identity，也没有 request attempt、输入水位、durably accepted response 状态。它虽然正确地把 InContext 与模型消费区分，却没有表示迭代计划要求分别持久化的请求阶段。

具体风险：provider 重试或恢复后出现同一个已接受调用；若 adapter 每次分配新 OperationId，仅以 UUID 不重复为准，会把同一调用当作新操作。反方向，若假设同一用户 turn 内 call_id 永不复用，可能把来自不同模型响应的合法调用错误合并。请求被取消/响应丢失后也没有明确输入确认水位，无法按所声明的消费规则回收数据。

**与父 agent 对齐后的选择：** A1 仅承诺 InContext 表示 canonical 通知已存在且可恢复，不承诺模型消费确认，也不按消费确认做回收。请求尝试和响应确认仅作为诊断观测，不构成 exactly-once 保证。ITERATION-PLAN 与协议需要同步采用这个范围；因此不要求 A1 增加具有消费事务语义的 request ledger。

调用来源去重仍是必须实现的 P1：OperationRecord 增加稳定 logical_step_id（同一次采样的 transport retry 不变）、call_id、可选 provider_response_id。重复准入键为 `(branch, execution_epoch, logical_step_id, call_id)`；同键不同 descriptor 明确拒绝，同键已有 intent 不得再 spawn；不同 logical step 可以合法复用 call_id。provider_response_id 用于关联，不替代稳定本地 step 身份。

这些是已选定设计，不是代码已经满足。至少测试重试同调用、同键不同参数、不同 step 复用 call_id、restart 后旧 intent 不重放。存储同步原语可继续独立实现。

## 4. P2（必须在延迟交付接入前补齐）：StoredResult 不能只说明字符串大小而不定义来源编码

契约位置：第 58–60、89、114–116 行。Ready 可以没有 host effects，只持久化 text 或 artifact。稍后生成 completion envelope 时，若只剩工具文本，哪些来源/metadata 应保留没有明确协议。

源码中 `core/src/tools/parallel.rs:113–121` 把工具调用及 delivered_assistant_message 信息附到原始结果 envelope；`core/src/session/turn.rs:2455–2465` 在正常历史写入前处理外部上下文属性并记录 annotated envelope。不能认为任意输出字符串足以恢复这些信息。

较小的修正：StoredResult 使用**有版本的宿主结果编码**，保留 post-hook 接受后的原始内容、来源及需要的 metadata；runtime 本身可以只保存受限 bytes/artifact reference 和 codec 名称，无须依赖 core。延迟渲染时由同一 adapter codec 解码。或第一版 Ready 与完整通知同 commit 直接成为 InContext，减少待渲染恢复状态；但仍要保留证据来源和非模型可见结果事实。

验证：在 Ready 已 durable、通知未生成时崩溃；恢复生成的 envelope 与无崩溃路径整体相等，权限/授权分类和外部来源状态一致。不要只比文本。伪造结构化内容不能改变内容来源。

## 5. ACK 丢失、重开与副作用：主规则正确，补两个实现约束

契约第 189–197 行已经区分 uninterrupted coordinator 与 restart：同一存活协调者能证明 factory 未调用时，reconcile 后可 dispatch；重启则 Prepared 变 indeterminate，不重放。这一设计成立，不应为了追求“恢复继续执行”放宽。`sync_all` 前后失败都不能通过重新 append 生成另一个执行权。

需要明确：

- **writer reopen 不是 coordinator restart。** writer 错误重开保留提交身份与原始 candidate；record 完整但未确认 sync 时，应同步并确认原记录，不能因为 pending 队列已空就返回 durable。对应实际入口是 `rollout/src/recorder.rs:1825` 附近的重试与 `:2016–2035` 重开路径。
- `reconcile_panng(commit_id)` 最好同时校验 expected sequence 和 canonical payload digest，或在 receipt 返回它们供协调者验证。查询到“同 ID”不能绕过协议要求的 same-ID/different-bytes 拒绝。限定 reconciliation 仅针对当前单一未决提交；若允许历史旧 ID 查询，bounded suffix 找不到必须返回“无法证明”，不能直接解释成 Absent 后重写。

建议 A1a 只允许 writer/协调者一个未决 commit。这样 last-commit 身份 + 当前有界 suffix 就足够，不需要新增通用幂等数据库。崩溃后先 strict load + ResumeFence，不能在确认旧 epoch 状态前进行任何 dispatch。

## 6. 旧 reader 拒绝与严格解码：方案可接受，需真实入口证据

契约第 199–207 行显式新增 `ThreadHistoryMode::PanngV1`，并默认隔离 CODEX_HOME，比 optional engine 字段可靠。源码 `rollout/src/recorder.rs:1223–1235` 对 session_meta 的未知 history_mode 明确报错；`:1121` 的加载路径在宽容解码前调用该检查。因此针对当前基线有源码依据。

保留三个边界：

1. 对旧基线的真实 resume/read/list 入口做 fixture 验证，不推广承诺到所有历史版本。
2. marked branch 的 direct import 必须先验证 header；不能只对正文里的 PanngCommit 加严格 codec，然后仍让未知 header 被其他通道跳过。
3. 更新后的 projection/materialization 也必须严格：`thread_history_materialization.rs:148–186` 当前会跳过 malformed/unknown line，不能保留这种行为再称 UI 与 canonical 状态一致。拒绝应阻止 projection cursor 越过出错行。

建议对新 mode 的读策略集中分派，而不是每个 reader 自行猜测遇到错误是否可跳过；native 宽容逻辑保持原样。迁移/压缩仅保持外层记录，不承担 runtime 转移，但也不能删除未知关键记录。

## 建议的最终窄接口

- 一个 `PanngCommit`，delta 或完整 joint snapshot；不嵌套、不增加第二份 runtime snapshot。
- 单未决提交的 `commit/reconcile`，默认 Unsupported，ACK 携带 ID/sequence/outer HistoryPosition，必要时增加 payload digest。
- 一个 host replay 视图 + 供初始化使用的只读恢复访问器；runtime 仅由 core adapter 恢复一次。
- A1 限制可见投影 cardinality，沿用现有 physical ordinal 和 cursor；不新增 subindex。
- 原有 Compacted 结构作为完整宿主快照载体，但只在显式 checkpoint adapter 中解释，不额外触发 compaction hooks/UI 事件，不把 replacement history 逐项再次追加。

这些限制足以保留单记录原子性的优点，同时避免一次引入通用事务语言、所有分页游标升级或副作用 exactly-once 服务。需要真实 checkpoint/companion 等价验证，不能用缩小接口作为省略状态的理由。

## 最小验证顺序

1. 单记录 durable ACK：写/flush/sync/目录 sync/ACK 故障；reopen 不重复 append；未 ACK 时 factory 构造数零。
2. codec/mode：未知版本、完整坏记录、截断尾、nested commit、超过投影 cardinality 明确拒绝；旧基线入口拒绝 marked fixture。
3. projection：page_size=1，创建和更新水位，SQLite 删除重建；每条 canonical item/terminal event 恰好一次。
4. 初始化恢复：只保留最新 joint snapshot + 有界 suffix，也得到完整 settings/world/window/token/runtime；清空值不复活旧值。
5. 压缩交错：c 后 hook/结果进入 s checkpoint，磁盘恢复与无崩溃路径整体相等，ReplayBound 无伪 compaction。
6. 请求/结果/调用去重：Ready→InContext 各崩溃窗口；请求取消/未知；同已接受调用再次出现；fork 不继承执行权，Ready 不丢失。

以上通过并完成生命周期集成前，不把“writer 已同步”解读成“可以正式执行真实工具”。
