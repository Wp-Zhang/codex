# Panng runtime prototype

This crate isolates decision and joint-checkpoint experiments from Codex internals.
It does not execute tools, own a model connection, or implement Codex rollout storage.
The host must persist a candidate state before publishing it or executing effects.

Run from the repository with `just test -p codex-panng-runtime`.
The core mock-session experiment is selected by `just test -p codex-core --lib --test all -E 'test(panng_)'`.

The core experiment is off by default and only intercepts a host-provided `panng_probe`
test tool. It is not available through CLI/configuration and does not support durable
resume. See `research/PROTOTYPE-RESULTS.md` for passing experiments, reproduced host
limitations, and the gates that remain before real tool execution is appropriate.
