# A0 / A1a.1 定向验证记录

| 检查 | 结果 |
| --- | --- |
| runtime | 9/9 |
| rollout + thread-store | 修正 fixture 后 405/405 |
| core/app-server 定向回归 | 首次 34/35，另有 4 flaky |
| 并行工具及附件串行复查 | 9/9，包含上次失败项 |
| 最后一次 Panng 回归 | 11/11，包含 2 项新增并发故障用例 |
| 定向 Clippy | 通过 |
| just fmt | 通过 |

原始首次失败及复测日志保留在本目录的 .log 文件中，仅作为本地调试证据，不纳入源码补丁。最后的 lint/format 后未重复运行测试。测试使用 mock provider，不代表真实模型账单或质量评估。
