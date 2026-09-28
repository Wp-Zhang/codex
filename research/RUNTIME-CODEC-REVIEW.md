# Runtime / history codec review

2026-09-28。只读分析后的结论；未运行测试。范围为 `history/src/panng.rs`、`panng-runtime/src/journal*.rs`，并核对 recovery 的 checkpoint 接入方式。

## 两项可复现缺陷

### R1：自包含 checkpoint 接受不可达 revision

`RuntimeSnapshot::try_from` 遍历 `(revision, operation)` 时忽略 revision。`RuntimeSnapshot::new(epoch, [(99, Prepared)])`、`[(0, Ready)]`、`[(0, InContext)]` 均满足现有检查。包装为 Compaction/ReplayBound checkpoint 后 serde 也接受。恢复以最新 snapshot 为自包含 anchor，不回读其前缀，所以不能依靠 `apply(prior)` 拦截。之后 delta 使用这份虚构 revision 作为 expected_revision 仍可继续。

当前状态图严格为创建 Prepared@0 → Ready@1 → InContext@2；Fork 把 Prepared@0 转为 InheritedUnknown@1，ResumeFence 把 Prepared@0 转为 Ready/Indeterminate@1；重复 fork/resume 保留终态 revision。最小修复是在 snapshot 构造/Deserialize 共享边界按状态检查这个可达关系。无需回读旧日志。若以后增加合法同状态更新，应同时版本化修改状态图与验证规则。

### R2：交付引用没有绑定外层 commit

`ready.in_context(commit_A)` 包装进 `PanngCommitData { commit_id: commit_B, mutation: Delta(...), ... }`，现有外层构造与 runtime `apply` 均接受。B 可以持久化声称结果已由不存在的 A 交付。源码中 history/rollout 尚无 `delivery_commit` 关联检查。

最小修复是外层 `PanngCommit::new` 对 Delta 中 InContext 记录强制 `delivery_commit == data.commit_id`。Snapshot 中已交付状态的旧 commit 引用必须保留，尤其 Fork/ResumeFence 不能误改为当前 commit。这个修复只证明交付引用指向承载状态变更的记录，不证明 host effects 已包含正确通知；后者仍是 host coordinator 的独立验收项，不应通过要求任意非空 host effects 来伪造保证。

## 其余核查结果

未发现通过现有 Delta `apply` 改写不可变 provenance/descriptor 或跳过 expected_revision 的路径；对已知结果的 Ready→InContext 要求 result identity、outcome、payload 相等。Fork/ResumeFence 保留 Ready/InContext，且子分支可交付继承 Ready；未发现这些合法结果被丢弃。Fork/ResumeFence 独立拒绝 Prepared 的前一轮修复有效。

有界字符串、delta/snapshot 数量与编码大小检查都在已检查路径上存在；history `from_slice` 有原始字节上限，普通 Deserialize 仍需要其调用方提供输入 framing 限制，不额外宣称任意 deserializer 在分配前有全局上限。本次未发现绕过限制后被成功接纳的具体超限 payload。窗口 UUIDv7、fork parent_cut 与 snapshot/append 排他关系未发现新的具体反例。

## 处置

父 agent 已授权修复 R1/R2，并要求保持现有存储/执行边界。本报告保留发现记录；对应回归和统一测试结果由实现阶段追加证据，不把静态检查称为测试通过。

### 已实施修复，待统一验证

R1 已在 RuntimeSnapshot 构造及 Deserialize 共用边界校验 Prepared@0、Ready@1、InContext@2、InheritedUnknown@1。新增构造/serde 不可达 revision 回归，并验证 Fork/ResumeFence 保留 Ready 和 InContext 的记录及 revision。

R2 已在 PanngCommit::new 仅对 Delta 检查 InContext.delivery_commit 等于外层 commit_id。新增不匹配外层 ID 的构造/serde 拒绝，以及 Fork/ResumeFence snapshot 保留历史 delivery_commit 的往返用例。未添加“任意非空 effect 即交付完成”的弱校验；通知内容绑定仍由后续宿主协调器负责。

本阶段未执行测试、格式化或提交。
