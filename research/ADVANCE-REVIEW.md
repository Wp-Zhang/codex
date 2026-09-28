# Pool::advance 拆锁修正：独立复核

日期：2026-09-28。工作区：`/Users/wpzhang/.codex/worktrees/panng-prototype/Codex-Panng`。
范围：`core/src/panng/mod.rs` 的 advance/stop/admission 改动及新 `advance_tests.rs`。在实现 agent 通知完成后读取。只写报告，不改源码、不跑测试；Clippy 和定向测试结果由主 agent 统一确认。

## 结论与修复处置

**当前结论：A1、A2 两项 P1 均已静态确认修复，关闭实现发现；定向测试及 Clippy 结果待主 agent 统一确认。** 未发现此次小改动仍存在新的实现阻断。以下保留两轮发现与修正证据，不将静态检查描述为测试通过。

父 agent 已明确不接受以原型限制解释此次新增回归。修正后的 `mod.rs:195–205` 先取消 token，再 await advancement permit，最后才获取 state guard 关闭/清理；任何执行中的 advance 必须先结束或取消并释放 permit，stop 才能返回。因此 no-late-publication 的进程内边界由同一 permit 保护；工具等待通过 pool token 被唤醒，不形成持 state 锁等待工具的循环。

`advance_tests.rs:38–50` 已改用 tokio::join! 并发 poll stop 和先前进入 pending 的 advance，并在 stop 返回后检查 executor 已 Drop、permit 已释放及 closed pool 未复活。该测试验证等待/收尾交错；尚未单独模拟实际 history publication 的暂停点，相关 fence 结论来自同一 permit 覆盖整段 advance 的源码证明。建议日后以提交服务可控 gate 添加 publication 专项测试，不把当前测试描述成已覆盖磁盘 publication 故障。

本审核只要求恢复进程内的 stop 完成边界，不要求本次实现持久化 finalizer。完成/ack 状态先于历史写入的旧原型局限仍然存在，不应被此拆锁修正包装为已解决。

以下保留初次发现和当时行号，便于审查修复原因。

## A2 · P1：同一 ready 批次的后续 Fatal 会吞掉先前已标 Delivered 的成功结果

补充复核版本：`mod.rs:145–174`。问题已由源码确认，未运行测试。

当前代码先持 state 锁遍历整个 ready 批次，将成功项移出 pending、complete/deliver 并放入局部 completed Vec；遍历结束后才在锁外写历史。若顺序为 A 成功、B 返回 Fatal，`:155` 的 `result?` 立即返回；局部 completed 中的 A 被丢弃，`:174` 的 publication 循环从未开始。A 在 Machine 中已 Delivered，也已从 pending 删除，且其 future 已被消费，不会在下一次 advance 自动出现。后续 pending 查找或状态转换的 `?` 失败也有相同的成功前缀丢失风险。

**这是本次批量转换引入的语义回归，不是仅限未来持久化的缺口。** 原逐项循环在处理 B 之前已经把 A 写入 Session 历史；新代码改变了可观察结果。即使错误随后结束 turn，已执行完成且原本可以记录的 A 事实也不应被新局部 Vec 吞掉。

**优先修法：** 在初始短临界区检查取消并归还尚未 ready 的 futures；随后逐项处理 ready。对每一项，用短 state 临界区取出 pending 并完成该项转换，释放 guard，立即进行该项锁外 publication，然后处理下一项。遇 B Fatal 返回时，A 已完成 publication。只有整个 ready 序列成功后，才收集/标记并发布剩余 pending 的 running ACK；错误路径不生成新 pending ACK。保留 advancement permit 覆盖整个过程，以及 stop 的 cancel→等待 permit→close 顺序。

备选是捕获 deferred error，先 publication 成功前缀再返回错误，但要同样避免提前标记未发布 ACK、处理状态转换错误及中途编码失败。逐项版本更接近旧语义，分支更少。

**最小确定性复现：** A executor 在首次 poll 中向 oneshot 发送信号并立即返回成功 envelope；B executor 先等待该信号，再返回 `CodexErr::Fatal`。这样 B 无法先于 A ready，不依赖 FuturesUnordered 的默认遍历顺序。可再加入永远 pending 的 C。调用一次 advance，断言返回 B 的 Fatal，同时 canonical history 恰含一次 A 的真实输出；没有给 C 新增 running ACK。调用 stop 后不再新增 A/B/C 消息，也没有未释放 future。该测试能够直接区分旧逐项语义与当前错误批处理。

### A2 修复复核 disposition

后续实现已采用逐项方式：`mod.rs:145–151` 仅短锁归还剩余 futures；`:154–172` 每项读取结果、短锁转换状态、释放锁后立即完成 publication。只有整个循环成功退出才在 `:174–196` 收集/发布 pending ACK。因此 B 的 Fatal 不再绕过 A 的 publication，其他后续项的查找/转换错误也不能吞掉已发布的成功前缀。`stop` 仍在 `:201–211` 先 cancel、等待同一 advancement permit、再 close，前一轮修复未被撤销。**A2 实现 P1 关闭。**

新 `advance_tests.rs:65–102` 使用 B 等 oneshot、A 发信号并返回真实结果、另 C pending，验证 Fatal 和仅 A 输出的完整序列，控制顺序合理。但初读版本将未准备的原始 output（id=None、metadata=None）直接作为 expected；Session 正常记录会先 stamp turn ID、分配缺失 ID（`session/mod.rs:3444–3449`），因此 fixture 需预置稳定身份/turn 或使用宿主 preparation 构造 expected。已通知实现者与主 agent；此项为测试 fixture 核对，不是逐项 publication 仍然错误。最终测试结果另行填写，本 reviewer 未运行。

## A1 · P1：stop 返回后仍可能出现旧 turn 的历史写入

被审版本：`mod.rs:174–190,195–201`。

advance 在 state 锁外逐项 publication；在 `:175` 检查取消后，随后 await `mark_thread_memory_mode_polluted_if_external_context`，再调用 `record_annotated_conversation_items`。running acknowledgements 同样检查一次后进入异步记录。stop 只 cancel token、拿 state 锁关闭 machine/清空集合，随即返回，不等待正在进行的 publication。

具体交错：advance 通过取消检查 → 在 mark_external 或 history record 内部 await 暂停 → stop 设置取消、关闭池并返回 → advance 恢复并把旧 turn 内容写入 Session。旧实现虽有不应持锁 await 的问题，但 state guard 覆盖 publication，stop 返回前会等这段执行结束；本次移出锁造成了行为变化。

再次在某个 await 后检查 token 只能缩短窗口，无法证明 stop 返回后没有写入。取消已进入的记录 future 也不一定能回滚其已发生的内存/磁盘 mutation。

**建议最小修正：** stop 先取消 pool token，随后等待唯一 advancement permit，取得后再关闭 state。等待期间不持 state MutexGuard；挂起工具的 advance 通过 pool cancellation 唤醒并释放 permit；正在写入历史的 advance 可结束当前 publication，stop 只在它退出后返回。新 submit 因 cancellation 已设置而拒绝。这个方案利用已有 semaphore，不增加第二个状态机，也不声称 durable/有界磁盘故障处理。

验证必须让 advance 与 stop 真正并发 poll。现有测试把 advance pin 在栈上，然后单独 await stop；若 stop 改为等待 permit，测试应改成 spawned coordinator 或 join/select 同时推进，而不能把未被 poll 的 future 当作独立运行任务。

## 已确认的正确边界

- `mod.rs:125–135`：先通过 try_acquire 获取独占 advancement permit，再在短 state 临界区取走 futures；后续 select 不持 state guard。并发第二次 advance 明确返回错误，不会把临时空 state.futures 当成 idle。
- `:136–150`：等待同时观察 turn 与 pool cancellation；获得结果后再次拿 state 锁并检查取消，再归还 futures。若 stop 已完成关闭，取消已设置，归还分支不会复活 closed pool。
- stop 在取 state 锁前先设置 cancellation。因此即使 stop 在 advance 的检查后开始，而 advance 已拿 state 锁，它也只能完成这个短临界区；stop 后续拿锁会清理，不存在 stop 完成后再把 futures 归还的持续状态复活。
- `:103–116`：submit 在 state 临界区检查取消与 advancement ownership，拒绝等待期重入后不构造 executor。若 advance 恰在 submit 已通过准入后取得 permit，只会等该短临界区结束再接管已加入的 futures，未发现 overwrite 丢新 future 的路径。
- `advance_tests.rs:23–49` 用 executor Notify 证明真正 poll 到 pending 后才调用 stop；它覆盖等待期解锁、第二次 advance 拒绝、pool cancellation 唤醒、future 被 Drop、closed state 未复活。它尚未覆盖 publication 期间 stop 的交错。

## 明确保留的原型局限

1. take 到局部的 futures 在 advance 被取消/强制 drop 时会随调用销毁，而 state.pending/machine 可能仍暂时包含记录。因此拥有该 coordinator 的宿主必须随后调用 stop/finalizer，不能丢弃 advance 后继续把同一个 Pool 当正常运行池使用。当前有限 probe 阶段可保留这个前置条件，后续持久化协调者必须显式接管遗留状态。
2. `mod.rs:156–169` 在实际历史记录前移除 pending、标 Delivered/acknowledged；record 失败或取消会造成事实与发布不同步。这是已知、未持久化原型问题，本次不要求将其扩展成完整事务，但不得宣称交付原子性已经成立。
3. 使用 advancement semaphore 跨 await 是有意的唯一协调权，不等于持有被 Clippy 禁止的 state MutexGuard；前提是 stop/工具内部不要求取得同一 permit 才能解除 advance 的工具等待。建议的 stop 流程先 cancel 再等待，正是为避免这个环。

## 最小验证

- 保留现有 pending executor + 并发 advance + stop 测试，调整为独立并发 poll。
- 增加或用可控现有记录依赖构造 publication 暂停点；stop 未返回前允许在途 publication 收尾，stop 返回后不得再追加旧结果/回执。
- 验证 stop 之后 submit factory 构造数为 0，advance 不再恢复运行状态。
- 统一执行 core 的 panng 定向测试与相同 Clippy 检查；本 reviewer 未运行，不能将静态通过称为 lint 已通过。

## 主 agent 验证回填

- 使用宿主 prepare API 为 fixture 建立稳定 ID/turn metadata 后，`just test -p codex-core -p codex-app-server --lib --test all --test-threads 2 -E 'test(panng_)'`：11/11 通过。
- 包括等待中 stop、并发 advance 拒绝、A 成功/B Fatal 时保留 A 历史且不发新 ACK，以及原有跨 step/中断/模型错误集成测试。
- 首轮 fixture 的预期遗漏宿主元数据造成 1 个失败，已保留日志；源码修正未放宽结果比较。
