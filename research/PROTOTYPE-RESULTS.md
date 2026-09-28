# Panng 执行内核：第一轮原型验证

日期：2026-09-28。上游基线：`41f9084b30812db321a0b592def4f500d1e79cf4`。
代码分支：`panng/prototype`；工作区：`/Users/wpzhang/.codex/worktrees/panng-prototype/Codex-Panng`。

## 结论

**跨模型 step 的工具执行在真实 Codex 采样路径上已跑通；完整阶段 0 尚未通过。**

原型使用宿主显式注册的 `PanngPrototype` contributor，仅接管 `panng_probe` 测试工具。CLI/config 不注册它，默认行为保持原生路径。该测试工具使用内存 semaphore 挂起真正的 executor future，没有 Shell yield、后台进程或外部副作用。

26 项 core 定向测试通过，其中 4 项为新的端到端调度测试，3 项为通知分类及架构探针，其余为现有并发/上下文回归；独立 runtime 的 8 项测试也通过。**探针通过表示成功复现宿主缺口，不表示缺口被修复。**

## 可复现命令

在上述工作区运行。此机器使用已安装的 Command Line Tools，未代用户接受 Xcode 许可：

```sh
export DEVELOPER_DIR=/Library/Developer/CommandLineTools
export PATH="$HOME/.cargo/bin:$HOME/.local/bin:$PATH"
just test -p codex-panng-runtime
just test -p codex-core --lib --test all \
  -E 'test(panng_) | test(tool_parallelism::) | test(contextual_user_message::)'
```

测试使用 mock provider，不调用真实模型。Rust 版本为仓库指定的 1.95.0。工具链、just、cargo-nextest、uv、dotslash 和 Bazelisk 已安装到用户可写位置；Homebrew 因 Xcode 许可检查失败而未用于安装。

## 实验结果

| 设计实验 | 本轮证据 | 状态 |
| --- | --- | --- |
| 1. 真正跨 step 工具 | B 的 executor 在 semaphore 上保持 pending；异步路径的 C 在 B 被放行之前启动，native 路径必须先放行 B。后续输入只有一个 B 的 function_call_output，另有完成通知 | 真实 Codex 路径通过 |
| 2. 压缩期间追加 | Machine 模型保留 cut 后的 hook 后缀；真实 Session 的既有 replace_compacted_history 仍删除快照后写入的 hook 内容 | **宿主未通过；反例已复现** |
| 3. 所有终止路径 | 真实 Codex 的用户中断、不可重试模型错误均清理挂起 executor；纯状态机的 closing/closed 转移幂等并把未知 job 标为 indeterminate | 部分通过；强制 task abort、hook stop、预算、替换、存储故障及 shutdown 未完成宿主验证 |
| 4. checkpoint/crash/fork | 独立联合 frame 的每个截断位置均回退到前一个完整快照；文件 sync 后可恢复未投递结果；纯模型 fork 不继承执行权。真实 ModelContextScan 在压缩处停止，看不到前置操作来源 | **独立契约通过，宿主恢复未通过** |
| 5. job/resource | 纯模型区分两者，资源不阻止 idle，交互任务仍需完成 | 仅状态契约；未接入真实服务、PTY 或 detach |
| 6. 通知语义 | 新 ContextualUserFragment 已注册 matcher；不是用户授权或 user turn 边界；真实请求不重复 call output，通知超过 768 字节 body 拒绝 | 部分通过；真实 provider、多模态和压缩后的全部来源属性仍待验证 |

### 为什么两个反例改变下一步工作

压缩探针直接使用真实 `Session`：记录历史 → 捕获 snapshot → 经正常记录入口写 hook → 安装旧 snapshot 对应的压缩历史。安装前 hook 存在，安装后消失。测试以确定性交错验证安装函数的语义，没有声称执行了完整远程压缩网络流程。

恢复探针直接使用真实 `ModelContextScan`：按逆序读取“操作调用来源 + 最新压缩记录”，扫描在后者停止，前者未被读取。独立 snapshot codec 能保留操作，不等于它已经接上 Codex 的分页/恢复读取。

因此不能把“运行时 crate 的测试通过”解释成历史提交门或联合 rollout 格式已经实现。继续支持真实工具之前，必须把有序追加、压缩安装及联合 checkpoint 读取贯通。

## 代码边界

- `codex-rs/panng-runtime`：自有纯状态转移，以及用于故障注入的联合快照 frame。**不是 Codex rollout 格式**；codec 的 write 不声称完成 fsync，提交函数要求宿主提供持久化确认。
- `core/src/panng`：实验任务池与 lifecycle 注册，跨 sampling 持有 executor futures，并复用原有工具执行链。池的有效期目前是一个用户 turn，覆盖多个模型 step；尚非可恢复的会话执行器。
- `core/src/context/panng_completion.rs`：有字节硬上限的文本通知；注册到原有 contextual matcher，保留原 envelope metadata。
- 原有源码接入集中于工具调用完成项、采样后等待及工具并发锁。宿主没有复制 `run_turn`，也没有批量改写已有工具。
- `core/tests/suite/panng_prototype.rs`：native/async 对照、中断和模型错误；`core/src/panng/probes_tests.rs`：两个宿主反例与通知分类验证。

当前 prototype Pool 只使用 Machine 的操作状态转移；常规模型流及 task 终止仍由宿主执行。它尚未实现统一的全出口仲裁、有序历史提交或持久执行权，不能称为完整设计的实现。

## 明确限制

1. 不提供真实命令执行、持久 resume、执行端重新连接或副作用 exactly-once。测试工具无副作用，因此可以实验性直接分发；真实工具必须先有持久化提交屏障。
2. pending 回执写入现有历史，但 Pool 本身不持久化。不要把实验会话当作可恢复任务；CLI 和配置中没有开启入口。
3. 尚未接入 1 秒合并窗口、完整预算/回压、resource 生命周期和所有中断原因。完成项只做即时就绪收集，测试因此允许 3 或 4 次模型请求。
4. 快照 frame 检查长度、JSON、版本、提交尾标记及历史水位，用来验证截断写入。它不提供追加后尾部修复、文件锁、位腐败校验、磁盘满后的执行端核对等生产存储能力。
5. 本轮未做性能承诺和历史基线移植演练。宿主两个关键边界未贯通前，文本冲突计数不能证明长期维护成本。
6. Pool 等待目前只监听工具完成和 turn 取消，尚未接入完整的 steer/heartbeat 唤醒。中断/模型错误用例证明挂起 executor 被清理，不证明已发送 running 回执之后的终态归档、UI 收尾或后续 turn 上下文都已完成。

## 后续最小实施单位

先做一个共享的“历史提交与 checkpoint 安装”服务：使工具/hook 的追加进入同一顺序，压缩安装保留 cut 后缀，写入与读取联合 runtime checkpoint。将本轮两个反例补上异步模式的正向对照，保留 native 基线。

随后把所有 task 退出原因经同一 finalizer 仲裁，再扩展真实 job/resource。若这些工作需要在多个上游分支复制同一判断，按设计文档改为提取共享 step/effect 服务，而非继续扩张任务池补丁。

## 验证记录

- `just test -p codex-panng-runtime`：8 passed。
- core 定向过滤测试：26 passed，4806 个未匹配测试被过滤；未把过滤测试算作通过。
- `just bazel-lock-update`：执行成功，`MODULE.bazel.lock` 无内容变化；存在上游依赖版本/annotation 警告。
- 扩大工作区测试已结束：`just test --workspace --exclude codex-voice-host` 共 20,722 项执行，20,623 通过、71 失败、28 超时，另有 60 跳过。此前完整命令被 voice-host 缺失系统依赖阻断。无干净基线对照，不将失败统一归因于上游；详见 [验证记录](validation/full-workspace/README.md)，未接受无关快照。
- `just fix -p codex-panng-runtime -p codex-rollout -p codex-thread-store -p codex-core -p codex-app-server` 最终通过；`just fmt` 通过。保留两处上游原有测试导入，未混入无关自动清理。按仓库要求，最后的 lint/format 后未重跑测试。

## A0 审核后的补正

- fork 仅剥离 Pending 执行权，Ready 未交付结果在子分支保留；父子可以独立交付。runtime 定向 9/9。
- 执行准入接收 factory，取消、closing、重复、限额等校验通过后才构造。七种拒绝条件均验证构造计数为零；这是内存准入，仍不是 durable-before-dispatch。
- native runtime 不再继承池的跨 step gate，只有直接 probe 接管；不声称真实混合工具已实现全局互斥。
- 真实采样测试现在验证第二请求完整携带 A 的真实结果、最终响应/消息 ID 唯一，以及模型错误终态对应实际 Error。4/4 集成测试通过。
- core Panng lib 定向 5/5，包含两个新准入测试和三个原有探针。两组 core 共 9 项新逻辑测试通过；未复跑全量。
- 下一阶段实施与审核记录见 ITERATION-PLAN.md、PERSISTENCE-CONTRACT.md 和 PERSISTENCE-CONTRACT-REVIEW.md。正式使用门槛尚未达到。

### 最后一次并发收尾修正

Clippy 发现 advance 持有状态锁跨 await；已改为短临界区，单 permit 拒绝并发 advance。stop 先取消并等待在途 publication 结束，再关闭状态；逐项发布保留 A 成功/B Fatal 前缀，不在错误后发送新 pending ACK。独立复核见 ADVANCE-REVIEW.md；包括两项新增故障回归的 Panng 定向测试 11/11 通过。它仍不等于生产 durable finalizer。
