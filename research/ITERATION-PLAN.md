# Panng：从原型到可正式使用的迭代验证方案

更新时间：2026-09-28。基线为 `41f9084b30`，开发分支 `panng/prototype`。
依据：`UNREAL-CORE-PROPOSAL.md`、`UNREAL-CORE-ADVERSARIAL-REVIEW.md`、`PROTOTYPE-RESULTS.md` 及当前默认关闭的原型。

用户最新目标：方案 → 子 agent 对抗审核 → 修订 → 开发和定向验证，循环到可正式使用；随后以模拟真实场景比较跨 step 执行的效率与成本。后续不运行完整工作区测试，不因语音、供应商等无关模块失败扩大开发范围。

## 0. 当前事实及不能混淆的结论

- 原型的真实 Codex 路径已验证 pending executor B 不阻止消费 A 并启动 C；native 对照必须先完成 B。中断与不可重试模型错误可以清理测试 executor。
- 真实宿主反例已复现：旧压缩安装覆盖快照后追加的 hook；有界读取在最新 compaction 处停止，拿不到此前活动操作的来源。
- 独立 Machine/Checkpoint 只是契约实验：文本 history、完整快照 frame、无执行端。它不能代替 rollout 接入、提交确认、交付协议或完整资源管理。
- Pool 当前只接受 `panng_probe`，生命周期仅跨一个用户 turn 内的模型 step；尚无稳健 steer、恢复和全出口仲裁。测试中断成功不等于运行回执已经有终态归档。
- 原型还需补正 fork 对 Ready 未交付结果的保留，以及模型请求确实消费 A 输出的断言。不能只看 C 启动时刻。
- 先前获授权的扩大测试仍在收尾；其失败只记账，不继续修复无关模块或据此声称全量通过。

## 1. 正式使用的硬门槛

“可正式使用”必须指一个可通过 CLI/config 开启、实际执行真实工具的 Panng 模式，不得以测试工具通过或默认关闭掩盖缺口。保留 native 兼容路径，明确能力边界；不支持的工具走可解释的 native barrier，不冒充可恢复异步工具。

1. 真实有限 Shell job 和至少一种可控 MCP/扩展工具能跨 step 执行；现有审批、sandbox、hooks、step 权限快照不被绕过。
2. 调用意图与执行身份在分发前获得明确持久化确认。持久化失败不启动新副作用；未知外部结果不自动重放。
3. 工具/hook/输入的历史追加与压缩安装采用同一提交序列。所有支持的压缩入口保留 cut 后缀；不支持的入口必须明确退回安全路径，不能静默丢事件。
4. runtime 与模型上下文 checkpoint 共享一致切点；真实 thread store 的有界 resume 和历史 cut fork 恢复操作及交付状态，fork 不复制执行权，也不丢已完成结果。
5. 正常结束、steer、中断、替换、模型错误、hook stop、预算、存储失败和 shutdown 有明确收尾契约。迟到更新保留事实但不能误投递新 turn；已有 running 回执必须得到可解释的终态/未知状态。
6. call、有限 job、后台 resource 明确区分。后台化必须显式；server/watch 不阻止正常完成，交互 stdin 不被长锁饿死，停止推理不偶然杀掉已移交服务。
7. 一次 call 的 wire output 唯一；后续结果有稳定身份和来源。通知不构成用户授权或用户 turn，遵守单项及总上下文上限。实际 provider 编码与客户端事件兼容性有证据。
8. 新模式通过实际入口的 E2E smoke、相关原生回归和故障注入；至少两轮独立对抗审核无未关闭 P1。只对有证据的支持范围作正式可用结论。
9. 文档包括启用、限制、旧会话策略、出错恢复与安全回退；不能让 native 错读异步活动会话。依赖关系和改动面足以持续跟进上游。

## 2. 迭代 A：先建立真实提交与恢复边界

### 审核后确定的接入方向（2026-09-28）

- 自有 `codex-panng-runtime` 演进为纯决策/版本化 runtime 数据协议，移除原型中仅靠字符串 history 假装完整宿主状态的用法。core adapter 保留真实 ResponseItemEnvelope、来源和 step 信息的编码/投影职责。
- 宿主为异步模式设一个有序历史提交入口：追加、snapshot、checkpoint 安装都在同一顺序上仲裁。不得持历史锁等待模型/工具；写入者取得提交确认不依赖整个工具 future 返回。
- 采用显式、版本化 `PanngCommit` 单行恢复记录，容纳 runtime 转移及可选 history envelope 追加。纯 ResponseItem metadata 不能表示推理休眠时的独立操作完成，因此不采用。联合 checkpoint 必须同时覆盖 runtime、历史与恢复必需的 world-state/settings；不能依赖 Compacted 后多个 companion 记录恰好全部写入。精确数据契约单独记录在 PERSISTENCE-CONTRACT.md，未实现前不宣称恢复已贯通。
- 写入必须有 sync barrier，而非把 `record_canonical_items` 入队或普通 flush 当作 fsync。审计 ThreadStore、LiveThread、RolloutRecorder；不支持该契约的自定义 store 明确拒绝启用，或在分发前选择 native，不在执行后降级。
- checkpoint 在 c 捕获压缩前缀，在 s 安装“压缩 ≤c + 原始 (c,s] 后缀 + runtime@s”。推进水位及模型交付记录同一提交；旧 history epoch 的响应不能覆盖新历史。
- resume 从联合 checkpoint 重放之后事件。执行端不能确认的操作标为 indeterminate；不自动重跑任意 Shell/MCP。回执、结果和已消费确认分开记录。

### 必须按顺序落地的阶段

- **A0：执行准入与原型反例修正。** submit 接收未执行调用或 factory，先校验限额、重复 ID、关闭/取消，再构造执行 future；构造时可能 spawn，不能只验证“未 poll”。撤除向 native/code-mode 递归 runtime 传播的共享 gate，仅受控 leaf probe 接管。修复 fork 丢 Ready 结果并强化真正消费 A 的断言。此阶段不声称 durable。
- **A1：存储协议与同步屏障。** 先实现版本化记录、严格校验及明确的本地 durable append ACK；现有 enqueue/flush、内存 store 和 custom store 不能冒充支持。写入或同步失败冻结新分发，区分未提交与不确定；按 commit ID 仲裁 ACK 丢失，不盲目重试副作用。
- **A2：读取/投影/联合切点。** 审计所有 rollout reader、分页、fork、压缩及 UI 投影。结果持久化与进入上下文分别表示；请求尝试、响应观测只作为诊断，A1不把它们当作消费确认或回收依据，没有本地“恰好一次模型消费”保证。所有恢复必需 companion 数据在同一原子恢复单位。
- **A3：宿主提交与最小 finalizer。** 先 durable ACK，后 live history/raw 事件发布，再构造 executor。失败必须能穿透宿主并阻止后续执行。finalizer 的所有权在可被强杀的 turn task 外，以 turn/generation 防止旧任务结算新任务。不能等到 B 才补错误收尾。
- **B 启用前能力表。** 未验证的 code-mode 组合明确拒绝或保持完全原生 gate，不能让所谓 native barrier 仍继承 pool gate。生产支持需要 leaf gate、不跨父协调调用持锁；exec invocation ready 与进程 job 退出独立，接入现有 watcher 的终态 fan-out。

审核依据：ITERATION-REVIEW-1.md、PERSISTENCE-REVIEW-1.md、LIFECYCLE-REVIEW-1.md。三份均为 request changes；上述为方案处置，不代表问题已由代码修复。

### 验证

- 把现有真实 Session 丢 hook 探针保留为 native 基线，新增异步模式正向对照：压缩暂停时追加 hook 和结果，实时、磁盘恢复后的模型输入一致且各一次。
- 模型请求已发送/被取消/响应丢失、结果已完成未投递、fsync 前后/ACK 丢失、checkpoint 提交后未安装，均有可控故障点。
- 真实 thread-store 的分页读取只从最新完整联合 checkpoint 开始，仍获得旧活动操作；未知版本、坏尾记录、缺失产物引用明确失败。
- fork 从历史 cut 重建；未完成执行权不继承，Ready 结果保留。父后续结果不自动进入子线程。

只有这些验证贯通后，才把真实副作用工具接入新调度路径。

## 3. 迭代 B：生命周期与真实工具

- 明确唯一宿主协调者。独立内核不再启动另一个也能结束 turn 的循环；正常/异常路径统一经幂等 finalizer。
- generation、执行 epoch、history epoch 分离。等待 ready 结果同时监听输入和停止；不能只在模型流阶段响应 steer。
- 运行中的 invocation 持有发起 step 的权限、工具目录、环境快照；恢复重新校验。不把 Arc/closure 当作可持久化描述。
- 优先接入真实有限 `exec_command`：yield 只结束一次交互，不自动等于 job 结束；实际退出通过 executor/watcher 通知，不要求模型轮询。
- 以明确工具参数或已有明确后台语义表达 resource；不按命令文本或运行时长猜测。先完成启动/移交，再从阻塞 job 集合移出。
- 并发互斥在跨 step 范围生效；code-mode 嵌套执行不得重复获取父调用持有的同一独占锁。未验证的 code-mode/特殊控制工具保留 native barrier；审核必须检查该回退是否仍可完成普通工作流，而不是制造死锁。
- 提供可配置、有界的完成合并窗口、队列/并发/输出限额。结果无需立即强行打断正在生成的模型；缺少信息时睡眠，不忙采样。

### 验证

- 除原型 semaphore 对照外，以真实子进程/临时项目运行 A/B/C，断言 C 输入包含 A 的真实结果，启动时 B 尚未结束，最终产物一致。
- 对照完整退出原因表。中断/替换后没有新 turn 的旧结果泄漏、卡住 UI 或无限等待；未知副作用被正确披露。
- 长服务、watch、PTY/stdin、显式停止资源；串行/独占工具、并行工具、code-mode 嵌套调用；MCP 慢响应和连接丢失。
- 权限拒绝、post-tool hook 改写/附加上下文、压缩、预算临界与运行中配置变更相互交错。

## 4. 迭代 C：正式入口、兼容与回归

- 将测试专用启用方式替换为文档化配置/CLI 入口及持久 engine/version。测试工具留在测试层，生产模式不依赖 `panng_probe`。
- 保留现有公开 thread/turn/item 语义；必要协议变化单独生成 schema/快照并验证。未知 store、provider、tool 能力有明确预检或安全回退。
- 旧会话默认 native；异步活动会话不能直接由旧引擎续跑。回退要求无未处理执行权和已投递状态一致，不能丢结果。
- 每轮只运行影响范围：runtime、core 的调度/历史/压缩/恢复/权限/hooks/工具生命周期，必要时 thread-store/history/rollout、app-server 与受影响 TUI 快照。不再跑语音、无关供应商等全工作区测试。
- 依仓库要求做 formatter、定向 Clippy、Cargo/Bazel 锁更新；大改动拆成可审查的阶段，测试与行为一起提交。
- 用真实补丁做至少两个相关上游基线的移植演练，记录文本冲突及语义重证成本；不能把编译通过等同于 rebase 安全。

## 5. 对抗审核与执行循环

每轮先由独立子 agent 按“可具体复现的失败序列”审核方案；反馈分为 P1 阻断、P2 改进和未验证假设。修订后按不重叠所有权派发实现，合并者负责交叉契约、回归和证据记录。每轮完成后再审核实现/结果，未关闭 P1 不进入正式可用判定。

目前对迭代 A 的审核尤其要回答：元数据方案是否能支持独立 operation 更新；持久化确认与历史发布是否真的一致；所有旁路写入如何纳管；联合 checkpoint 读取和 fork 会触及哪些文件；错误后是否会继续执行；相比提取共享宿主服务，原地接入是否更脆弱。

## 6. 最后再做模拟真实场景的效率/成本对比

在功能与恢复门槛通过后执行，不能用 benchmark 掩盖正确性缺口。

| 场景 | 工作内容 | 预期观察而非预设结论 |
| --- | --- | --- |
| 并行检索 + 慢测试 | 多份文件检索、慢测试、依赖检索结果的编辑/验证 | 快结果是否缩短关键路径 |
| 长构建 + 独立产物 | 长构建同时生成文档/检查另一模块 | 是否减少阻塞和显式轮询 |
| 修复 → 验证串行链 | 后续步骤严格依赖前一步 | 不应制造虚假加速；观察额外轮次成本 |
| 大量短工具 | 独立短读任务 | 合并窗口是否增加延迟或降低请求数 |
| 后台服务 + 交互 | 启动服务并执行健康检查/stdin | 正确性、完成条件和资源存活 |
| 压缩/中断/恢复 | 长任务期间触发边界事件 | 正确恢复及额外成本 |

至少比较 native 默认、native 合理使用 yield/poll、Panng 三种策略；使用相同任务、产物断言、模型设置、权限、预算和工具延迟。不能故意禁用 native 的现有异步能力来夸大收益。

用受控模型模拟器和真实工具执行可复现 DAG，明确标注推理延迟为模拟参数。报告真实测得的端到端时延、工具等待、模型请求次数、工具调用/poll 数、实际请求内容的 tokenizer 估计输入 token、可复用前缀及输出规模。mock 的 usage 数字不能当成真实账单；若没有真实 provider 调用，美元成本/真实模型质量不作结论。缓存计费按明确情景计算并与未缓存 token 分开；展示无优势或反向结果。

每种场景重复运行并记录中位数/范围、原始数据和功能结果一致性；基于测量说明收益适用范围。正式可用结论与成本优势结论分别判断。

## 已执行状态（2026-09-28）

- A0 修正已开发，runtime 9/9、core Panng lib 5/5、采样 integration 4/4 通过。
- A1a.1 单记录同步原语已开发：Unix + 现有独占 writer + paginated local store；文件/目录同步后 ACK，uncertain 后 fence；projection failure 不改写 canonical ACK。独立审核 S1/S2 已修复，rollout/thread-store 405/405 通过。
- 旧扩大测试已结束，失败证据见 validation/full-workspace；不继续扩展无关测试。
- 待办仍包括 A1a.2 codec/strict reopen/reconcile、A1b 联合恢复/宿主 commit coordinator、生命周期/真实工具/配置、两轮生产审核及最终模拟真实任务对比。未达到正式可用门槛。
