# 扩大工作区测试记录

日期：2026-09-28；原型初始版本。测试开始后开发继续，因此该结果不是最新迭代的验证替代。

`just test --workspace --exclude codex-voice-host`：20,722 执行，20,623 通过（11 flaky），71 失败，28 超时，60 跳过；nextest 执行耗时 3277.199 秒，另有冷编译耗时。

完整 `just test` 先因 voice-host 本机系统依赖不全未能编译；最终运行排除此包。不能称为完整工作区通过。V8 使用仓库 MODULE.bazel 中指定的 macOS arm64 预编译产物及 bindings。

未在无改动上游基线复跑，不能断言下列失败均为上游缺陷。原型定向用例通过；全量中出现 shell/daemon 超时、UI 快照、终端模拟及 V8 feature 配置等失败，本轮保留证据，不自动接受无关快照。

| 测试 binary | 失败 | 超时 |
| --- | ---: | ---: |
| codex-app-server::all | 5 | 0 |
| codex-cli::app_server_daemon | 0 | 8 |
| codex-cli::daemon_startup | 0 | 1 |
| codex-cli::doctor_path_safety | 0 | 2 |
| codex-core | 1 | 1 |
| codex-core::all | 5 | 0 |
| codex-exec-server | 18 | 0 |
| codex-exec-server::accepted_websocket | 1 | 0 |
| codex-exec::all | 3 | 1 |
| codex-network-proxy | 22 | 0 |
| codex-tui | 15 | 15 |
| codex-v8-poc | 1 | 0 |

完整失败名称与 flaky 清单见 [summary.txt](summary.txt)；原始日志压缩保存在 run.log.gz。本次生成的 .snap.new 已移至 snapshots/ 保留原路径，未接受为新基准。
