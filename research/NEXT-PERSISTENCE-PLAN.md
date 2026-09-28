# A1a.2 执行与验证方案

基于 9bf3d32764；A1a.1 已有真实同步 ACK，但没有提交身份和恢复核对，本阶段不改变正式可用门槛。

1. 将已审核的数据契约实现为有界、版本化 runtime journal 类型：不可变操作来源、revision、Prepared→Ready→InContext、快照和 epoch。构造及反序列化均校验；无任意 JSON 状态或执行端依赖。
2. 在 history 实现 PanngCommit 编解码及受限宿主 effects/checkpoint。先落地 codec 与真实恢复投影入口，再接明确的 Panng history mode；不能把 runtime 序列塞入用户消息。禁止嵌套 commit，保留完整 envelope provenance。
3. 严格 reader/writer 核对稳定 commit ID、内容与序列，区分 complete corrupt record 与 incomplete tail。持锁修复只截断未完成尾部；完整坏记录拒绝。确认丢失只能重新核对/同步既有记录，不追加第二份意图。每次至多一个未决提交。
4. 明确逐阶段 fail-closed：没有联合恢复支持的宿主入口拒绝 marked mode，不得交给 native 宽容 parser 续跑。既有 native 路径保持当前语义。

验证：数据往返与越界/未知版本、同身份不同内容、revision/来源变更、状态转移和跨 epoch；真实文件每个截断边界、完整损坏/序列缺口、ACK 丢失/重新同步、重开拒绝旧 reader；checkpoint metadata 浮点往返及 page_size=1 不漏项。只跑 runtime/history/rollout/thread-store 及必要宿主回归。

实施由自有 runtime 数据层、history adapter、严格恢复三部分分别拥有；依赖变更统一更新 Cargo/Bazel lock。独立审核先检查上述边界并在发现阻断时立即反馈，随后复核实际实现。未实现的恢复/投影不以空分支伪装支持。

## 本轮审核处置

- 当前epoch不删除已保留操作身份；Snapshot按reason检查，Fork/ResumeFence结构上不得含Prepared。正式长任务仍需后续背压/静止epoch轮换或独立tombstone ledger。
- codec使用规范化payload，拒绝上下游serde会丢失/重解释的字段；不绕过warehouse-only字段的skip_deserializing。宿主稍后显式准备可恢复内容，不由codec静默删字段。
- 新格式checkpoint必须有本线程settings、完整world baseline（若存在）、宿主接受的UUIDv7窗口ID；Fork-only parent cut。
- R1先以专属strict parser接入，未向原生RolloutItem读者宣称新格式支持。新header绑定panng_v1 + panng.version/branch + thread id。
- reader独立验证header后的首checkpoint，再读取最新checkpoint+suffix；只读cut使用独立PanngReadCut，不能冒充fsync ACK。身份唯一性验证限定root+本次suffix，checkpoint前缀不是整段重新审计。
- native preflight使用流式忽略大字段，不能用新格式header限额破坏已有native文件。原始完整坏记录/超限拒绝，只有物理EOF的未完成片段可报告为torn；R1不修复磁盘。
