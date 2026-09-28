# Unreal 执行内核方案：对抗性架构审核

审核日期：2026-09-28。被审文件：`research/UNREAL-CORE-PROPOSAL.md`（长期 rebase 修订版）。

固定源码基线：Codex `41f9084b30812db321a0b592def4f500d1e79cf4`；unreal-agent `1b9f778453f411c029b39b85102aaefb95e7e48d`。已核对本地 HEAD、根目录 AGENTS.md、关键执行/上下文/存储路径及相关测试。本次没有运行构建、测试、rebase 或联网验证；只新增本报告，不修改被审方案和代码。下文路径均相对于项目根目录，行号对应上述基线。

## Verdict

**Request changes：可以开展有明确失败条件的原型，不宜将当前边界认定为已经成立的最终架构。**

独立 Rust crate、复用工具和模型传输、不复制 `run_turn`、保留 native 对照路径，都是合理选择。但“上游宿主流程 + 独立会话调度内核 + 薄适配层”目前仍是职责愿望，没有证明这些职责可以在现有代码上独立拥有。最薄弱的部分不是事件队列，而是：**谁有权写历史、谁有权结束 turn，以及恢复读取的切点是否覆盖仍在运行的操作。**

报告中的 P1 表示会阻止安全、完整接入的设计问题；P2 表示会低估维护成本或产生错误验收的缺口。这里没有新的运行时代码，因此不把潜在场景描述为已经发生的产品 bug。每项区分“已证实的源码事实”“设计未说明”及“待实验验证”。没有证据支持直接判定 Rust 方案不可行，也没有证据支持“必然只需薄适配”。

## 哪些问题必须先定，哪些可以随实现解决

- **正式接入前必须定下协议：F1、F2、F3。** 历史提交顺序、终止所有权和恢复切点决定模块边界；留到后面会迫使重写已建内核及适配层。
- **推广到后台/交互工具前必须定下：F4。** 有限非交互任务原型可以先行，但必须显式排除 service/resource，不能沿用同一 idle 规则扩展。
- **可与 codec 实现一同解决：F5。** 不要求先改所有历史算法；需要在首个通知贯通时补齐分类、来源与边界测试。
- **应立即修改验证设计：F6。** 只需改变原型的测试控制方式；不要等性能评测阶段才发现 native 同样能通过。

## 发现

### F1 · P1：工具完成的附带历史写入不经过调度器，压缩可能丢掉已经到达的上下文

**分类：源码事实已证实；在新并发语义下的冲突可由控制交错复现，尚未执行原型。**

方案位置：第 3 节第 87–95 行；第 4 节第 125–128、154、158 行。方案强调调度权唯一、复用压缩，并要求审计 hooks，但尚未定义模型历史的唯一提交者及压缩期间的写入规则。

源码证据：

- `codex-rs/core/src/tools/registry.rs:717–739`：工具完成后执行 PostToolUse hook，并直接调用 `record_additional_contexts`。这发生在工具 future 最终返回之前。
- `codex-rs/core/src/hook_runtime.rs:849–864`：上述函数直接调用 `Session::record_conversation_items`，并不把消息作为返回值交给采样循环。
- `codex-rs/core/src/compact_remote_v2_attempt.rs:39`：压缩读取会话历史快照；`codex-rs/core/src/compact_remote_v2.rs:244–252` 等待远程压缩，`:359–372` 安装替换历史。
- `codex-rs/core/src/session/mod.rs:4102–4129`：安装时获取 settings 持久化锁及 state 锁后整体替换历史，并未以原历史版本做 compare-and-swap 或自动保留快照后的全部消息。
- `codex-rs/core/src/session/turn.rs:550–551` 明确写着：只有 sampling 及其工具全部结束后才处理异步 hooks；`:707–708` 也明确依赖当前 task 串行化压缩历史更新。

具体失败序列：A 完成触发新 step，B 仍在运行 → 当前历史达到阈值，宿主取 H0 开始远程压缩 → B 的 PostToolUse hook 把附加上下文 H1 写入会话 → 远程结果只覆盖 H0，安装时替换当前历史 → H1 消失。即便 B 的最终结果被调度器正确缓存，H1 也已经绕过它写入并丢失。

这是**语义排序与快照一致性问题，不是 Rust 数据竞争或“缺一把 Mutex”**。局部 state 锁不能保护横跨网络等待的快照事务。单纯把完成 future 放进会话表、或在 normalize 前投影结果，无法解决。

影响：hook 的反馈/约束可能丢失；运行时已投递游标与实际模型上下文不一致；resume 与实时执行可能出现不同历史。上游新增任何工具内历史写入，都可能无文本冲突地破坏 fork。

建议修正：明确一个历史提交门，将工具返回、hook 附加上下文、异步通知作为带序号的待提交事件；压缩采用明确 cut，替换的仅是 cut 以前的上下文，之后的事件保留为后缀。若要复用现有 hook 写入函数，应让它们经过同一门，而不是只改 `drain_in_flight`。也可短期禁止活动异步操作期间压缩，但必须规定达到硬上限时的有界等待/停止行为，且不得把该降级宣称为完整目标。不能持有 history 锁等待工具，而工具又需要该锁提交 hook。

### F2 · P1：唯一调度权没有覆盖所有终止路径；正常决策接入无法保证错误与取消时收尾

**分类：源码事实已证实；生命周期转移规则未说明。**

方案位置：第 91、95、109–115 行。方案一方面令内核决定继续/等待/空闲，另一方面保留上游错误、预算、停止策略，并规定中断取消操作。缺少的不是状态名，而是这些决策发生冲突时谁提交最终转移。

源码证据：

- `codex-rs/core/src/session/turn.rs:599–644` 自行决定压缩、错误返回和继续；`:647–739` 自行运行 Stop hook、继续或结束；`:799–819` 遇一般错误可以报告后 break，并返回正常的 `Ok(last_agent_message)`。
- `codex-rs/core/src/tasks/regular.rs:105–123` 在 `run_turn` 返回后依照 terminal_error 和 input_queue 决定 task 是否结束，并不知道新 operation 表。
- `codex-rs/core/src/tasks/mod.rs:813–875` 发布 TurnComplete/TurnAborted、清除 active turn，然后可以启动待处理工作。
- `codex-rs/core/src/tasks/mod.rs:923–953` 取消 task token，经过有界宽限期后 abort task handle；`codex-rs/core/src/tools/parallel.rs:194–254` 的现有工具 handle 依赖 AbortOnDropHandle 和取消 token。

具体失败场景：B 跨 step 运行，新模型请求遭遇不可重试错误。若只替换正常路径的 `needs_follow_up`，上游返回并发布 turn completed，而会话表仍持有 B；B 随后把结果写入已结束的 turn，甚至与新 turn 的上下文/清理交错。另一种错误是把 B 仍绑定到被丢弃的模型请求 future，导致正常换 step 就提前 abort。反过来，机械地等待全部 B 结束才允许任何错误收尾，又会让硬停止失去有界响应。

建议修正：列出终止原因到动作的完整表：自然 idle、用户 steer、硬中断、task replaced、模型错误、hook stop、预算耗尽、存储失败、会话 shutdown。由同一个生命周期所有者执行 `close admission → signal operations → bounded settle/classify → persist terminal state → emit external terminal event`。模型 generation 与执行 epoch 分离，迟到模型响应可丢弃，已经执行的操作结果不能因模型 generation 变化而被静默丢弃。明确强制取消后的未知副作用怎样记录。

为了长期 rebase，推荐先把独立 crate 定义为**纯状态转移/决策器**，由宿主 task 作为唯一执行协调者；不要同时让 crate 的 actor 和 `run_turn` 各自拥有终止权。双层状态机并非天然错误，但二者共享可结束的 task 状态而无仲裁契约是错误边界。

### F3 · P1：一个权威日志不等于可恢复读取；runtime checkpoint 必须与 compaction/fork 的读取边界绑定

**分类：源码事实已证实；存储扩展设计缺少必要协议。**

方案位置：第 134–142、154 行。方案已诚实说明扩展槽不存在、必须核对分页和 fork，并没有错误地声称 fsync 已有；问题在于其主契约仍停留在“扩展记录 + durable append”，没有定义从哪里开始恢复。

源码证据：

- `codex-rs/rollout/src/model_context.rs:18–47`：最新带 replacement_history/window_number 的 Compacted 记录被视为完整边界，逆向扫描在此停止。
- `codex-rs/thread-store/src/local/model_context.rs:167–185`：实际本地历史读取使用这个扫描并停止读取更早 segment。
- `codex-rs/core/src/session/mod.rs:4135–4155`：现有 checkpoint 保存替换历史、guardian/retained context、MCP origins、窗口及恢复元信息，没有通用 runtime checkpoint。
- `codex-rs/thread-store/src/live_thread.rs:153–178`：继承模型上下文按过滤后的持久化项计算 inherited ordinal。新增扩展项会影响 fork 前缀边界，不能只在 codec 中解释。
- `codex-rs/history/src/rollout_payload.rs:24–63` 与 `codex-rs/rollout/src/policy.rs:12`：记录的反序列化和过滤均是明确枚举分支，opaque payload 仍需进入这些路径。

具体失败场景：操作 O 在 checkpoint 前提交，压缩后仍运行；恢复只加载最新模型上下文后缀，看不到 O 的创建记录。即便其 running 回执留在摘要中，也不能据此恢复权限快照、执行句柄、交付游标和执行权。若 O 的完成恰在压缩边界两侧，独立追加 runtime 快照还可能与历史 checkpoint 不一致。fork 若直接复制扩展记录，可能把父执行权带入子线程；简单过滤又会丢掉解释继承 pending 回执的状态。

建议修正：在设计阶段明确以下二选一，不能延至工具覆盖阶段才决定：

1. runtime checkpoint 与 history checkpoint 共享一致 cut/提交单位，包含仍活跃的操作、已提交未消费的事件、codec 版本、执行 epoch 与游标；读取最新上下文时同时获得它。
2. runtime 通过单独、显式的全日志/索引读取契约重建，模型历史仍读有界后缀；明确这条读取的性能上限、fork 截断点和恢复一致性。

推荐第一种作为长期目标，但它会触及 checkpoint 生产与读取路径，应计入真实上游修改面。fork 应产生子线程自己的继承状态/无执行权标记，并定义针对任意历史 cut 的处理，而非仅对当前全历史 fork 做一次清理。该问题不能仅由追加日志的 fsync 保证解决。

### F4 · P1：全量“没有未完成 operation 才结束 turn”会与后台服务、交互进程的正常生命周期冲突

**分类：现有后台行为已证实；新方案的资源分类未说明，不是已实现 bug。**

方案位置：第 69、115、169 行。方案正确指出 tool future 返回不代表进程结束，但随后把外部 completed 的条件写成“无归属它的未完成操作”，又没有把运行任务与长期资源分开。

源码证据：

- `codex-rs/core/src/unified_exec/process_manager.rs:594–615`：进程在初次 yield 等待前进入 process manager，注释明确指出中断 turn 不应因为最后一个 Arc 被释放而杀死后台进程。
- 同文件 `:718–724`：工具响应可以携带仍存活的 process_id。
- `codex-rs/core/src/unified_exec/async_watcher.rs:155–180,224–240`：独立 exit watcher 等到真实退出后再发 ExecCommandEnd，并保留原始 turn/call 关联。

具体冲突：用户要求启动开发服务器并给出访问方式，模型启动 `npm run dev` 后完成交付。若 operation 活跃期必须直到 process exit，这个用户 turn 永不结束。若为满足 idle 自动杀掉进程，又破坏用户需要的服务。若在 yield 时无条件将 operation 完成，则对普通长任务又退回现有轮询式行为。

建议修正：至少区分 call（一次工具交互）、job（有确定完成条件、阻塞当前目标完成）、resource（可跨用户 turn 存活的服务/交互会话）。明确后台化的可见状态转移及执行权归属，规定 resource 退出如何通知当前会话，避免启动伪造的新用户请求。取消策略按种类定义：用户停止推理是否停止其已启动服务，是产品契约，不应由 Arc 生命周期偶然决定。会话级并发锁也应绑定工具临界段/明确 operation 语义，不能把“保留互斥”解释成让整个长期后台进程永远持锁。

首版可以只支持有限、非交互的受控任务，并明确不接管服务资源；这不阻碍原型，但不能把同一完成规则直接推广到阶段 3。

### F5 · P2：实现 ContextualUserFragment 不会自动保留工具结果的历史分类和来源属性

**分类：源码事实已证实；兼容表达尚需实测，不断言 provider 会拒绝。**

方案位置：第 119–130 行。一次 tool output + 后续结构化通知避免重复 call ID，作为候选 wire 表达合理；但“实现 fragment trait”只是构造消息，并不是全部上下文兼容性。

源码证据：

- `codex-rs/context-fragments/src/fragment.rs:35–50,64–105`：trait 构造带 content kind 的消息，默认是文本；角色由实现自行选择。
- `codex-rs/core/src/context/contextual_user_message.rs:22–37,84–95`：是否为上下文片段依赖显式 matcher 列表；新 trait 实现不会自动加入。
- `codex-rs/core/src/context_manager/history.rs:1280–1289`：user-role 消息若未识别为 contextual fragment，会被当作新 user turn 边界。`:931–935` 的截断也依赖这个分类。
- `codex-rs/core/src/context/contextual_user_message.rs:51–81`：用户授权检测另用 content-kind 元数据，和上述文本 matcher 不是同一机制。
- `codex-rs/core/src/session/turn.rs:2455–2465`：原结果先经过外部上下文污染标记，再写 annotated history；`codex-rs/core/src/tools/parallel.rs:113–119` 还向结果附加已交付消息和工具调用记录信息。

失败场景：新的 user-role 完成通知有正确非 user content kind，却没注册 contextual matcher；授权检测可能仍正确，但 turn 边界/压缩/截断按另一套规则把它当作真实用户输入。若转换时只保留结果字符串，也会丢失原有 metadata 和来源关联。多模态输出不能由默认文本 fragment 自动获得等价表达。

建议修正：给 wire codec 加明确的消息角色、content kind、matcher、call/operation/source ID 和 envelope metadata 映射；先在原始工具结果上执行既有来源检查，再投影。把“不会形成用户授权”“不会形成用户 turn 边界”“不破坏上下文截断”“多模态内容保持类型”列为不同断言。首版可仅开放文本结果，不应以此宣称多模态已覆盖。新增片段需设真实硬上限；依 AGENTS.md，任何可能超过 1K tokens 的单项必须单独人工审核，不能只写笼统的“有上限”。

### F6 · P2：当前快慢 Shell 验收可能被 native 模式通过，不能证明切换了等待屏障

**分类：验收设计不足；可通过确定性原型立即验证。**

方案位置：第 164–173 行，尤其阶段 2 的“A=1s、B=60s，C 在 B 结束前启动”。

源码证据：

- `codex-rs/core/src/tools/handlers/unified_exec/exec_command.rs:142–143` 允许并行。
- `codex-rs/core/src/unified_exec/process_manager.rs:631–636,718–724`：exec_command 可以 yield 并返回 still-alive process_id；native 的 drain 收齐的是工具 future，不是所有进程退出。
- `codex-rs/core/tests/suite/tool_parallelism.rs:226–298` 已测试 native 工具输出按调用顺序分组；`:304` 开始的测试已验证模型响应未结束时工具就能启动。

失败场景：B 在一秒 yield 返回 process_id，native 立即收齐本批工具结果，随后也能发起 C。新增调度器通过上述时间断言，却没有证明“B 的工具调用本身尚未返回时，A 已推动新推理”。同时只对比旧上游基线能证明补丁可移植，不能证明未来维护成本低。

建议修正：用受门闩控制的 mock tool，令 B 的 **handler future** 保持 pending，禁止自身 yield，直到观察到 C 的启动事件才释放。native 必须无法到达 C，新路径必须可以。另设一项独立测试验证 yield 后的有限 job 完成通知。维护成本测量不只记录冲突文件数，还记录每次移植需要重新证明的语义契约、适配修改行数、修改的上游函数数；先前基线演练是代理指标，需明确这个局限。

## 最高优先级的三个改动

1. **把接入单位从“工具 future 调度”升级为“有序提交边界”。** 定义模型/工具/hook 事件进入上下文的单一顺序，以及 compaction 的 cut + suffix 保留。先解决 F1，才有资格声称宿主压缩可复用。
2. **明确唯一 turn 生命周期所有者，并拆开 job 与 resource。** 用状态转移表覆盖所有退出原因、tool handle/token/step 快照持有、迟到结果归属和完成事件发布顺序。独立 crate 先做纯决策器，宿主负责 I/O 和终止协调；不让两个 actor 都结束同一个 turn。
3. **把 runtime checkpoint 与历史读取边界纳入存储原型。** 先验证“活跃操作跨压缩后崩溃再恢复”和“历史 cut fork 不继承执行权”，再讨论 opaque 扩展槽是否足够小。

## 替代边界及取舍

| 方案 | 可以满足的目标 | 主要代价/限制 | 审核建议 |
| --- | --- | --- | --- |
| 独立纯决策器 + 宿主唯一协调者 | 异步推进、明确生命周期、共享上游宿主逻辑 | 仍需历史提交门、退出仲裁及 checkpoint 接入；适配层未必薄 | 当前优先验证 |
| 提取上游 host step/effect 服务，由新协调循环调用 | 更接近 Unreal 单一事件循环所有权；不复制业务实现 | 初次提取面较大，必须维护上游调用契约；代码移动也有 rebase 成本 | 若前者需要遍布 turn.rs 的模式分支，应改选此方向；不要为“不改循环”牺牲清晰所有权 |
| 复用 exec yield + 后台通知，局限有限工具 | 最快证明模型能利用异步通知；较少调度改动 | 不提供通用跨-step pending tool、通用意图提交和恢复机制 | 可作收益对照或短期产品实验，不能命名为完整机制迁移 |
| Go sidecar / MCP 包装 | 可以复用部分参考运行时 | 权限、上下文和外层 Codex 调度成为双层协议；不自动消除等待屏障 | 无新的证据支持替换当前 Rust 方向 |

“必须保留 `run_turn` 函数原形”不应成为不可挑战的目标。应保留的是一份宿主业务实现。若真正干净的边界需要提取 reusable step/effect 服务，接受一次可审查的重构，可能比在每个新上游退出路径都补模式判断更容易长期维护。

## 最小可证伪验证方案

这不是先造完整内核再接线的开发计划。先写一个小型、默认关闭的纵向原型，使用 mock provider 和受控工具；真实代码量必须由 diff 度量，不预先保证 500 行。按 AGENTS.md 将复杂改动拆成可审查阶段，每个阶段有行为测试。

| 实验 | 控制交错 | 通过条件 / 可推翻的主张 |
| --- | --- | --- |
| 1. 真正跨-step 工具 | A 完成，B 的 future 被门闩挂起，模型据 A 启动 C | C 启动时 B future 仍 pending；native 对照到不了 C；无新信息不额外请求模型；每 call 恰有一次 wire output |
| 2. 压缩竞态 | 在远程 compaction 取快照后挂起响应，放行 B 的 PostToolUse 附加上下文，再完成压缩 | B 结果和 hook 内容都在正确后缀且仅一次；实时与恢复的模型输入一致。若丢失，推翻“只替换调度边界即可复用压缩” |
| 3. 非正常退出 | B 挂起时分别触发不可重试模型错误、hook stop、硬中断、替换 turn | 有界收尾；外部 terminal event 唯一；不得向新 turn 写旧结果；未知副作用保守标记；不重复启动 |
| 4. checkpoint + crash + fork | O 在压缩前启动，在压缩后 crash；分别在未完成/完成未交付处恢复，另从该 cut fork | 恢复拿到 O、游标和正确权限/执行 epoch；父操作不在子线程执行；子线程不永等父操作；正常有界历史读取也通过 |
| 5. job/resource | 一个有限长任务，一个长期服务，一个需要 write_stdin 的交互进程 | 有限任务完成可唤醒；服务可以保留而 turn 正常结束；write_stdin 不被长期锁饿死；退出通知和取消语义可预测 |
| 6. 投影分类 | running 回执→文本完成通知→压缩/截断/resume；注入类似用户指令的工具文本 | 无第二 tool output、无新用户授权、无伪 user turn；原结果来源 metadata 不丢失；片段硬上限可验证 |

复用现有测试工具和风格：`core/tests/suite/tool_parallelism.rs` 的受控并发场景；`core/tests/suite/hooks.rs:1321` 的 Stop hook 续跑场景；`core/tests/suite/interrupt_hooks.rs:137` 的中断事件顺序；`core/tests/suite/compact_resume_fork.rs:198` 的实时/resume/fork 模型历史一致性。新增 agent 行为必须写 core 集成测试；有界测试优先用 gate/oneshot 或测试时间控制，不靠真实 60 秒 sleep。

完成上述原型后再移植到两个具有相关语义变化的已知上游提交，分别记录文本冲突和需要修改的行为契约。若历史/退出逻辑只能靠散落在上游多处的 feature 分支维持，应判定现有接入边界失败，并比较“提取宿主 step 服务”的真实 diff，而不是继续把补丁称为薄适配。

## 不应误判为缺陷的部分

- 方案已经明确任意 Shell/MCP 不可盲目重放，也没有承诺外部副作用 exactly-once；这部分无需重复提出空泛警告。
- 保留 Arc<StepContext> 可以解决同一进程内跨 step 的快照持有；它不等于 durable recovery，方案已经承认。需要补的是执行权、终止及权限生命周期，而不是仅指出 Arc 不可序列化。
- 参考项目自身的 fork FIXME（`research/unreal-agent/harness/coordinator/loop.go:552`）不证明移植必然失败；但它也不能充当本方案 fork 正确性的现成依据。
- Unreal 单一协调器的结构可参考（`harness/coordinator/loop.go:190–209,261–274`），不能把它的完整状态所有权拆成两个控制层后仍默认继承其正确性。
- 未运行 provider 实验，因此本审核没有断言完成通知编码一定被 API 拒绝或一定降低模型质量。这些属于必须测量的兼容性与行为假设。
