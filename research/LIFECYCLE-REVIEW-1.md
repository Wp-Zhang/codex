# 生命周期专项对抗审核（第一轮）

2026-09-28；审核对象：ITERATION-PLAN.md、PROTOTYPE-RESULTS.md 与工作区原型。仅源码审核，未运行测试；结论不代表下面反例已执行复现。

## 裁决

方案方向可行，但迭代 B 尚不能直接作为实现规格。至少四个 P1 契约需要先锁定：执行所有者不能随采样 task 被销毁；exec invocation 完成不能冒充 job 完成；跨 step gate 不能套在 code-mode 容器上；等待必须响应输入而不能取消仍有效的执行。建议先发布“同一用户 turn 内、显式有限非交互 job + 已声明能力的 MCP”的真实模式；不宣称 arbitrary Shell/service/PTY/code-mode 都异步。这个范围满足核心跨 step 目标，但只有补齐显式 resource 路径及恢复等既定硬门槛后，才能满足方案自己定义的完整正式可用目标。

## P1-1：finalizer 必须在可强杀的 task 之外，且必须按 turn 身份仲裁

证据：tasks/mod.rs:365 的 spawned task 在 run 返回后调用 on_task_finished；:535 abort_all_tasks 和 :597 finish_turn_abort 是另外两条终止路径；:915 handle_task_abort 取消后超时直接 task.handle.abort，再发 TurnAborted。turn_suspension.rs:85 附近还有独立强杀路径，随后 shutdown_session_runtime、flush、关闭 writer。Panng on_turn_stop/on_turn_abort 仅 pool.stop，不能成为保证所有出口均完成异步持久化的最终屏障。

额外风险：on_task_finished 在 tasks/mod.rs:650 取得当前 active_turn.task，未在这里匹配传入 turn_context.sub_id。当前上游依赖既有取消/等待顺序维持不变量；若新增旧 task 延迟 finalizer、会话级后台任务或替换异步化，不能沿用这个隐含前提。

可复现序列：B 已返回 running 回执；用户替换 turn；B 的后置 hook 等待磁盘；主 task 超时强杀；新 turn 开始；旧 B watcher/hook 晚到。仅 drop futures 不代表 OS 进程已退出，也不代表历史归档或 terminal event 已完成。

建议明确实现：

- Session 持有自有执行登记与 finalization 状态，键为 thread/turn/generation + operation/execution epoch；turn task 只持引用。operation completion 不直接触发 TurnComplete。
- 抽取一个宿主 finalize_turn(turn_id, reason) 服务，幂等转移 Open → Closing → Closed；上述正常、abort 两条入口、suspend/shutdown 都经过它。正常结束只允许没有未结有限 job、没有未归档结果；错误/中断则取消、等待有界确认、未知标 indeterminate，不假造成功或已杀死。
- finalizer 先关闭 admission/旧 step 历史投递权，收集已知事实与 hooks，持久化结算，再发一次 terminal event，最后释放 active turn/启动后续工作。不能持 active_turn/state/history 锁等待进程、hook 或 I/O。
- 强杀对象只能是推理 task；finalizer 由调用方继续 await 或由 session supervisor 持有。Drop 最多做同步取消信号，不能承担 async 持久化保证。writer 故障时停止新副作用，保留失败状态且不把 flush warning 当成功恢复点。
- run_turn 的所有 return/break 不必逐处复制 finalizer；外围包装收敛错误原因。保留 turn.rs:619 compaction error、:635 session-start-hook stop、:682 stop hook、:689 legacy hook、:777 TurnAborted、invalid-image/model-error 等区别，传递明确 CloseReason，避免全部 Ok(None) 丢失关键信息。预算路径 abort_turn_if_active 同样纳管。

## P1-2：exec_command future 返回不代表进程终止；现有事件可以复用，但不是恢复契约

证据：unified_exec/process_manager.rs:718–839 返回 Alive → Some(process_id) 的工具结果；:1238 注册 spawn_exit_watcher。unified_exec/async_watcher.rs:165 后 watcher 等 exit_token、output_drained、network_denial_monitor，再取 interaction_lock 并发出 ExecCommandEnd。process.rs:450 后 exec-server 输出任务订阅 ExecProcessEvent；检测序号缺口时 read(last_seq) 补齐，处理 Exited/Closed/Failed。流关闭仅说明连接失败，不能当远端进程终止证据。

不要解析模型可见文本来取 process_id，更不要把所有 exec_command 加进 Pool 后见 future ready 就 machine.complete。必须建立 typed 结果关联：invocation_id → environment/executor identity + process_id + execution epoch，注册持久化后才能分发；process_id 不是跨重启稳定唯一身份。

具体路径：

1. invocation 经过原 ToolCallRuntime/registry/ToolOrchestrator，原样保留权限、sandbox、step 环境和 hooks。yield 结束 invocation，最多写一个 wire output；有限 Job 保持 Running。
2. 在 manager 注册进程时提供内部 completion subscription（watch/replayable terminal value），复用已完成网络拒绝归因与输出尾部收集后的 watcher 终态。不要让两个消费者竞争 output_drained Notify 或破坏 interaction_lock 顺序；由唯一 watcher fan-out 完成事实。
3. job 退出后从有界 retained transcript 生成完成结果/产物引用，持久化后向模型追加通知。stdout 消费 write_stdin 与 watcher 不可抢同一可消耗 buffer；必须明确 snapshot 与已交付字节水位。通知不能直接塞整段大输出。
4. 快进程可能在 invocation 输出之前退出：登记结果容纳这个顺序，以 wire output 是否已提交决定直接最终输出或 running+completion，绝不重复 call output。
5. MCP 断线、exec-server stream closed、terminate_confirmed 失败都保留 Unknown/Indeterminate，不能“取消 future 即取消副作用”。现有远端事件恢复可解决连接内丢包，不足以声称客户端重启后的 durable reattach；未实现身份重连验证时 resume 标未知且不自动重跑。

最小公开工具边界建议：显式 operation kind 参数/工具，不按命令文本、tty 或超时时长猜测。有限非交互 Job；交互/服务是 Resource，启动成功且移交记录持久化后不阻塞 turn。若首版只提供有限 Job，长服务需明确调用现有 native resource 路径，文档说明其无新引擎 durable reattach 保证。不能默认所有命令 finite，否则 npm dev 阻塞 turn；也不能所有 yield 都 resource，否则慢测试尚未结束就宣布完成。

后置 hook 的语义也需锁定：现有 post-tool hook 对 invocation 执行一次，yield 的输出就是它看到的结果；job 退出通知不是第二次工具调用，不重放 hook。若产品要求退出时 hook，需单独定义 job completion hook，不能偷偷重复 PostToolUse。

## P1-3：把现有 RwLock 升为共享锁会引入 code-mode 死锁与交互饥饿

证据：tools/parallel.rs:59 从 Pool 取全 turn gate；:201–229 在调用 router 的整个 await 中持读/写锁。tools/code_mode/delegate.rs:124 为子调用构建 ToolCallRuntime，tools/code_mode/mod.rs:402 调用 handle_tool_call_with_source。父容器与子调用因此可能获得同一 gate。

死锁不只限“父写锁→子写锁”：父持读锁，另一个写请求排队，子读请求因 Tokio 公平队列排在写请求之后，也可形成父等子、子等写、写等父闭环。简单给子调用改 supports_parallel=true 不能解决。

推荐：容器不持执行 gate，只有实际 leaf invocation 获取 gate；code-mode 的执行上下文携带明确 Container/Leaf admission 类别，并由调度器验证容器本身不执行未受控副作用。不要使用全局可绕过的 reentrant bool。若要继承父 permit，必须定义所有权/子任务生命周期和兄弟写入互斥，复杂度高于先保持 native。

首版安全回退：在任何 code-mode 容器启动前，先结算已有异步 finite jobs，再执行该容器的原生路径，并使其所有子调用使用原来的局部门，不误接 Pool gate；容器自身也不再持 Pool 的独占 gate。容器/单元可跨 turn 生存时必须禁用该组合或显式沿用 native 整个 turn，不能只在 tools/parallel 临时判断父名字。

锁的持有期限必须明确：invocation gate 保护 handler 运行，不自动覆盖 yield 后 OS process 的整个生命；若宣称 job 排他直到退出，write_stdin/terminate 必须走同一资源的控制通道，不再等待此 job 持有的全局独占锁，否则自锁。跨 step job 默认并发仅限宿主认可的工具/明确 capability；MCP readOnlyHint 不是可信安全保证。

适合 native barrier：code-mode 容器及其 children、request_user_input/审批交互控制、权限修改、context/turn 控制、未声明并行语义的扩展工具、多模态不支持结果。barrier 的意思是先处理异步任务完成并允许 steer/abort，再进入原逻辑；不能在 holding gate 时等待 pool 清空。明确可能降低吞吐，仍不影响有限命令和 MCP 的核心收益验证。

## P1-4：Pool.advance 当前阻塞 steer；持锁等待也阻碍外部仲裁

证据：panng/mod.rs advance 持 state Mutex 并只 select cancellation 与 futures.next；还在锁内 record history。session/turn.rs:3127 在采样尾调用 advance，因此 queued user steer 不能抵达下一轮 input drain。用户“改参数/停止等待 B、先处理 X”会一直等 B。Pool.stop 用自己的 cancellation，而 advance 监听传入的另一 token；单独调用 stop 时可等待 state 锁，无法自行唤醒 advance。

具体接入：利用 session/input_queue.rs:116 subscribe_activity（先订阅再检查队列），从 turn_state_for_sub_id 获取对应队列，select completion receiver / activity.changed / turn cancellation / execution failure / 合并窗口。返回枚举 Ready、InputReady、Closing，而不是仅 bool。InputReady 回到 run_turn 顶部现有 pending input/hook/step activation 流程；旧 job 保留发起 step 权限快照，steer 不自动取消 job。Mailbox/被动通知与真正用户输入要保留原分类，不将任意 activity 当用户授权。

Pool 的 future 持有与消费应移到 executor supervisor，向 bounded completion queue 投递；主协调者不在 state lock 内等待 future，也不在该锁内写历史。等待第一结果前即可提交必要的 running 回执，否则所有工具均长跑时输入唤醒后仍可能存在缺失 call output。没有有用结果也没有输入时休眠，不能靠无限 next sampling 实现“响应”。

## 迟到写入与压缩接入清单

- registry.rs:719–737 的 PostToolUse 会直接 record_additional_contexts；hook_runtime.rs:721–745 接入 Session 历史入口。Pool 只收最终 envelope 无法拦截这些先发生的写入。
- 异步 hook drain、用户输入 hook、extension_tools.rs:452 直接记历史；session run_turn stop/legacy hook 也能产生历史。
- unified_exec watcher 的 UI end/output 事件持旧 turn Arc，可晚于新 turn；它们必须保留原 turn/call identity。UI 事件和模型历史不是同一种提交，不能因为已有 ExecCommandEnd 就认为模型见到结果。
- 对所有 Session canonical/annotated append 入口统一附 generation/source/sequence，进入同一 history commit 服务；普通上游调用至少能从 TurnContext 取得所属 turn。旧 generation 的事实可进入 operation journal，不能自动投给新 turn；恢复或下一轮需显式投影“前次任务结果”，不是伪造当前用户消息。
- manual/auto/post-turn/remote compaction、suspension、fork 的边界由提交门统一检查。不要仅在 Pool.advance 前后加 mutex：工具 hook 在其外执行，仍会在快照后丢失。

## 首轮应新增的定向失败测试

1. pending B + steer，无放行 B 也必须看到下一模型请求消费用户输入；同样覆盖 mailbox，并验证没有重复 running 回执。
2. B 的 hook 延迟、replace + 强杀交错，旧 hook 不污染新 turn；两种完成竞争仅一个 terminal event，已发 running 回执归档为完成/取消/未知之一。
3. 真实 exec yield 后 A→C 仍能前进，B job 保持未完成直到真实退出；快退出在 receipt 前后的两个次序，以及 remote 断线不标成功。
4. PTY 等 stdin、控制调用能获取资源，不被 job 的锁阻塞；显式 service 移交后 turn 能结束且服务仍活着。
5. code-mode 父持读/写、并发 writer 排队、子调用三方交错；native 回退也必须无死锁。不能只测无异步任务的 code-mode。
6. Stop hook、auto compaction error、模型 fatal、预算 abort、suspend、shutdown 各在 B pending 时触发；存储故障后无新副作用，terminal UI 与恢复结果一致。

建议更新 ITERATION-PLAN 的顺序为：提交/身份协议 → supervisor 与唯一 finalizer → input-aware wait → typed process completion → 真实有限 job/MCP → resource/control → code-mode 兼容边界 → 故障矩阵。小步保留 native 对照；benchmark 最终必须包含 native 合理 yield/poll，才能区分真正调度优势与已有后台能力。
