# Panng 宿主接入最小路径与验证地图

日期：2026-09-28。工作区：`/Users/wpzhang/.codex/worktrees/panng-prototype/Codex-Panng`。本文件来自实际调用链的只读定位；没有改源码或运行测试。R2实现仍由其他agent负责，本文不依赖其半成品代码。

## 建议的下一纵向交付

**下一阶段不要再只交付孤立存储API：完成“真实LocalThreadStore加载Panng → Session启动前恢复完整宿主视图与runtime → 一次受控工具结果/模型继续 → 同一store恢复与UI分页一致”的纵向路径。** 初期仍由测试宿主开启，native生产入口继续拒绝未完成能力；该测试应调用真实Session和真实LocalThreadStore，不只测试字符串codec。

canonical authority仍是outer PanngCommit；模型历史、初始化设置、UI和runtime分别消费同一记录的不同视图。不能把所有inner effects提前flatten进普通Vec后丢弃outer身份/sequence/runtime，也不能把checkpoint.replacement_history再逐项当新append重复播放。

## 一、global RolloutItem 与专属parser的取舍

当前 `StoredThreadHistory.items`、`StoredModelContext.items`（thread-store/types.rs:173–190）、`ResumedHistory.history`、`InitialHistory::Forked`（history/lib.rs:358–370）、`PreparedFork.model_context` 都以 `Vec<RolloutItem>` 携带恢复资料。app-server和core的resume/fork桥接也是直接复制这些字段。专属R1结果如果只把模型消息塞进items，pending/Ready/epoch/cut必然丢失。

建议在准备好下文host视图后，增加 **boxed `RolloutItem::PanngCommit` 和明确的 `ThreadHistoryMode::PanngV1`**，让既有载体保留完整outer记录。继续用R1/R2专属严格parser/writer处理marked文件；加入global variant不表示允许native tolerant loader/append接管。未支持消费者必须按engine/mode明确拒绝，不能新增一个通用 `_ => ignore` 即算兼容。

不建议同时再给StoredModelContext、StoredThreadHistory、ResumedHistory、PreparedFork及各调用options分别增加零散runtime sidecar字段：这会在多个copy/fork/remote路径形成第二套容易丢失的传递协议。若选择独立typed bundle，应一次替换整个恢复载体，而不是让`items`与可选sidecar独立流动；真实diff若明显优于global variant可以重新比较，但不能为了少一处enum改动把runtime隐藏在临时线程变量里。

必须保留两种不同的读取产品：

- **模型/runtime恢复：** 最新joint checkpoint + bounded suffix，调用R1语义验证。
- **UI历史重建：** 从canonical历史的已知投影位置正向读取，必要时全文件分批重建；不能仅拿最新模型checkpoint后缀重建全部UI，否则此前ItemCompleted会消失。checkpoint本身不伪造旧UI项。

## 二、共享host视图及依赖方向

在`codex-history`增加一个只读host replay视图：普通RolloutItem保持原样；Panng Delta产生有序append effects；Panng Snapshot产生一个显式checkpoint（Compacted + companions），不展开成一串新的模型消息。runtime只由core Panng adapter从outer commit恢复一次。

这些共享访问器必须服务**早期配置读取**，不是等到model history reconstruction才展开。可提供专门的latest-settings/token/runtime-version accessor，以及用于native reconstruction的受控host replay流/临时Vec。它们读取canonical资料但不能再次持久化。

UI projector在`codex-app-server-protocol`，thread-store已经依赖它；**不要让codex-history或codex-rollout反向依赖app-server-protocol。** 实际UI cardinality检查应由app-server-protocol的统一Panng projection helper提供，LocalThreadStore在host commit准入和rebuild时调用。R2 writer负责字节、身份和runtime transition，不自行复制UI规则。

## 三、精确调用点：从磁盘到Session初始化

| 文件 / 函数 | 当前行为 / 必改接入 | 要证明的行为 |
| --- | --- | --- |
| `history/src/lib.rs::RolloutItem`、`rollout_payload.rs::RolloutItemWire` | 增加boxed outer variant与双向wire转换；禁止嵌套由现有Panng codec校验 | 同一canonical payload往返保持，未知版本不回退Other |
| `protocol/src/protocol.rs::ThreadHistoryMode`、SessionMeta | 新mode及branch/version标记；类型承认不代表native可写 | mode保留到StoredThread/InitialHistory；不得改native header假装Panng |
| `rollout/src/recorder.rs::open_rollout_for_append` | native preflight仍明确拒绝Panng；严格constructor专用 | 即使enum认识新mode也不能走native补换行 |
| `thread-store/src/local/live_writer.rs::resume_thread`、`create_thread`、`LocalThreadStore::resume_thread` | 按mode创建native或strict recorder；strict冷开必须ResumeFence | 无legacy append旁路；没fence不启用新intent |
| `thread-store/src/local/model_context.rs::load_latest_model_context` (:35) | 当前先typed读SessionMeta，再对Paginated走native reverse scan；Panng改走strict恢复，保留outer suffix | thread ID与请求一致；checkpoint之前pending/Ready仍由runtime snapshot保留 |
| 同文件 `load_for_fork` (:81)、`scan_model_context_from_lineage_blocking` (:163) | source runtime version恢复和cut处理不能跳过outer；Panng不走Rejected=>continue | 冻结cut，不跟父live tail；非法cut拒绝 |
| `thread-store/src/local/read_thread.rs::read_thread` / `read_thread_by_rollout_path` / `attach_history_if_requested` | 新mode走对应分页display/model读取，禁止legacy full-history fallback | 显示metadata不掩盖canonical mode；include_history约束保持明确 |
| `thread-store/src/types.rs::StoredModelContext`、`StoredThreadHistory`、`PreparedFork` | items保留outer，不丢runtime；PreparedFork还保留真实outer cut | 不用展开后的inner数量替代物理ordinal或fork继承边界 |
| `app-server/src/request_processors/thread_processor.rs::load_resume_initial_history_from_stored_thread` (:4564) | 当前仅Paginated调用load_latest_model_context；加入Panng明确分支 | 实际thread/resume经过strict store，而不是legacy path |
| 同文件 fork流程 (:5035–5070) | latest_context先用于settings与multi-agent版本，必须看见checkpoint companions | 历史cut不会丢唯一的已应用配置/运行版本 |
| `core/src/thread_manager.rs::resume_thread_with_history`、`fork_prepared_thread` (:1488)、`stored_thread_to_initial_history` (:2444) | 传递outer记录，不新建只有host文本的ResumedHistory | 调用链到Session仍能找到runtime/engine身份 |
| `core/src/session/mod.rs::Session::spawn_internal` (:539，特别:751–795) | disabled plugins/collaboration mode在Session::new之前就读取history | 插件/tool catalog构造前应用最新设置；空集合/None不复活旧值 |
| `core/src/session/session.rs::Session::new` (:737，persistence:1004–1100) | 选择LiveThread create/resume，早期就建立新模式恢复状态 | 不在初始化旁路写native ThreadSettingsApplied；未fence拒绝任何effect |

加入enum后应逐项检查所有`match ThreadHistoryMode`。一个`uses_paginated_storage()`可表达共同ordinal/UI存储行为，但**不能**代替`uses_strict_panng_protocol()`；两者等同会让新mode掉进原生宽容reader/writer。

## 四、所有已定位的早期恢复消费者

| 文件 / 函数 | 隐藏在outer后会丢什么 | 最小接法与验证 |
| --- | --- | --- |
| `history/src/lib.rs::latest_disabled_plugin_ids` (:550) | thread-owned plugin清空/变更，错误回退旧TurnContext | 使用有序host视图；明确最新空值也是权威值 |
| 同文件 `session_cwd_from_items` / `InitialHistory::session_cwd` (:537/:396) | 最新cwd | checkpoint companions/delta都可见；caller override按既有优先级 |
| 同文件 `multi_agent_version_from_items`、`compaction_resume_metadata.rs::resume_multi_agent_version` | multi-agent选择及冷恢复版本 | 最新Compacted.resume_metadata和TurnContext由共享accessor解释 |
| `app-server/src/request_processors/persisted_resume_settings.rs::latest_persisted_resume_settings` (:14) | approval_policy、approvals_reviewer、active_permission_profile；嵌套fallback也可能读错 | 升级整个反向遍历及prior-prefix fallback；不能只改外层match |
| `core/src/session/mod.rs::spawn_internal` 的 collaboration mode scan (:769–793) | 协作模式/模型设置 | 共用latest thread settings访问器，不再手写顶层扫描 |
| `core/src/session/mod.rs::last_token_info_from_rollout` (:1811)、`last_token_usage_record_from_rollout` (:1818) | UI token计数、最近TokenUsageRecord/预算恢复 | 看见Delta TokenUsageRecord及checkpoint.latest_token_usage_record；不将缺字段当0 |
| `core/src/session/session.rs::Session::new` MCP恢复循环 (:1608–1625) | mcp_resource_origins及MCP事件归属 | checkpoint与suffix相关effects在初始化MCP runtime时重放；不能仅恢复模型文字 |
| 同文件 `ExecutedToolCalls::new(&features, &initial_history)` (:1649附近)，实际扫描在 `tools/executed_tool_calls/mcp_attribution.rs::new` (:49)；以及 `InitialHistory::get_event_msgs` (:1805调用，history/lib.rs:412实现) | 已持久化的工具来源checkpoint及初始化事件视图 | 让合法history-owned attribution可见，但继续忽略upstream禁止反序列化的warehouse-only字段；get_event_msgs不能因outer嵌套变成空历史，也不能从checkpoint伪造UI事件 |
| `core/src/session/mod.rs::record_initial_history` (:1544–1656附近) | first user turn、AgentStatus::Interrupted、effective settings checkpoint | 用host视图识别真实user/terminal；Panng初始化记录经统一commit，不写native事件 |
| `core/src/session/rollout_reconstruction.rs::reconstruct_history_from_rollout` (:169) | last_started_turn_id、previous settings、reference context、world-state baseline、window、retained/guardian context | 在adapter明确安装一次joint checkpoint，再按序播放suffix；复用原重建算法而非复制 |
| `core/src/session/mod.rs::apply_rollout_reconstruction` (:1690附近) | media preparation与metadata配对、窗口安装 | 保持一对一媒体准备；runtime snapshot同时由Panng adapter安装，不能被history replace清掉 |
| `core/src/thread_rollout_truncation.rs::initial_history_has_prior_user_turns` / `user_message_positions_in_rollout` / `fork_turn_positions_in_rollout` | turn边界、last-N fork | 使用host view定位语义边界，但实际cut映射回outer record；不按inner index切断commit |

关键区别：`Session::spawn_internal`先读取设置，随后调用`session/session.rs::Session::new`，最后才record/apply initial history。因此“在reconstruct_history_from_rollout里处理Panng”单点补丁必然太晚。

## 五、UI投影、ordinal与分页：一条commit最多一个visible item / 一个turn boundary

必须接的真实路径：

1. `app-server-protocol/src/protocol/thread_history_projection.rs::project_rollout_line` 当前只从TurnStarted/TurnComplete/TurnAborted/ItemCompleted产生变化，ResponseItem和Compacted本身不产生UI项。新增Panng helper必须复用这套规则，而非从模型消息再制造一份item。
2. `thread-store/src/local/thread_history_materialization.rs::read_projection_steps` 当前每个物理line生成一个`ProjectedRolloutLine`，unknown/malformed可能跳过。Panng模式必须strict reject，不推进projection cursor越过坏行；outer line的offset/ordinal保持原样。
3. `thread-store/src/local/thread_history.rs::apply_projection` 与 `apply_change_set` (:103/:288) 把变更及next byte/ordinal放在同一SQLite事务；多个inner effect仍只推进一次outer ordinal。
4. `thread-store/src/local/thread_history/segment_paging.rs::push_cursor_clause` (:455) 用严格`>`/`<`；`push_order_and_limit` (:476) 按ordinal排序。`read.rs`与`search.rs`也依赖同一排序键。A1不得新增同ordinal多个distinct visible items。

**准入验证位置：** 在`LocalThreadStore`未来`commit_panng`调用R2 writer之前，先运行统一projector并验证实际changes；cold materialization/rebuild也运行同一validator。这样非法UI payload不能先获得durable ACK再发现无法分页。存储primitive仍保持不依赖app-server的低层边界。

cardinality不是`host_effects.len() <= 1`。一个commit可以同时含runtime变化、一个ResponseItem、对应ItemCompleted及辅助model-invisible metadata；它们可以产生恰好一个visible item。应按稳定item identity归并同item变化并保留顺序，禁止两个不同item。turn boundary按实际投影/生命周期事件验证，不只数不同turn ID：TurnStarted与TurnComplete即使同turn也不能因Map按ID折叠而偷偷跨越两个边界。最保守首版允许一个boundary effect；未来如需多boundary应单独升级cut语义。

**必要用例：** page_size=1升/降序遍历；更新水位续读；多runtime结果合一bounded通知；非法两个ItemCompleted在写入前拒绝；snapshot不制造ContextCompacted/UI项；同item模型output和canonical ItemCompleted只显示一次；SQLite删除后严格重建得到同一UI序列和turn状态。

UI重建的全历史读取不能复用仅返回最新checkpoint后缀的R1接口；应按outer记录正向流式分批投影，保持每批内存边界。`read_projection_steps`当前会整段分配尚未投影bytes，Panng大文件重建应避免把全历史一次装入内存。

## 六、宿主写入与压缩：不能只接read端

读端贯通后下一小阶段必须形成真正的shared commit路径，以下已有函数是集中入口：

| 文件 / 函数 | 接入责任 |
| --- | --- |
| `core/src/session/mod.rs::record_prepared_conversation_items` (:3556) | 复用现有prepare/truncation/身份分配，Panng先生成candidate，durable后才安装live history/发布raw事件；native保持当前分支 |
| `core/src/session/inject.rs::record_annotated_conversation_items` (:112) | 保留原envelope metadata，经同一入口；不能在此另写一个journal |
| `core/src/hook_runtime.rs::record_additional_contexts` (:849) | 原hook直接写history必须进入同一commit序列，不能在等待工具结果时绕过 |
| `core/src/session/mod.rs::send_event` / raw event持久化 (:2553–2556)、`emit_turn_item_completed` (:2601) | 持久UI事件与相应history/runtime按同commit准备，避免先history commit再一个native EventMsg；transient事件明确分类 |
| `record_step_world_state_if_changed` (:3679)、`record_context_updates_and_set_reference_context_item` (:4644) | WorldState/TurnContext追加同样经commit，不因它们model-invisible而绕过 |
| `record_observed_response_completed` / `record_token_usage_info` (:4736/:4770) | TokenUsageRecord有序持久化，预算和恢复读取使用同一事实 |
| `replace_compacted_history` (:4071) | snapshot c、安装s后缀、runtime@s及companions共同提交；ACK后安装，禁止内存先替换再persist |
| `compact.rs`、`compact_remote_v2_attempt.rs::run_remote_compact_v2_attempt`、`compact_remote_v2.rs` | snapshot之前取得cut，异步推理不持state锁；结果回到同一installer，不丢cut后suffix |
| `compact_token_budget.rs::run_compact_task_inner`、`Session::start_new_context_window` (:4571) | 非总结式window reset也须明确同样的runtime保留/后缀策略，不能只修remote compaction |

`persist_rollout_items`是底层集中点但**不够早**：当前很多调用已经改了live state。只在它里面包PanngCommit仍违反publish-after-ACK。建议在fork自有`core/src/panng/host_commit.rs`提取prepare/commit/publish，原有大模块仅加薄路由；每个绕过入口按测试纳管，不复制整个run_turn。

新Session恢复后的首次effective ThreadSettingsApplied也要走这里；否则strict recorder一启用便会拒绝初始化阶段的native写入，根本到不了首个工具。

## 七、fork/rollback 与pending不丢失

- `thread-store/src/local/paginated_fork.rs::prepare` (:15)、`history_base_at_boundary` (:87) 通过UI位置获取outer cut；不能向前/后偷包半条commit。R2 native persist路径不适用于strict writer，先用其明确的strict确认接口取得cut。
- `local/model_context.rs::load_for_fork` 根据冻结cut恢复runtime，然后才做last-N模型context裁剪。Prepared在子线程无执行权，Ready保留，InContext对应通知不能重复追加。
- `core/src/thread_manager.rs::fork_prepared_thread` 目前用`prepared.model_context.len()`记录inherited count；Panng需使用outer record身份/count，不用flatten后的item数量。
- `core/src/thread_rollout_truncation.rs::truncate_rollout_to_last_n_fork_turns` (:266) 不能直接对outer Vec做旧式user-message切片而丢runtime来源；先恢复，再派生子snapshot。
- `thread-store/src/local/revert_thread.rs::revert` / `create_replacement_recorder` 不能原地截断有执行权的Panng线程。按已定方案派生新branch，保留不可撤销副作用事实；未接好时明确拒绝，不回退native。

最小pending验收：O在checkpoint前Prepared、checkpoint后仍未完成；冷恢复通过ResumeFence转为typed unknown并保留通知事实，不静默忽略或再执行。另测Ready未发布、InContext已发布、fork前后Ready、父cut后结果不入子线程。此验收必须经过真实store→thread manager→Session，不只RuntimeSnapshot::apply单测。

## 八、最小可审核落地顺序

| 阶段 | 真正交付 | 不能用什么替代 |
| --- | --- | --- |
| H0：host视图和projector | history只读视图/早期accessor；app-server协议层Panng projector+cardinality validator，直接消费现有codec | 不能再只添加未被宿主消费的数据类型 |
| H1：门控格式与store桥 | global boxed variant+mode；LocalThreadStore strict加载/写入路由，保留outer；不支持路径明确拒绝 | 不能给enum新分支返回空Vec/忽略 |
| H2：真实Session恢复纵向测试 | 同一LocalThreadStore创建/加载marked fixture，app-server/core早期settings/MCP/token/window/runtime全部一致，pending保留 | 不能只调用R1后比较JSON文本 |
| H3：提交与UI闭环 | 真实Session通过host commit写一个受控工具结果，mock模型下一请求确实看到它；reload和page_size=1 UI一致；sync失败factory=0 | 不能只在fake store里证明ACK顺序 |
| H4：并发压缩/终止/fork | 工具/hook跨cut后缀不丢；cold fence、Ready交付、全出口finalizer、fork outer cut完整 | 不能只修happy-path恢复 |
| H5：真实有限工具与入口 | finite Shell/MCP走现有权限/hooks，实际入口启用，相关客户端及性能对比 | 前阶段仍保持测试宿主门控，不宣称正式使用 |

H0可在不新增global variant时直接测试现有PanngCommit；H1加入variant时，H0 helper已可接，不需要空match分支过渡。H1/H2可以拆成若干可构建提交，但合在同一纵向交付验收，避免长期停在另一个孤立存储API。每次复杂diff控制为可审查的小阶段；必要机械enum接线与真正语义修改分开提交。

父agent统一安排定向测试：history/runtime、thread-store的model_context/materialization/fork、app-server-protocol projector、core的resume/compaction/hooks/agent集成，以及必要app-server实际thread/resume测试。只有UI行为变更触及TUI时补对应snapshot；不再运行无关全工作区。

## 明确的停止条件

本地图不是新增一轮研究的终点。H2/H3应产出可运行的真实宿主证据：从canonical checkpoint恢复并继续一次受控工作，内存/model request/disk/UI四者一致。若连续提交仍只测试独立codec或writer，说明尚未按此最小路径接入；应停止扩张存储抽象，优先完成store→Session→model→store这条链。
