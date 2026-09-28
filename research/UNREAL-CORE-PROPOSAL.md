# Codex 的 Unreal 风格执行内核方案

研究基线：Codex `41f9084b30812db321a0b592def4f500d1e79cf4`；unreal-agent `1b9f778453f411c029b39b85102aaefb95e7e48d`。

建议（2026-09-28 对抗性审核修订版）：在 Rust 内实现 unreal-agent 的执行语义，采用“独立纯决策内核 + 宿主唯一执行协调者 + 有序历史提交 + 联合恢复检查点”。保留一份上游模型流、上下文准备、工具、权限与 hooks 实现；必要时提取共享执行步骤，不要求 `run_turn` 保持原形。不引入第二个掌握 turn 终止权的 actor，也不预先承诺适配层很薄。

本版依据 [对抗性审核报告](UNREAL-CORE-ADVERSARIAL-REVIEW.md) 修订，F1–F6 的处理与验证入口见第 8 节。当前状态为“设计契约已补充，等待原型验证”，不代表审核问题已经通过实现和测试关闭。

本文是设计建议，尚未修改运行时代码。结论来自以下执行链、存储实现及相关测试的静态阅读，不代表逐文件审计全部仓库。未进行性能实测；本机 Go 执行失败（`bad CPU type in executable`），未运行参考项目测试。

## 1. Unreal 的核心机制与价值

### 会话是持续运转的事件系统

`harness/coordinator/loop.go:86` 的 `Run` 同时接收外部输入、operation 更新、模型响应、宽限计时器和 heartbeat。协调器保持单一状态所有权；模型请求及实际工具工作在外围异步运行。工具可以跨 LLM 轮次存在。

例如同一轮发起 A（1 秒）和 B（60 秒）：A 完成后，模型可读取 A 并发起依赖 A 的 C，B 继续运行。模型暂时没有工作时休眠，B 完成后再次唤醒。价值在于缩短任务的依赖关键路径，而不是增加并发线程数量。

真实行为比 README 的“完成即唤醒”更细：

- 新工具批次有 1 秒宽限期，全部完成可提前结束；到期时只消费已到达结果，空到期不发起新推理。
- `slurpChannel` 按短时间窗口收集事件（1ms / 最多 100 项），减少碎片化请求。
- 模型正在生成时，普通工具完成通常先缓存，等当前响应结束后消费；新的外部输入可以取消并重新发起模型请求。过期响应按 turn ID 丢弃。
- 无新调用但仍有未完成操作时是等待；真正 idle 要求没有模型请求、待投递输入及未完成操作。库保持可用，runner 通过 `StopWhenIdle` 退出。
- runner 的工具等待 heartbeat 默认 10 分钟，可禁用；并不是定时忙轮询。

依据：`harness/coordinator/loop.go:190,225,272,346,417,429`，`grace_test.go`，`scheduling_test.go:140`，`cmd/internal/agentrunner/run.go:174,400`。

### Tool 描述意图，Operation 承担执行

`Translator.Translate` 只校验参数并通过 `Context.Submit` 构造可序列化的工作描述，不做 I/O。一个 tool call 可对应多个 operation；`CallStatus.WaitingFor` 描述它依赖哪些 operation。`TranslateResult` 负责把记录的状态转回模型结果。

Operation 有独立 ID、类型、版本、状态与幂等信息，运行时以 actor/state machine 推进。Shell 的建目录、创建输出文件、启动进程、读取输出是不同状态；文件、进程、计时器等 primitive 执行实际 I/O。这样模型协议与执行资源解耦，也为远端执行和恢复提供边界。

依据：`harness/tool/tool.go`，`harness/operation/operation.go`，`harness/operation/local_manager.go`，`harness/operation/shell.go`。

### 先记录意图，再分发操作

模型响应先入历史，随后工具翻译结果与初始 operations 以同一条存储记录写入，成功后才 `Operations.Add`。恢复时可区分“调用已生成但未翻译”“操作已提交但尚未分发”“执行结束但结果未投递”等窗口。

本地 store 为带版本的追加日志；写入包含 `file.Sync()`，重开时处理不完整尾记录。Inbox 用调用方稳定 ID 在内存去重，恢复时由历史补回 external input IDs。

这不能泛化成 exactly-once：Manager 的 Add 去重承诺只覆盖自身生命周期；Submit 返回也不是持久化确认。尤其是 actor 内部把 checkpoint 加到更新队列之后便可启动 primitive，没有逐步等待 store 的持久化 ACK。

依据：`harness/coordinator/loop.go:780,878,943`，`harness/sessionstore/localfile/store.go:180,413`，`harness/inbox/local.go`，`harness/operation/local_manager.go:258`。

### 模型上下文是状态的投影

ContextBuilder 只做内存组装。它区分已提交前缀与未提交后缀：尚未发送的 running 占位可被真实结果替换；已提交前缀保留，后续结果继续追加。这让“工具仍在运行”成为模型能够理解的状态。

不要把接口愿景当成全部已实现：Builder 定义了 omitted/truncated/compacted Report，但当前默认 Build 只是拼接并返回请求，没有完整压缩算法；协调器也没有把该 Report 写入 turn。已有 fork 分支明确留着 inherited calls / pending-input accounting 的 FIXME。

依据：`harness/contextbuilder/builder.go:103,125,130`，`harness/contextbuilder/contextbuilder.go`，`harness/coordinator/loop.go:346,541`。

### 值得移植的是这组约束的组合

事件循环、actor 和日志本身都是成熟技术。这个项目有价值的设计组合是：**工具脱离模型轮次、纯翻译与执行分离、意图持久化、结果按可用性驱动后续推理。** 它没有证明能普遍降低 token、提高正确率或安全重放任意命令。

Shell 恢复遇到 process 阶段会明确返回“启动未记录导致结果未知”或“退出状态未记录”，不会假装命令恢复成功。远程任务能否恢复取决于具体 handler 的句柄、查询和幂等机制。

依据：`harness/operation/shell.go:194`，`harness/operation/remote_job.go`。

## 2. Codex 现状与真正需要切换的层

当前检出的 Codex 已把循环放在 `core/src/session/turn.rs`，不是旧版本的 `core/src/codex.rs`。

| 关注点 | 本地 Codex 实现 | 迁移结论 |
| --- | --- | --- |
| 主循环 | `tasks/regular.rs` → `session/turn.rs::run_turn` → sampling → task completion | 保留一份宿主业务实现；统一正常和异常出口的终止仲裁，必要时提取共享步骤 |
| 工具并发 | `tools/parallel.rs` 使用 Tokio task 和读写锁；sampling 使用 `FuturesOrdered`，流结束后 drain 全部结果 | 已有并行执行，但常规路径存在批次收齐屏障；换成会话持有的运行操作表与完成事件 |
| 长进程 | `unified_exec` 已有 process manager、后台输出/退出 watcher、yield/后续输入能力 | 复用执行资源，将完成通知接入调度；不能把 tool future 返回误认为进程结束 |
| 工具抽象 | `codex-tools::ToolExecutor`、core ToolRouter/CoreToolRuntime | 适配层记录可序列化意图后调用现有 runtime，避免逐个改写上游 handler |
| 权限与 hooks | `tools/orchestrator.rs`、`tools/registry.rs` | 保留审批、sandbox、重试与 hooks；工具/hook 的历史写入必须进入统一提交入口 |
| 模型传输 | `core/src/client.rs::ModelClientSession` | 保留认证、流式输出、取消和 provider 会话；不新造 provider 层 |
| 历史与存储 | `codex-history`、`codex-thread-store`、`codex-rollout` | 操作提交和联合 checkpoint 共用权威日志；修改有界恢复读取与 fork 边界 |
| 上下文 | `context_manager/history.rs` 和 `normalize.rs`，现有 compaction | 复用预算和压缩算法，改造提交/安装边界以保留压缩期间到达的后缀；补齐通知分类 |
| 外部协议 | app-server 的 thread / turn / item，TUI 与 exec | 维持外部 turn 语义，内部多个模型 step 对应一个用户 turn |

关键证据：`session/turn.rs:2446,2556,2755,3125`、`tools/parallel.rs:45,143,208`、`stream_events_utils.rs:316`、`context_manager/normalize.rs:23`。

只把 `FuturesOrdered` 改为 `FuturesUnordered` 仍会在 drain 等完全部结果，也没有跨轮生命周期、恢复或上下文契约，不能完成这次机制切换。

## 3. 推荐结构：纯决策内核 + 宿主唯一执行协调者

新增 `codex-rs/panng-runtime`（crate 名 `codex-panng-runtime`），承载纯状态转移、调度与版本化 operation 数据契约。内核只接受事件并产生决定，不启动任务、不写历史、不发送 UI 事件。让 `codex-core` 依赖它，内核不反向依赖 core；core 内新增 `src/panng/` 集中适配目录。

| 所有者 | 职责 | 变化边界 |
| --- | --- | --- |
| 共享宿主服务 | 模型请求/流、step 快照、预算、压缩算法、工具、审批与 hooks | 保留一份业务实现，必要时从大函数提取；不复制采样循环 |
| Panng 纯内核 | 操作身份/状态、ready 集合、合并窗口、恢复策略和生命周期转移规则 | 只认识自有事件、ID 和版本化数据；状态由宿主协调者持有 |
| 宿主协调者（core/panng 接入 task） | 调用内核、启动/取消 I/O、维护任务所有权、提交历史、执行终止协议 | 唯一落实继续/等待/完成决定的组件；现有 task completion 仅在仲裁完成后发布结果 |
| 历史提交入口及存储适配 | 为语义事件分配序号、持久化、投影、安装联合 checkpoint | 上游所有相关历史写入和替换都经过该入口；不可只接管工具最终返回值 |

内核输出“提交操作 / 消费就绪结果 / 等待事件 / 可继续采样 / 开始收尾 / 可发布终态”等决定，宿主执行 I/O 并送回事件。时间与 I/O 结果由宿主作为输入提供，使状态转移可确定性测试。工具驱动可并发执行，但不能自己结束 turn 或直接改变模型历史。上游错误、预算、Stop hook 结果都进入同一个仲裁流程，不能通过 return/break 绕过它。

内核不存储 Arc<Session>、StepContext、模型 ResponseItem 或上游枚举的序列化快照；这些留在适配层。可序列化的执行环境描述与版本校验由适配层提供，不能用一个进程内引用充当恢复数据。既不复制完整 provider 协议，也不把不断变化的上游类型传播到内核每个模块。

源码接入面至少包含工具分发/等待、所有终止路径、历史追加及替换、checkpoint 生产/读取/fork、结果分类与 UI 映射。StepContext、会话级执行锁、取消 token 与 task 强制销毁路径一起审计。PostToolUse additional_contexts 等附带写入必须统一接入，不能保留绕过提交入口的路径。

优先在现有 task 中建立这一协调边界。若必须在 `run_turn`、RegularTask 和 task completion 的多个分支复制相同的模式判断，或仍存在未经仲裁的历史/终止路径，则判定该接入方式失败：提取共享的 step/effect 服务，由唯一协调循环调用。以实际 diff 和语义测试比较两种边界；“不改函数原形”不再是验收条件，“不复制宿主业务实现”仍是硬约束。

短期桥接使用 `InvokeCodexTool` operation：记录稳定工具身份、参数、来源 step、执行环境及权限描述，再调用原有 executor。具体工具的可恢复 driver 放在 fork 自有模块，按能力选择；不批量把上游 Shell/MCP handler 重写成 Unreal state machine。不能把整个异步闭包序列化，也不能声称它可恢复。

通过实验开关逐线程选择调度模式；运行模式与版本写入持久化记录。保留上游 native 路径作为长期行为基线和新会话回退入口，不复制一份 native 实现。活动异步会话不能静默降级到 native；旧会话默认沿用原语义，切换必须经过已定义的安全边界。

已有 ExtensionRegistry/ExtensionData 可用于附着运行时状态、生命周期和静态提示贡献，但不能假定它已经提供可替换调度器。ToolLifecycleContributor 主要观察生命周期，ModelResponseInterceptor 包装响应流，都没有直接替换 `drain_in_flight` 或持有工具任务的权能；ExtensionData 也不提供持久化。新接入接口先作为 fork 内部接口，避免为了“插件化”修改整个公共 extension-api。

不建议 Go sidecar：虽然可直接调用参考实现，但需要跨进程复制模型流、权限审批、hooks、工具目录、会话与 UI 生命周期，长期边界比在 Rust 里实现小内核更宽。也不建议仅用 MCP 包装 Unreal，因为 Codex 原有调度仍然包在外面。

## 4. 必须先锁定的行为契约

### A. ID、调度与生命周期

区分 `thread_id / user_turn_id / model_step_id / call_id / operation_id / resource_id`。模型 generation 标识一次可取消的推理，execution epoch 标识操作执行权，二者独立。运行操作由会话持有，记录所属 turn 和来源 step；换 step、steer 或丢弃旧模型流不自动销毁操作。

同一会话最多一个常规模型采样请求；压缩等上下文替换与新采样串行仲裁。结果按已记录序号投递并短暂合并；模型生成中继续收集结果，无新信息则休眠。取消后拒绝旧 generation 的模型响应；已经产生副作用的操作事件仍须按 operation ID/epoch 归档，不因推理换代而静默丢弃。

正常完成要求模型已结束工作、无待采样输入/结果、无阻塞完成的 job；已明确脱离的 resource 不计入此条件。Stop hook 的“继续”仍可重新开放采样；真正 stop 进入收尾。内核只提出转移，宿主 task 是唯一落实终止的所有者。

| 原因 | 协调动作 | 完成与资源策略 |
| --- | --- | --- |
| 自然 idle | 检查待交付事件与阻塞 job，运行 Stop hook | 条件满足且 hook 允许后发布完成；resource 保留 |
| 用户 steer | 保留输入、按上游抢占策略取消/更新模型 generation | 不发布 turn 终态；现有操作保持原归属，不重新执行 |
| 用户硬中断 | 关闭本 turn 新操作准入，取消模型与未脱离 job，有界等待 | 发布 aborted；超时操作标记 indeterminate；已脱离 resource 不因停止推理自动销毁 |
| task replaced | 按原 turn 的中断协议收尾，再交接新 turn | 旧结果不能作为新 turn 的完成结果；resource 按会话归属保留 |
| 不可重试模型错误、hook stop、预算耗尽 | 关闭准入并停止采样，取消未完成 job，有界归类 | 沿用上游错误/停止原因，记录尚未被模型消费的结果，不强制再请求模型 |
| 存储失败 | 禁止新副作用、停止采样并尽力取消 job | 报告持久化失败，禁止继续任务；不能伪造已写入终态，下次从最后有效提交核对未知操作 |
| 会话 shutdown | 关闭准入、取消模型/job，并落实宿主资源清理策略 | 只有已有可靠外部所有者且显式支持存活的 resource 可转移；无此能力则清理 |

常规收尾顺序是“关闭准入 → 发出取消 → 有界等待/归类 → 提交终态 → 发布外部终态 → 释放 active turn”。task 的强制 abort 路径也必须由宿主保留的 supervisor/finalizer 完成归类，不能让被销毁的 future 独自承担清理。内核终态必须幂等；实时只发布一次，断线重放使用稳定事件身份，不承诺跨网络 exactly-once。磁盘故障时可发失败通知，但不谎称恢复数据完整，也不进入下一轮工作。

supervisor 只在原 task 退出后接管同一生命周期状态及关闭凭证，不能与原 task 并发发布终态；所有完成出口经同一幂等 finalizer。其实际接口属于阶段 0 的验证内容。

迟到操作更新可修正日志中的未知结果，保留原 turn/epoch，不能复活已结束的 turn 或自动注入新 turn；需要新任务消费时经过明确内部通知路由。新会话恢复必须取得独占执行权，旧执行端回报需查询核对，禁止两个恢复实例同时执行同一操作。

目录、环境、工具版本和权限绑定来源 step，恢复时重新校验。工具句柄由宿主持有而非模型流持有；审批等待不阻塞协调器。并发锁提升到会话范围，但锁覆盖的是定义好的执行临界段；驱动未审计前维持现有串行约束，不擅自放宽。

### B. 有序历史提交与压缩一致性（F1）

为所有影响模型历史的语义事件设置一个提交入口，包括模型完成项、工具结果、hook 附加上下文、真实用户输入、完成通知，以及压缩/回退替换。UI token delta 可即时显示，但不能绕过语义事件提交。每个事件有稳定 ID、来源 turn/step/operation 和单调序号；执行意图在持久化确认之后才能分发。

启用异步模式后，`Session::record_conversation_items` 等相关写入口需要统一路由；审计工具/hook 直接写历史及替换历史的路径。提交入口可以先持久化 hook 的附加事件再确认，不等待整个工具 future 结束；协调者等待工具期间仍要处理提交请求，避免互相等待。单独保存最终工具返回值不能覆盖这些附带事件。

提交入口按来源 turn/epoch 校验可见性：已结束 turn 的迟到 hook/结果可归档，不能未经仲裁变成新 turn 的模型输入。模型可见投影与操作事实归档采用独立交付状态，避免为防止误投递而直接丢弃执行证据。

压缩事务采用明确的切点和后缀：

1. 在序号 c 捕获模型历史和 history epoch，压缩算法只处理该前缀。期间继续接收和记录事件，不持有历史锁等待模型或工具。
2. 结果返回后，经提交入口取安装边界 s；准备“压缩后的 ≤c 前缀 + (c,s] 的原始有序后缀”，以及推进至同一 s 的 runtime checkpoint。这里 s 是准备安装时所有已接受语义事件的水位，不是最初请求的 c。
3. 将联合 checkpoint 作为一个提交单位持久化成功后安装；其后的事件在 checkpoint 后按序重放。普通追加不会使结果失效；rollback、fork 切换或其他历史替换导致 epoch 不符时拒绝旧压缩结果并重新准备。
4. 崩溃前后恢复得到相同后缀和操作状态。已提交模型前缀不因每条完成通知重写；窗口替换仅走上游明确的 compaction 边界，并让原有客户端逻辑判断连接复用。

保留上游压缩算法，但安装、投影及 checkpoint 协议需要改造；实时保留的后缀不能在序列化 checkpoint 时再次遗漏。适用路径包含自动/手动、本地/远程压缩与恢复。若后缀导致仍超上下文上限，先合并/截断可投影片段或再压缩，仍无法满足则按预算失败收尾；不能带着超限输入采样或无限忙循环。

临时原型可禁用活动 job 期间的压缩，并在硬预算处有界停止，但它不能通过完整目标的压缩验收。任何仍能绕过提交入口写模型历史的已启用路径，均视为设计未贯通。

### C. 一次工具响应与后续完成通知（F5）

Unreal 的默认 builder 可能让同一 call ID 先出现 running 结果，后续再追加真实结果。这种 wire 表达在 Codex 当前 provider、历史规范化、缓存与恢复路径上的兼容性尚未证明，不应直接照搬。

建议初版采用明确的兼容表达：

1. 下一次模型请求前工具已完成：按现有工具结果格式返回。
2. 尚未完成但已有其他事件需要推进：只发送一次带 `operation_id` 的 running/accepted 工具结果。
3. 其后完成：追加带同一 operation ID 的结构化完成片段，不再伪造第二个相同 call ID 的 tool output。该片段保留工具来源与不可信数据属性，不冒充用户授权；实现于 `core/context` 并遵循 ContextualUserFragment 约束。
4. 运行状态、投递形式与交付记录一同持久化，确保恢复后仍产生相同前缀。首版只开放已验证的文本结果；多模态异步投影必须保留原生类型并单独验收，未适配工具走原有同步路径。

codec 必须显式规定消息 role、content kind、contextual matcher、call/operation/source ID 与 envelope metadata 映射。新片段在 `core/context` 实现 ContextualUserFragment，并注册现有识别入口；trait 实现不会自动完成历史分类。先对原结果执行现有外部来源检查，再转换并保留来源和已交付消息等元数据。分别测试“不构成用户授权”“不产生 user turn 边界”“压缩/截断后仍正确分类”，不能互相替代。

初版新增模型通知每条硬上限为 1,000 tokens（含包装、ID、路径等；使用模型 tokenizer 或经验证的保守上界），超出内容作为产物引用保留，单次总注入还受现有剩余上下文预算限制。不得把同一结果无限拆分以绕过总预算。若某类片段确需超过 1K tokens，按 AGENTS.md 标为 P0 单独人工审核；任何片段不得超过 10K tokens。此限制不把原有工具输出大小规则偷偷改写为统一 1K。

只有验证 provider 原生异步结果能力后才增加相应编码策略。优先在调用 normalize 前写入完整的 running/真实结果配对，保留上游 normalize 算法；仅在原型证明不可避免时增加针对活动调用的窄接入点，不能另建整套 normalize。模型提示应明确：可继续独立工作、无需自行反复查询、不因没有新调用而判定任务结束。

UI 投影区分工具交互、job 和进程/resource 的生命周期：generic pending call 的 running 回执不伪造真实完成；现有 exec_command 若已完成一次交互并交回 process_id，可按原语义完成该交互，进程退出仍由独立事件表达。不能把它们都归并成一个 item/completed；逐项验证现有 app-server 映射，必要的协议增量单独审核。

### D. 一个权威日志与联合恢复检查点（F3）

完整目标仍然是在一个权威日志内提交调用意图、操作与交付游标，但需缩窄存储层对业务的认知。建议评估一个带 namespace/version 的扩展记录及有持久化确认的追加边界；Panng 的操作状态、恢复策略、投递语义均由 fork 自有 codec/适配层解释，不把 Operation 及其每个新状态加入公共 RolloutItem/ThreadStore 接口。该扩展槽当前不存在，属于需要实测影响面的 fork 补丁，不是免费扩展点。

调用与操作放进同一提交单位后，宿主适配层需将对应调用恢复为原有历史项，并维持稳定 item ID、顺序与投影；通用历史读取、fork、压缩和分页仍要核对，不能靠“opaque payload”省略这些行为。未知版本必须明确失败；不能承诺旧版原生 Codex 可以续跑这种会话，也不要借用 EventMsg 或用户消息偷存内部状态。

本版选择 **runtime checkpoint 与 history checkpoint 共享提交单位和一致安装边界**，不再把恢复切点留给后续阶段决定。联合 checkpoint 至少包含：历史窗口/epoch、序号水位、压缩前缀与保留后缀、活动操作及执行 epoch、环境/权限描述、resource 所有权、已终止未交付事件、投影与模型请求交付记录、codec/engine 版本。artifact 可用经验证的持久引用，不把大输出塞入元数据；缺失引用显式报错。

交付状态区分“已写入上下文”“已包含在某次模型请求”“该请求响应已接受”，不以发请求等同模型已经消费。请求前记录其事件范围与身份，接受响应时记录确认；响应丢失或取消时只重建待处理上下文，不因重复推理自动重放已有操作。具体 provider 重试身份仍须验证，不承诺远端推理 exactly-once。

现有 model_context 在最新 compaction 处停止逆向扫描，必须改为同时取得对应 runtime checkpoint，然后只重放其后的完整提交。不能把摘要里的 running 文本当成执行状态，也不能仅追加 operation 记录却继续沿用原恢复读取。首个 checkpoint 前以线程起点为边界；升级格式必须先迁移或拒绝，不静默用不完整状态恢复。

保留两种明确的工程退路：扩展联合记录；或在同一日志用多个 frame 加最终 commit marker，由读取者忽略未完整提交的组。二者都必须原子地发布同一逻辑 checkpoint。若放弃联合 checkpoint，改用独立全日志/索引恢复，则须作为新的设计决策重新证明读取上限、cut 关联和 fork 一致性。旁路数据库也不得因 diff 较小而免除跨存储故障验证。

这需要实现工作：`record_canonical_items` 目前主要是入队；writer 的 `flush()` 路径不能直接当作 `fsync` 承诺。新 commit 需明确 sync_data/sync_all 与新文件目录同步、撕裂尾记录检测、提交 ACK 丢失后的按 ID 查询/去重。不能假定现有 append API 已经满足事务性。

恢复顺序：获取独占执行权 → 加载最后完整联合 checkpoint → 重放后续有效提交 → 重建操作表与交付状态 → 核对执行端/权限 → 恢复或标记不确定 → 交付尚未处理的结果。故障注入必须覆盖写入中断、fsync/ACK 窗口及 checkpoint 已提交但内存未安装。磁盘失败后不继续产生新的副作用。

### E. 恢复与历史 cut fork

每种 operation 声明恢复策略：

- `ReplaySafe`：已证明可安全重复的操作。
- `Reattach`：执行端有可验证的持久句柄与结果查询协议。
- `NeverReplay`：任意 Shell、未知副作用 MCP、不能重建的 code-mode 执行等；崩溃窗口结果未知时记录 interrupted/indeterminate。

“写了 started 再执行”仍可能发生 started 已记录而命令尚未开始的崩溃；“执行后写 done”也存在结果已发生但 done 未提交的窗口。没有执行端幂等/事务协议，就不承诺外部副作用 exactly-once。首版选择保守未知，后续复用 exec-server 时也要验证它的跨进程句柄存活与去重契约。

Fork 使用所选历史 cut 之前最近的联合 checkpoint 并重放到该 cut，不能用父线程当前状态代替历史状态。子线程获得新 epoch；继承记录只保留来源和状态，不复制父 job/resource 的执行权、活跃锁或可执行句柄。继承的 pending call 以结构化“来自父线程、此分支不执行”状态闭合，已发送的回执不改写，子线程不会永久等待父操作。父线程后续完成不自动投递给子线程；重新执行必须是新的显式操作。

fork 前缀 ordinal、分页投影、checkpoint 过滤与回退路径需同时适配。历史 cut 若已被保留策略清除且无法准确重建，返回不支持该 cut，不能猜测或从当前状态伪造。格式迁移及读取成本列入存储验收。

### F. 区分 call、job 与 resource（F4）

| 类型 | 生命周期 | 是否阻塞正常 turn 完成 |
| --- | --- | --- |
| call | 模型发起的一次工具交互，有稳定 call ID 和一次 wire output | 等待它得到真实结果或持久化的运行回执；最终工作是否阻塞由 job 决定 |
| job | 有确定终态的有限工作；可跨模型 step，exec yield 不结束它 | 是，直到终态及必要结果已被处理 |
| resource | 经明确移交、可跨用户 turn 存活的服务或交互终端 | 否；启动/移交动作本身及就绪回执仍需完成 |

不能仅凭 yield、运行时长或命令文本猜测 resource。工具适配器通过明确的后台语义或显式 detach 动作提交 job→resource 转移，验证请求允许其持续运行、执行端支持管理且有就绪依据；同一提交记录 handle、所有者和清理策略。未分类工具沿用 native 路径，不悄悄改变语义。首版只接管有限文本 job。

resource 的进程输出/退出沿用宿主事件，保留原始身份，并记录内部状态。模型通知只在允许的活跃 turn 边界消费，空闲时保留供下次读取，不伪造用户输入、自动新建 turn 或复活已结束的 turn。用户显式停止资源/清理终端才销毁已脱离资源；会话 shutdown 按 A 节策略处理。交互 write_stdin 是对现有 resource 的新 call，不能被资源存活期间一直持有的独占锁饿死。

### G. 控制额外开销

默认试验参考项目的 1 秒工具合并窗口，之后通过测量调优；为并发数、事件队列、模型唤醒频率、单条输出和总上下文设上限。保留 Codex 现有预算和压缩逻辑，长输出落产物文件。异步反馈可能增加模型轮次与 token，不能只看总耗时。

## 5. 实施顺序与验收

先验证边界，再扩大内核和工具覆盖。本阶段交付的是可推翻设计假设的纵向原型，不先开发完整内核再尝试接入。

| 阶段 | 实际交付 | 通过条件 |
| --- | --- | --- |
| 0：验证所有权与提交边界 | 默认关闭的最小原型：受控 pending tool、有序历史提交、退出仲裁与联合 checkpoint | 下表实验 1–4 通过；列出所有旁路写入/退出，覆盖范围内不能留旁路；比较实际 diff 后决定原地接入或提取共享步骤 |
| 1：固化内核与存储 | 自有纯决策器、版本化 codec、完整提交与恢复协议、文本通知映射 | 确定性回放及实验 6 通过；存储故障窗口可恢复；同一操作不会因 ACK/结果丢失被盲目重放 |
| 2：有限真实任务 | 接入一种受控非交互 Shell job、复用模型流/执行器、完成 UI 和取消适配 | 保持 handler pending 的测试仍通过；另测进程 yield 后自动完成通知；权限/hooks/压缩正确，未知副作用不重放 |
| 3：资源与工具扩展 | 显式 job/resource 移交，MCP、交互终端、apply_patch、code mode、子代理等分批接入 | 实验 5、多模态独立测试及兼容测试通过；不支持的能力明确走 native；评测通过后才默认启用相应能力 |

阶段 0 的 F1/F2/F3 是正式接入前的门槛；原型中临时限制不等于通过完整验收。F4 必须在启用 resource/交互工具前完成；F5 与首个通知 codec 一起实现；F6 的对照方法从第一个测试就使用。阶段间可拆成更小提交，复杂改动遵守 AGENTS.md 的可审查规模；不能为数字目标删除必要契约。

阶段 2 未适配的工具使用明确同步 barrier/native 策略，不能假装获得持久恢复能力。interactive write_stdin 仍是模型/用户输入通道；已适配有限 job 由完成事件驱动。始终共享宿主业务实现与 native 路径，避免永久维护两份业务循环。

### 六项必须能证伪设计的实验

| 实验 | 控制交错 | 验收断言 |
| --- | --- | --- |
| 1. 真正跨 step | A 完成，B 的 handler future 被 gate 挂起且禁止自行 yield；模型据 A 启动 C | C 启动时 B future 仍 pending；native 对照在放行 B 前无法到达 C；无新信息不额外采样，每 call 只有一次 wire output |
| 2. 压缩后缀 | 远程压缩取得快照后挂起，放行 B 的 PostToolUse 上下文和结果，再完成压缩 | hook 内容和结果各保留一次、顺序明确；内存输入与崩溃恢复输入一致；本地/其他压缩安装入口复用同一协议 |
| 3. 所有退出原因 | B pending 时触发模型错误、hook stop、硬中断、预算耗尽、task replaced、存储故障、shutdown | 对照 A 节转移表有界收尾；原 turn 终态幂等；不向新 turn 写入旧完成结果，不因取消产生重复启动 |
| 4. checkpoint/crash/fork | O 在压缩前启动；在 checkpoint 写入/确认/安装及结果交付前后注入崩溃；从历史 cut fork | 有界恢复拿到 O、交付记录和权限/epoch；缺尾记录不成为有效 checkpoint；子线程无父执行权，也不永等父操作 |
| 5. job/resource | 同时测试有限任务、已就绪服务与 write_stdin 交互进程 | job 完成唤醒；服务可保留而 turn 正常结束；交互不被长锁饿死；停止推理/停止资源/shutdown 区分清楚 |
| 6. 通知语义 | running 回执后追加含类似用户指令的文本完成通知，再截断、压缩、resume | 无第二相同 call ID 的工具结果，无新授权或伪 user turn；来源 metadata 保留；片段及总预算硬上限有效 |

实验 1 使用 mock provider、gate/oneshot 或虚拟时间，不以真实 1 秒/60 秒 sleep 判断正确性。native 分支在观察到等待状态后由测试控制器放行 B 以结束测试，避免用“等 C 才放行 B”让对照永久死锁。普通 `exec_command` yield 可能让 native 也提前启动 C，因此只作为独立的有限进程通知测试，不能代替实验 1。

核心集成测试至少覆盖：乱序完成、多个操作组成一个调用、运行中继续调用、重复/迟到更新、新输入打断模型但不误杀工具、硬取消、审批等待、存储故障、进程崩溃、最后结果投递、fork 与压缩、工具互斥、code mode 嵌套调用及旧会话恢复。按仓库要求使用 `just test`，工具变化做 core 集成测试，协议/界面投影补相应 app-server 测试和必要快照。

复用 `core/tests/suite/tool_parallelism.rs`、`hooks.rs`、`interrupt_hooks.rs`、`compact_resume_fork.rs` 的测试工具，优先通过公共会话/API 验证行为。执行阶段按仓库要求跑对应 `just test`；涉及 core 的完整测试套件须按 AGENTS.md 在获授权后运行。此文档修订不需要执行编译或测试。

对比固定任务集时固定模型、提示、权限、预算与环境，测量成功率、端到端时延、token/费用、缓存命中、取消延迟及恢复结果。这里只预期能够缩短独立长短工具混合任务的关键路径；串行依赖任务和快速工具任务未必受益。

## 6. 设计取舍

最小而完整的接入单位是“调度决策 + 有序历史提交 + 唯一生命周期仲裁 + 联合恢复 checkpoint”。future 表、结果通知或单独的 durable append 都不足以构成完整切换。

优先验证纯内核与宿主唯一协调者；若实际接入变成散落的模式分支，则提取共享 step/effect 服务并重新比较。保留一份上游业务实现比保留 `run_turn` 的函数结构更重要。Go sidecar/MCP 包装和仅复用 exec yield 的路径不满足同一目标，不作为规避所有权问题的捷径。

推荐方向仍是 **Unreal 的执行语义、Rust 的纯决策内核、Codex 的共享宿主服务**。边界和维护收益需由六项实验与移植数据证明；独立 crate 不是“天然容易 rebase”的证据。

官方接口背景：[Codex App Server](https://learn.chatgpt.com/docs/app-server)。本方案中的具体实现判断以文首两个本地提交为准。

## 7. 长期 rebase 评审

结论：初版方向合理，但不足以称为长期 fork 下的优雅方案。“独立 crate + 薄适配层”只说明代码放在哪里；只有约束上游修改面、避免宿主业务复制、验证存储边界和持续测试语义，才能约束维护成本。

### 本地历史证据

基于当前 HEAD 可达历史，按 `git log --since=2026-09-14T00:00:00Z --until=2026-09-28T00:00:00Z HEAD -- <path>` 统计提交数（目录包含多个文件，各行有重叠，不能相加；不使用 --follow，不代表重命名前的全部历史）：

| 路径 | 触及该路径的提交数 |
| --- | ---: |
| core/src/session/turn.rs | 20 |
| core/src/tools/parallel.rs | 3 |
| thread-store/src | 23 |
| history/src | 15 |
| ext/extension-api/src | 19 |

例如 `12de0e395d` 修复 steering 的 WebSocket continuation，`f92655d07f` 增加用户输入抢占，`5f3180c793` 修复压缩时的模型/权限程序配对，`12cb14f7b7` 改变默认分页历史行为。复制主循环可能不产生 Git 冲突，却会漏掉这些更新。extension-api 也在演进，不能把它当作稳定 ABI。

### 维护边界与补丁结构

- 自有目录承载调度、恢复、codec 与测试；上游已有文件尽量只保留调用点、所有权调整和不可避免的存储支持。
- 维护接入清单：每个上游修改文件、修改原因、依赖的生命周期假设、对应契约测试。把“工具全部结束后才能执行”的假设显式列出。
- 按边界接入、内核、存储、工具能力拆分提交，每个行为变化连同其必要测试一起提交，保持每步可构建可验证；不混入大规模格式化或无关重构。不硬性承诺全部改动少于某个文件数。
- 保持一份上游宿主逻辑，native 模式作为对照；不复制 turn.rs 到自有目录，不引入到处散落的 feature 分支，不用源码文本替换脚本重新注入补丁。
- 公共 app-server 类型、CLI 参数与配置尽量不扩散 operation 细节；不可避免的配置注册、Cargo/Bazel 构建接线属于显式维护项。

### 每次同步的验收

1. 在临时集成分支上 rebase 到固定 upstream SHA；使用 `git range-diff` 比较旧/新补丁栈，人工检查所有接入点的语义变化。`rerere` 只能帮助重复文本冲突，不能代替检查。
2. 内核契约测试验证事件乱序、合并、取消、恢复和交付；适配层测试验证 provider 输入、权限、hooks、压缩、fork 与 UI 生命周期。
3. native 关闭特性时，与相同 upstream SHA 的原生构建运行同一组确定性 mock 场景对比；开启时验证目标语义。相关上游测试也要运行，不能只测自有代码。
4. 对被修改的上游区域做依赖扫描，判断其新增逻辑是否同样适用于异步模式；即使编译通过、rebase 无冲突也执行这一步。
5. 升级测试同时加载上一个 fork 发布版本的真实格式 fixture；Git rebase 成功不代表用户会话可升级。

原型通过后移植到至少两个具有相关语义变化的可用上游基线。记录冲突文件数、修改的上游函数数、适配修改行数及每次需要重新证明的契约，不只统计 diff 大小。历史基线演练只是未来维护成本的代理指标，后续每次真实同步持续记录。

判定接入边界失败的条件：存在未纳管的历史写入/终止路径；相同状态判断散落在多个上游函数；必须复制模型/权限/压缩业务才能保持正确；checkpoint 不能覆盖活动操作或恢复输入不一致。出现这些情况先调整边界，不靠继续堆 feature 分支维持“薄适配”的说法。

这些步骤是维护流程建议，本次没有执行 rebase、修改 Git 配置或运行构建。

## 8. 对抗性审核反馈处理记录

原审核报告保留不变，其方案行号指向审核时版本。以下“已采纳”仅表示本版已给出设计响应；所有验证仍待实施。

| 审核项 | 本版设计响应 | 验证与门槛 |
| --- | --- | --- |
| F1 · 历史写入与压缩竞争 | 4B：统一提交入口、c/s 切点、原始后缀保留与 epoch 校验；不持锁跨模型等待 | 实验 2；正式接入前必须通过 |
| F2 · turn 终止权分散 | 第 3 节及 4A：纯决策器、宿主唯一协调者、全部退出原因转移表、强制 abort 后收尾、generation/epoch 分离 | 实验 3；正式接入前必须通过 |
| F3 · 恢复读取切点缺失 | 4D/4E：选择联合 checkpoint、适配有界读取、历史 cut 重建及 fork 执行权隔离 | 实验 4；正式接入前必须通过 |
| F4 · 后台资源阻止结束 | 4A/4F：call/job/resource 分层、显式移交与取消/清理策略、执行锁不覆盖资源整个寿命 | 实验 5；推广资源工具前必须通过 |
| F5 · 通知分类不完整 | 4C：role/kind/matcher、来源及 metadata 映射、独立授权/turn 边界断言与硬上限；首版文本限定 | 实验 6；首个 codec 接入必须通过 |
| F6 · Shell 验收缺乏区分度 | 第 5 节：gate 挂住真正 handler future，native/new 对照；yield 另测，不用墙钟耗时替代 | 实验 1；从首个原型开始执行 |

仍待验证的决策包括：完成通知对各 provider/模型的兼容性与效果、所有相关写入口能否集中路由、联合 checkpoint 的实际上游修改面，以及原地接入与提取共享步骤哪个更易维护。未证明之前保持实验开关关闭，不声称审核已通过。
