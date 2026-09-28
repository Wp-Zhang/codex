# H7 model-context review

Reviewed by the coordinating agent after independent implementation, 2026-09-28. This is the additional review required for newly injected items that can exceed 1k tokens; it is not a claim of human approval.

- Hook context and hook execution decisions use the existing `HookAdditionalContext` typed fragment. Maximum 16 fragments per emission; each serialized native message is capped at 4,096 bytes, including JSON escaping. The bound is below 10k tokens even at one token per byte. Oversized input is rejected intact rather than silently changing a hook decision. The decision includes the call identity and effective rewritten arguments; the Prepared descriptor retains the original model intent. The native hook trust/role is unchanged.
- Client additional context uses the existing typed native fragment construction. Each prepared fragment is capped at 8,192 serialized bytes before candidate installation. It is not a growing operation manifest or transcript dump. The joint record has its separate 256 KiB delta bound.
- Result pages remain capped by the smaller of 8,192 serialized bytes and the native model history budget. A returned continuation offset describes only returned characters. Completion notices remain <=768 bytes. Large results and UI display truncation do not enlarge the model fragment.
- Accepted input, hook effects, and control answers enter model history after the existing joint commit ACK. No new untyped system-message injection or separate runtime authority was introduced. Native compaction bounds continuing prompt growth; archived result lookup does not inject an archive index.

The larger fragments are intentional only for explicitly requested result pages or existing hook/client context. They are not counted as free token/cost improvements in the comparison report.
