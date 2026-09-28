# A1a.1 单记录同步验证

日期：2026-09-28；基线 41f9084b30，panng/prototype 工作区。

## 本阶段交付

真实 RolloutRecorder → LocalThreadStore → LiveThread 已提供显式单记录同步 API。当前支持 Unix、持有已有独占 writer guard、paginated local thread。普通 store 默认为 Unsupported，legacy 明确拒绝，不伪造 ordinal。

一次队列命令按先前写入顺序追加一条 canonical record，等待文件 sync_all 及首次目录同步后返回真实 ordinal/byte cut。投影至 SQLite 或 metadata 失败不会把 canonical ACK 改成未写入。写入结果不确定时 fence 后续写入；公开入口未持 guard 则在入队前拒绝。

## 审核与验证

- 独立审核发现并修复无 ownership guard 的公开入口和 deferred-empty 收尾绕过 fence 两项问题，详见 SYNC-PRIMITIVE-REVIEW.md。
- 首轮 402 项中 401 通过、1 个新增 fixture 失败：错误选择了 paginated policy 不保留的 UserMessage event。改为 ItemCompleted(UserMessage)，保留真实过滤规则。
- 修复及补充用例后，`just test -p codex-rollout -p codex-thread-store`：**405/405 通过**，0 skipped。日志 `/tmp/panng-sync-tests-fixed.log`。
- 覆盖文件/目录 sync 前后故障、部分写入、不确定状态阻止两类后续写入、创建失败后的 flush/shutdown、ACK 丢失、关闭通道、无 guard 拒绝、真实本地 cut/reopen、投影失败、类型化错误穿透、unsupported 和 legacy 拒绝。
- sync 故障测试验证控制流与实际 sync 调用顺序；没有模拟真实断电，也不以 reopen 能读作为物理持久性的单独证据。
- core/app-server 调用方的穷举错误匹配已更新。35 项定向宿主回归首次 34 通过、1 项并行耗时阈值失败（另有 4 flaky）；清理已完成全量构建的增量缓存、恢复磁盘空间后，串行复跑并行工具/附件相关 9 项全部通过。保留首次失败，不把串行通过描述为无条件稳定。
- 原始日志已保存至 `research/validation/a1a1/`；包括首次失败与修正后/串行复跑结果。

## 尚不提供的能力

这不是 Panng 提交协议：没有稳定 commit ID、幂等 reconcile、严格重开、runtime/history 联合 checkpoint 或真实工具分发。fence 目前属于当前 writer；重开普通 recorder 不能当作解除不确定状态的恢复策略。失败/取消后不得盲目重试原 item。

API 的 writer guard 必须按现有调用方契约保护对应 rollout 的整个生命周期，不防御故意传错 guard、重复使用同一 guard 建立多个 writer 或不遵守锁的外部文件修改。LocalThreadStore 负责保证该生命周期与写入顺序。

Windows 尚无此同步实现；生产 Panng 入口仍不存在。下一阶段先完成 marked format、strict reopen、commit identity/reconcile 和联合恢复，再考虑真实工具及正式可用结论。

## 整理结果

定向 Clippy 与 `just fmt` 均通过。最后的 lint 清理将队列 payload 装箱并把私有 stop fence 的 panic 改为类型化错误返回；Clippy 编译验证通过，按仓库要求不在最终 fix/format 后重复测试。两处上游已有测试导入的自动删除已撤回。

代码已拆为五个独立本地提交：f4c2a3bb06（runtime，591 行）、c369d1d2fe（recorder，549 行）、9dca08a3f0（store，476 行）、def4d2c411（核心原型与集成测试，698 行）、4ffe70f4bf（故障回归，394 行）。每个提交小于 800 行；未推送远端。
