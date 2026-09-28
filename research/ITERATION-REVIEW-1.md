# 迭代方案第 1 轮对抗审核

日期：2026-09-28。审核工作区：`/Users/wpzhang/.codex/worktrees/panng-prototype/Codex-Panng`，目标分支 `panng/prototype`。审核对象：`research/ITERATION-PLAN.md`、`research/PROTOTYPE-RESULTS.md` 及当前原型源码。行号对应本次读取的工作区，后续并行实现可能使行号移动。

只新增本报告，没有修改方案或代码，没有运行构建或测试。已通过源码核对测试的控制方式；报告中的测试通过数引用原型结果记录，未独立重跑。两个宿主探针的成功意味着反例复现，不意味着修复。

## Verdict

**Request changes，然后进入缩小后的 A0/A1 实现。** 新方案明确了正式使用门槛，也纠正了“测试工具跑通即完成”的判断，目标没有偷换。但是迭代 A 仍将关键格式和执行顺序留作候选，并把它依赖的失败仲裁放在 B，容易先造出无法履行自身契约的持久化层。

建议直接选定：**独立纯状态机 + 宿主唯一提交协调者 + 显式版本化 runtime 提交记录 + 带 runtime 状态的联合 checkpoint。** 不再优先尝试把独立操作更新塞入 ResponseItem 的附属 metadata。metadata 可以承载来源/关联信息，不能承担全部执行日志。

当前最具体的阻断不是架构措辞：**工具执行在构造 future 时就可能启动，早于 Pool::submit。** 如果沿用这个接口把 fsync 加到 submit 内，仍会违反“先提交再执行”。

正式可用门槛总体可达，但必须锁定支持矩阵、定义请求交付确认、提前最小失败仲裁。只有两轮审核无 P1 不是独立证据；每轮必须对应固定补丁及未跳过的功能门槛。

## P1 阻断项及选定修正

### R1：提交边界在 executor future 构造之后，真实工具会抢跑

计划位置：第 35–40、49 行。当前源码：

- `codex-rs/core/src/stream_events_utils.rs:346–364`：先记录调用，再调用 `handle_tool_call` 构造 future，最后传给 Pool::submit。
- `codex-rs/core/src/tools/parallel.rs:79–98`：`handle_tool_call` 是返回 future 的普通函数，进入 `async move` 之前同步调用 `handle_tool_call_with_source`。
- 同文件 `:127–138,196–226`：后者也不是 async fn；在返回等待 future 之前执行 `tokio::spawn`，spawn 出来的任务可以立即获得锁并运行 handler。
- `codex-rs/core/src/panng/mod.rs:82–88`：submit 的参数已经是构造好的 `InFlightFuture`，在内部登记状态已经晚于上述启动点。

具体反例：构造真实 shell executor future → submit 开始 fsync，磁盘失败或进程崩溃 → shell 已写文件，但日志无已提交操作。即使从未 poll 返回的 future，spawn 已经发生。当前 probe 无外部副作用，因此这是**扩展到真实工具时的阻断**，不是否定已报告的 probe 结果。

**选定修正：** admission 接收未执行的 ToolCall/稳定意图，或只在 ACK 后调用的 dispatch factory；先分配 operation_id、commit_id 并持久提交，再调用现有工具 runtime。不能把“future 通常惰性”当作 API 契约，也不建议为此全局改变 native runtime 的启动时机。

**最小验证：** fake durable store 卡住/拒绝 ACK 时，handler 的 `started` gate 必须始终关闭；测试必须包含“构造但不 poll”情形。ACK 成功后只启动一次；同 commit ID 重试不能再构造第二个 executor。

### R2：持久化错误无法沿现有 void 记录路径阻止继续执行；最小 finalizer 必须前移到 A

计划位置：第 36、38、45 行；finalizer 却在第 53 行的 B。源码：

- `codex-rs/core/src/session/mod.rs:3533–3562` 的记录入口返回 `()`；`:3647–3666` 先更新 live history，再尝试 persist。
- 同文件 `:4509–4516` 仅返回 bool 并记录错误；无 live_thread 时返回 true。这不是 Panng 的 durable commit 成功。
- `codex-rs/core/src/session/inject.rs:112–137` 的 annotated 路径最终也进入相同函数。
- `codex-rs/core/src/panng/mod.rs:103–114` 在历史记录前就 remove pending、complete、deliver；`:129–135` stop 清空 futures/pending，未把已给模型的 running 回执归档为终态。
- `codex-rs/rollout/src/recorder.rs:1035–1046,1891–1899` 是 enqueue/flush，尚无所需 sync ACK。

失败序列：hook 或结果追加失败，但 live history 已对后续 sampling 可见 → 模型发出依赖该内容的新工具 → 下一次单独的 intent commit 成功 → 新副作用建立在未能恢复的上下文上。只给 intent 增加 fsync 不足以保障历史与执行一致。

**选定修正：** A 先建立宿主提交服务的 `Open / CommitUncertain / Failed / Closing` 最小控制状态。准备候选变更，持久确认后才发布到 live history/runtime 并允许后续 effect。错误不可确认时冻结新模型请求及新工具 admission；允许已有执行端报告事实，不能继续推进副作用。最少把存储故障、shutdown/强制 abort 的操作归档路径前移到 A，其余完整停止原因可留 B。失败通知本身不能依赖损坏日志成功才解除死锁。

对现有 `()` 调用者无需一开始把数百个签名全改成 Result；可在集中 record 层路由至提交服务并设置 fail-closed 状态，但**每个 sampling/dispatch admission 都必须检查该状态**，且不能在调用端吞错后继续运行新的 handler。需要列出绕过 record 的历史替换/注入入口并纳入同一门。

ACK 丢失必须区分“确定未提交”和“提交结果未知”。持久事件使用稳定 commit_id；原 writer 串行化同 ID 的查询/幂等确认，恢复解析也去重。同 ID 不得包含不同 payload。不能以新 commit_id 盲目重试并重启操作。

### R3：把日志格式留作“优先元数据，必要时扩展”会拖延 A 的核心裁决

计划位置：第 37、39–47 行。源码：

- `codex-rs/history/src/lib.rs:51–63`：CodexHarnessMetadata 明确从属于 ResponseItemEnvelope；`:201–217` 是当前全部 RolloutItem 类型。
- 同文件 `:275–295` 的 CompactedItem 没有 runtime snapshot。
- `codex-rs/rollout/src/model_context.rs:32–47` 在当前完整 Compacted 处停止读取。
- `codex-rs/core/src/session/mod.rs:3579–3600` 已说明持久化 batch 可以部分写入，fork 需要完整边界快照。把两个独立 record 放进 Vec 不是原子事务。

操作在模型休眠时完成、取消或失去执行权，不一定有新的模型 ResponseItem 可挂 metadata。修改旧 item 的 metadata 违反追加式日志；制造空模型消息只为携带状态则混淆模型输入。即便 snapshot metadata 可附在 checkpoint 上，独立状态更新仍需明确事件记录。

**选定修正：** 增加模型不可见的、版本化的 runtime commit 单位。可以是一条完整外层提交记录，或者由 writer/reader 共同识别的提交组；其中原子包含本次宿主历史投影与 runtime 变化。字段至少明确 engine/version、commit_id、前序/当前 commit_seq、thread/execution/history epoch、变更及联合 checkpoint。namespace 表示 payload 的解释域，不赋予其任意跨版本兼容能力。

推荐原型先用**单个自包含提交记录**证明原子读取；不要先实现多记录事务、后再处理 torn batch。若引入外层记录，需要单独定义物理 record ordinal 与展开后的模型 item index，分页和历史 cut fork 必须使用可还原映射，不能让 ID/ordinal 随重放改变。保持 native 记录行为不变；Panng reader 不得把不完整提交展开给任何消费者。

联合 checkpoint 在 s 提交，包含 `compressed(≤c) + original(c,s]`、runtime@s、模型交付状态及所有必需 companion 状态；读取停止条件是**完整联合 checkpoint**，不能因普通 Compacted 存在就停止。旧的读取探针是正确起点，但必须扩展到真实 local store 的分页、物理截断与重开。

Engine/version 及 store 能力判定必须在 A 写入首个操作前完成，不能到 C 才设计。对完全旧的外部 Codex 二进制，不应承诺未知格式必然拒绝；需限定支持入口或做存储隔离，避免旧 reader 静默跳过扩展后继续。

### R4：“已消费确认”尚无可实施定义；不能在固化 checkpoint 后再决定

计划位置：第 39–45 行。当前 `codex-rs/panng-runtime/src/machine.rs:12–18,118–126` 只有 Ready → Delivered，deliver 不关联模型请求；Pool 在 `core/src/panng/mod.rs:106–114` 写历史之前就标为 Delivered。该模型在纯原型可用，不能直接成为生产 schema。

失败序列：结果标为已消费/被压缩回收 → 模型请求发送后取消或响应丢失 → 恢复时既无结果待投递，也无可确认的响应。另一个方向是在每次未知请求后重新追加完成通知，破坏稳定前缀并重复交付。

**选定修正：** 至少拆分 `result durable`、`notification published to canonical history`、`request attempt includes through watermark`、`model response durably accepted`。同一完成消息有稳定 item ID，只向 canonical history 追加一次；模型重试可在同一前缀再次携带该项，不等于再产生一条通知。请求开始记录 attempt ID/输入水位，响应和其产生的调用被接受时提交确认；取消或未知结果不能伪造消费成功。实际模型是否“理解/利用结果”无法 exactly-once 证明，产品契约应是宿主可观测的请求/响应确认。

Ready 结果在 fork 保留，Pending 执行权变为 inherited/no-owner；Published 结果靠保留的历史项重建，不再发第二条。现有 `machine.rs:176–179` 会把 Ready 也改成 Inherited；计划已指出需修正，该修正必须跟交付 schema 一起完成。

### R5：native barrier 回退不是天然隔离；共享 gate 已影响未接管 code-mode

计划位置：第 17、58、71 行。源码：

- `codex-rs/core/src/tools/parallel.rs:59–60`：只要 turn 中有 Pool，所有 ToolCallRuntime 都共享 execution_gate，并不只对 probe 生效。
- `codex-rs/core/src/tools/code_mode/delegate.rs:118–127` 创建 nested runtime，也取得该 gate。
- `codex-rs/core/src/tools/code_mode/mod.rs:402–412` 将 nested 调用送回相同执行链。
- `CodeModeExecuteHandler` 未覆盖 `supports_parallel_tool_calls`，继承 `codex-rs/tools/src/tool_executor.rs:122` 的默认 false；执行时父调用持写锁，nested 又需同一锁。

这会产生父等待 nested、nested 等父锁的交错；code-mode 自己的 yield 可能暂时释放父锁，因此本审核不未经实验断言必然永久死锁，但不能把它称为已验证的 native fallback。

**选定修正：** A 的 admission 矩阵先明确拒绝 Panng + code-mode 组合，或保留完全独立且不会绕过全局互斥的已验证执行路径；不要只在工具白名单外标一句 barrier。B 的正式方案采用 **leaf invocation 获取执行 gate，协调型父调用不跨 nested await 持 leaf 锁**；code-mode cell 保留独立并发/取消约束。跨 step 的独占互斥语义在 leaf 层验证，不能简单给子调用换一把锁而允许它绕过其他独占工具。

同时在 A 的持久身份里区分 operation 与 call、parent operation/cell、job 与 resource、resource owner。完整资源执行可到 B，但 schema 不能继续仅以 call_id 为唯一执行身份。后台移交必须是 durable 的 owner 变更；在 child fork 里不能凭相同 process_id 获得父资源控制权。

## 正式可用范围及可留到后续的项目

计划的 9 项硬门槛应保留，不以“限定范围”删除失败项。建议新增一张实际支持矩阵，至少列出 store、provider/transport、直接工具、code-mode、压缩模式、资源和 resume/fork：`支持并验证 / 明确拒绝 / 验证过的同步回退`。每格关联一个测试或实验。首个发布可以只支持 local store、有限文本结果及一个 provider 组合，但必须真实支持有限 Shell 和一种可控 MCP/扩展执行，保留权限与 hooks。

- 可以以后扩展：更多 provider、多模态通知、远程执行端 reattach、吞吐优化、更复杂后台资源类别。
- 不能以后补：当前宣称支持范围内的提交失败不启动、终态归档、压缩/恢复、权限、消息来源和幂等投影。
- provider 的 mock 请求断言证明编码结构，不证明真实端点接受。正式入口至少做一次选定真实 provider 的协议 smoke；没有凭据/模型访问时应明确记为未验证门槛，不能用模拟器补写“正式可用”。无需为此扩展到所有供应商。
- 只开启新模式才能暴露的工具/存储错误，不能通过临时切回 native 隐去；预检失败可以拒绝或在操作分发前选 native，并向用户说明该会话实际引擎。

## 建议的最小实施/验证顺序

| 阶段 | 最小交付 | 有效失败条件 |
| --- | --- | --- |
| A0 | 未执行意图 admission；稳定 operation/commit 身份；commit error/uncertain 冻结；能力矩阵 | 构造但不 poll、ACK 卡住/失败、ACK 丢失时任何 handler 抢跑；code-mode 非预期进入共享 gate |
| A1 | 显式提交记录、durable ACK、publish-after-commit；纯 runtime 去掉权威字符串 history | 提交失败但 live history 可见；同 ID 重试启动两次；半条/半组记录被重放 |
| A2 | 真实历史 snapshot/cut/install 保后缀；与 runtime/交付状态联合 checkpoint | hook/结果在压缩窗口丢失；runtime@s 与 history@s 不一致；旧 epoch 安装覆盖新历史 |
| A3 | local store 有界 resume、历史 cut fork、Ready 保留；request attempt/响应确认恢复 | checkpoint 前活动任务丢失；fork 带执行权；取消请求导致结果丢失或重复追加 |
| B0 | 全退出原因 finalizer，等待期间 steer/stop | 完成事件早于归档；旧结果污染新 turn；硬中断无界等待 |
| B1 | 真 Shell job、显式 resource、leaf gate、受控 MCP；真实权限/hooks | yield 被当作 job 结束；服务卡住 turn；stdin/nested tool 饥饿；权限快照失效 |
| C | 实际启用入口、文档化能力矩阵、选定 provider/客户端 smoke、两轮实现审核及 rebase 演练 | 任一支持格没有行为证据；异步会话被 native 静默续跑 |

每一行应是可单独审核的提交/小补丁组；出现共享接口变更时先由集成者确定类型，再派发不重叠实现。不要让存储 agent 与生命周期 agent 各自发明 Delivered/Closed/owner 语义后再合并。

A0/A1 测试先用无副作用 handler 验证启动时机与冻结；A2/A3 通过后才开放真实副作用工具。现有 semaphore test 继续保留，但必须断言模型请求确实携带 A 的真实内容，不能只断言脚本中的 C 被安排。

验证只运行对应 runtime、core 集成过滤及 history/thread-store/rollout 的受影响测试，必要时受影响 app-server/TUI 测试。无须再次跑全工作区，也不因旧运行的 voice/provider 无关失败改变本轮范围。

## Benchmark 公平性审核

三种策略对比、真实工具、明确模拟模型延迟、mock usage 不冒充账单，这些约束正确。补充四点即可执行，不应阻挡 A：

1. **模拟模型必须按可见结果驱动同一任务 DAG。** 所有策略在同样的前置结果可见后才能启动 C；不要预排 Panng 的 C，也不要给 native 脚本人工串行额外等待。各自使用真实可用的工具语义，包括 native yield/poll。
2. **测完整宿主路径。** Panng 的 fsync、checkpoint、通知、合并窗口、审批/hooks 开销不能绕过；native 不强加其并不存在的 durable operation 写入。两者使用同样任务预算/外部条件，架构固有成本保留。
3. **将等待策略作为显式变量。** 记录 native 默认参数和“合理 yield/poll”的具体值，报告参数敏感性；不挑选只利于 Panng 的延迟比例。service 就绪与 job 退出使用相同产物断言定义完成，不能比较不同终点。
4. **token 是推算而非账单。** 对实际各轮完整请求统计 estimated input/output tokens、可复用前缀，包含 tool schemas/通知/重试/压缩。真实缓存命中和传输增量不是同一概念，不能直接把最长公共前缀全额算作缓存折扣；按情景单列。随机交错策略运行，固定/重置工作目录与产物，报告失败次数、样本数及离散程度。

建议先跑确定性模拟器获得调度上限与估算 token，再在可用时跑小样本真实 provider 测试。只有前者时，可以结论“在该延迟/DAG 下调度耗时降低、估算请求 token 改变”，不能推出真实代码任务成功率或真实账单下降。正式可用与性能优势继续分开判定。
