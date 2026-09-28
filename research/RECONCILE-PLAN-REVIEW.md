# R2 strict reopen / append / reconcile：计划对抗审核

日期：2026-09-28。工作区：`/Users/wpzhang/.codex/worktrees/panng-prototype/Codex-Panng`。审核 `STRICT-REOPEN-PLAN.md` 的 R2 部分，对照当前 RolloutRecorder 队列、single-record sync 原语、R1 reader 和 writer guard。只写报告，不修改源码/计划，不运行测试。

## Verdict

**Request changes：R2 的 exact-attempt reconciliation 方向正确，但需要先补 writer-owned runtime candidate 与 cold NeedsResumeFence 状态。** guard 身份验证也应在允许 truncate 的本阶段落实。其余基础设施可以复用现有 queue/handle，不需要另一套 writer 或全局事务框架。

本轮范围应为：专属 strict constructor、同一 writer 内的有身份提交/核对、明确拒绝 native 旁路。它不自动完成完整 host publication、生产Panng能力或真实工具恢复执行。R1已有只读结果不能被转换成旧intent的执行许可。

## P1-1：Healthy/PreparedAppend 缺少运行时前态和候选态，序列正确不等于状态迁移合法

计划第 40–67 行的状态只存 branch、sequence、ordinal、offset、last payload与counters；没有current RuntimeSnapshot，PreparedAppend也没有candidate runtime。`append_panng`接收的PanngCommit只保证结构验证，不能代替prior→next规则。

真实现有语义：`panng-runtime/src/journal.rs::RuntimeMutation::apply`负责revision、不可变来源、logical call去重和snapshot reason约束；`history/src/panng.rs`明确要求writer另外验证prior transition。若R2只检查commit ID/bytes/sequence，调用者可以提交合法编码但非法revision、冲突来源或snapshot pruning，writer仍同步并ACK。

**选定修正建议：** HealthyState持有当前已提交RuntimeSnapshot。队列内处理Append时，先用current state执行mutation.apply，生成candidate；PreparedAppend保存该candidate和原始整行。只在同步成功/完整行reconcile成功后同时推进runtime、sequence、ordinal、cut和last；Absent回到prior，不消费sequence，不发布candidate。错误后不能先改runtime再试写文件。

不要只把validator交给外层调用者，因为多个已排队调用共享同一个writer前态；每次校验必须基于队列实际处理时的已提交state。Host完整恢复/UI cardinality仍是后续能力门，但runtime迁移合法性属于本阶段identified journal写入的责任。

最小验证：相同合法sequence但非法revision/descriptor/origin/快照变化拒绝且文件不变；后一个delta只有在前一个durable后才能基于其candidate通过；uncertain→Absent前后runtime整体相等；uncertain→Durable只应用一次。

## P1-2：冷重开不能直接进入Healthy；必须在writer层强制ResumeFence

计划第 97–101 行描述cold恢复需fence旧执行，但Healthy枚举/strict constructor/API没有强制这条规则。仅要求调用者记得先fence不够：它可能把恢复出的Prepared当成旧的未执行调用，或直接在旧epoch添加新intent。

此外，R1返回的是PanngReadCut，不是sync receipt。如果cold构造器把最后一条读到的完整记录放入 `last: CommittedAppend { receipt: SyncedRecord }`，对旧commit的相同重试就可能返回一个无同步证据的Durable；即使重新sync了那条记录，也不能证明旧factory未曾运行。

**最小状态修正：** 增加 `NeedsResumeFence { recovered, cut, ... }`。严格打开并验证/必要修复后停在此状态，last retry cache为空；只接受与recovered state匹配的新epoch ResumeFence snapshot。经过真实同步ACK后，进入Healthy并令last为该fence。任何旧intent retry、新intent、普通delta在fence前都明确拒绝；旧readcut只能用于恢复诊断/核对。

同一存活writer的ACK丢失与cold reopen必须区分：前者拥有原始PreparedAppend和“尚未返回许可”的调用上下文，可以核对后返回Durable；后者没有旧执行是否发生的证明，先fence并将旧Prepared保守归档，不重跑。两条路径不应共用一个模糊的“恢复成功即可append”状态。

最小验证：含Prepared的完整文件cold打开后尝试旧intent/新intent均拒绝；正确ResumeFence durable后新epoch可准入；fence本身ACK丢失可同writer重试一次确认；进程再次重启仍需新fence，不能复用旧epoch。

## P1-3：将guard身份检查推迟为“防恶意调用者”不适合会truncate的接口

计划第 101 行要求验证on-disk thread identity与expected branch，却只给constructor传expected_branch；guard与thread/Codex home匹配依赖既有调用约定，并将identity accessor列为以后hardening。

当前 `rollout/src/writer_lock.rs:26–30` 的guard只暴露不透明类型，实际锁来自coordinator home与acquire(thread_id)；`recorder.rs::new_with_writer_lock`保留guard但未核对路径对应身份。上一阶段要求“存在guard”已封住无锁公共入口，本阶段不能因为guard存在就对任意文件truncate。普通调用错误传入线程B的guard、线程A的path/expected branch，就可能在A仍由别的writer工作时修复它。

**最小修正：** 在现有guard增加只读身份验证能力（稳定thread ID与锁namespace/Codex home），strict constructor把它与raw header.id、config home和expected branch一起核对，全部在任何文件变更前完成。复用原锁，不新建锁文件协议。若constructor由thread-store私有封装保证这一点，也应使不带身份验证的公共入口不可用。

不要求抵御故意绕过协作锁的外部进程；目标是让API的普通参数错误不能变成不受保护的尾部修复。拒绝fixture需断言错thread guard、错home guard、错branch、错header时字节不变。

## exact-attempt reconcile：可采用的部分与必须保留的细节

计划第 107–114 行的四分支正确：

| 检查结果 | 可以做什么 | 不能推导什么 |
| --- | --- | --- |
| target offset之后无字节 | 同步已确认的当前cut，返回Absent | 不能构造executor；这不是intent durable |
| 尾部是cached完整line的严格prefix | truncate到原start并sync，返回Absent | 不能补换行/补剩余line，也不能把任意torn数据当此次attempt |
| 尾部恰等于完整cached line | 重新同步既有line，推进一次并返回Durable | 不能追加第二份intent |
| 任意不匹配、额外字节或完整冲突record | Corrupt/fenced，保留证据 | 不能删除完整record或退回旧snapshot继续 |

以下细节应落入实现/测试：

- PreparedAppend必须在首次可能触盘前进入writer状态；包含原timestamp、ordinal、start offset和完整line，不能在错误后重新serialize一个timestamp再比较。
- 区分总文件长度小于start_offset（既有prefix异常）和等于start_offset（target缺席）；前者不能算Absent。
- sync失败后的异步文件buffer先flush才能检查tail；flush失败仍Uncertain，不能根据当前metadata长度贸然截断。
- candidate runtime与原始bytes一同保留，完整linereconcile只发布一次candidate；同步失败/ACK发送失败不能重复advance counter。
- Absent只恢复“可以重新提交”的资格，必须再次append并取得Durable后才允许未来宿主构造factory。返回Ok(Absent)不等于操作成功。
- Healthy.last只服务最近一次相同payload/ID的重试。对过期sequence显式stale拒绝；查不到旧ID不能推断它未曾存在。不要把这个常数内存缓存宣称为任意历史commit的幂等数据库。
- “同ID不同bytes”应明确指canonical payload bytes；live uncertain tail则比cached完整outer line。两个比较域不同，不能忽略outer timestamp/ordinal变化。

## 禁止native旁路必须覆盖命令矩阵

真实队列 `recorder.rs:1954–1980` 包括AddItems、AppendSynced、Persist、Flush、Shutdown、Discard；AddItems没有ACK，`record_canonical_items`返回enqueue成功。

计划已指出该问题，建议采用immutable RecorderMode（Native/Strict）在公共方法入队前拒绝不适用命令，同时writer loop也按mode分派。至少明确：

- strict recorder拒绝native AddItems、generic AppendSynced及任何调用native write_pending_with_recovery的路径；不能只挡一个公开方法。
- Persist/Flush不能在strict uncertainty下把泛化“成功”当作fence清除；strict实现应返回当前明确状态或拒绝。
- Shutdown在Uncertain时可以报告失败并保留writer等待reconcile，或显式关闭且保留uncertainty；不能内部调用native补换行/自动重试。Discard允许放弃本进程writer，但不承诺磁盘clean，下一次仍strict恢复。
- native recorder拒绝append_panng/reconcile_panng；不能把Panng line写入native header文件。

测试必须调用公开API验证可见错误和字节不变，不能只直接构造私有writer command证明日志被忽略。任何内部拒绝都不能让上游收到成功ACK。

## 对现有R1接线的修订提醒

计划正文还有早期示意：`RecoveredPanngSuffix.records: Vec<RolloutLine>`、`repaired_tail_bytes`、新增strict reader模块。当前已实现的是专属Panng parser、PanngReadCut和只读torn_tail_bytes；R2应复用它们，不重新创建第二套R1对象或读取逻辑。预算实际包含独立header和首checkpoint读取，文档也应沿用最新定义。

strict cold修复只支持自包含、未压缩的本地文件是可接受的当前范围；对compressed input应在materialize/rename前Unsupported，继续保留native压缩行为。缺header/初始snapshot、最新complete坏record、序列缺口或预算超限均拒绝，不回退native，不为了找到更旧可用snapshot而跳过最新坏记录。

## 最小实施顺序

1. 确认WriterMode、NeedsResumeFence、Healthy.runtime、PreparedAppend.candidate及guard身份的共享接口；先写准入拒绝表。
2. 接同一queue的identified append，严格prior→candidate验证；fault注入尚未写/partial/full-before-sync/after-sync/before-ACK。
3. 实现live exactbytes reconcile，各分支整体验证runtime/counters/cut/file；首字节到换行逐断点，错误期间不同commit始终拒绝。
4. 严格constructor复用R1，先身份验证和所有完整后缀验证，后可控truncate/sync；返回NeedsResumeFence。
5. 验证cold fence、native旁路公开API拒绝、ACK丢失与Absent不能构造factory。最后才由thread-store适配新的writer接口。

只运行rollout/runtime及必要thread-store定向验证。正式host publication、UI恢复、生产PanngV1开关和模拟真实任务性能/token比较仍是后续目标，不能由R2存储ACK替代。
