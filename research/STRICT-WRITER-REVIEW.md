# R2 strict writer I/O review

2026-09-28。范围：recorder/strict.rs、strict_state.rs、recorder.rs 的模式分派与 writer 生命周期。仅静态审查，不执行测试；已在实现者确认源码与测试收尾后完成最终复核。

## 发现并修复：冷打开的锁 namespace 没有绑定实际目标文件

初版 `resume_panng_with_writer_lock` 读取任意传入 path，再验证 guard 与 config.home/header.thread_id。实际可令 config/guard 位于 home A，而 path 指向 home B 的同 thread ID/branch 日志；home B 的合法 guard 仍能独立打开相同文件。两者使用不同锁文件，append 的长度检查无法替代互斥，两次并发检查可同时通过并写入相同 ordinal。

已向父 agent 与 persistence_review 报告。最新实现已增加：

1. canonicalize home 与目标路径，要求目标位于该 home 的 sessions 或 archived_sessions 路径下；路径分量比较，不用字符串前缀。
2. 对最终打开的 journal File 获取独占文件锁，覆盖 hardlink 等不同路径指向同 inode 的情况。
3. 使用该同一 File 的可变借用进行 suffix 恢复，修复与写入继续使用同一个打开文件，不再次按路径打开另一个对象。
4. guard/thread/branch/ordinal 验证全部发生在 set_len 前。

因此普通跨 home 和 symlink 逃逸在打开阶段拒绝；跨 namespace hardlink 由同文件锁互斥。未把非协作外部进程任意篡改视为可由本协议解决的场景。

## 其他已检查路径

未发现以下已落代码有新的具体 ACK/reconcile 缺陷：

- append 先验证模式、pending/poison、文件长度、commit identity/sequence/branch、cold ResumeFence、runtime transition，再保存 exact pending attempt。write/flush/sync 任一步失败保留同一 attempt，返回 Uncertain；publish 只发生在 file sync 后。
- 同一 live writer 的 ACK 丢失可通过最近一次 identity+canonical payload 重取 receipt；same ID/different bytes 拒绝。pending attempt 未处理时后续普通 append 与 flush/persist/shutdown 屏障不能掩盖不确定性。
- reconcile 对 pending ID 与 canonical payload 严格比较，读取实际文件尾，只接受 exact attempted line 的前缀；额外尾、截短前缀或不同字节 poison。完整记录 re-sync 后返回 Durable；不完整记录截到已确认 end，再 sync 后返回 Absent。
- O_APPEND 文件在 reconcile seek/read 后继续写仍追加；恢复读取推进的共享 cursor 不会改变追加位置。
- 原生 record_canonical_items 和 generic append_synced 在 strict mode 前端拒绝，actor 端再次检查；原生构造不能静默打开 marked history。identified append/reconcile 在 native mode 拒绝。
- writer task 持有 guard，调用者取消等待不会提前释放后台写入的 namespace 所有权；最终 File 自身保留内核文件锁。cold reopen 不提供旧实例最后请求的自动幂等重试，需先完成新 epoch ResumeFence，这与当前契约一致。

这些是源码结论，不是新增测试通过声明。实际文件与目录 sync 故障、ACK loss、wrong guard/path、hardlink竞争等测试结果由主 agent 统一执行后补充。


## 最终处置

已复核最终 constructor 与新增测试源码：包含 wrong thread/home/branch 不改 torn bytes、同一 guard 开两个 recorder 拒绝、不同 home/namespace 的 hardlink 同 inode 竞争拒绝、symlink 逃逸拒绝。最初报告的所有权缺陷已在代码层关闭；本轮未发现其他可复现的 I/O、ACK 或 reconcile 阻断项。

主 agent 正在统一运行测试，本报告不抢先声明其结果。真实通知与 runtime 的联合宿主发布不属于此次 strict writer I/O 审核结论。
