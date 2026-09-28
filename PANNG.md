# Codex Panng

Panng keeps Codex's TUI and native command, patch, approval and hook implementations. Its execution pool survives individual model steps: one completed command can advance the conversation while another is still running. Results remain retrievable after context compaction and session recovery.

## Run the macOS arm64 distribution

Extract the complete package, then run:

```sh
./bin/codex-panng -m gpt-5.5
```

The launcher uses your existing Codex login and configuration. Use `-m` to select another model supported by your account. Resume with `./bin/codex-panng -m gpt-5.5 resume`; interrupt with Escape, then continue with another message. `/fork` creates an independent conversation from a frozen history position.

The Panng launcher selects the new execution mode and disables Code Mode. The unwrapped `bin/codex` remains the native entrypoint. New Panng sessions use their own history mode; existing native sessions are not rewritten.

The shipped Panng tool set is finite shell commands, patches, retained-result reads, plan progress and user questions. This release's binary target is macOS Apple Silicon. The strict journal implementation requires a Unix host.

## Build and package

From `codex-rs`, build the CLI and its native helper:

```sh
cargo build -p codex-cli -p codex-code-mode-host --bin codex --bin codex-code-mode-host
```

Use `scripts/build_panng_distribution.py --help` to package the prebuilt binaries and bundled patched zsh through the upstream package assembler. It writes the launcher, build manifest, checksums and archive. Runtime resources must stay beside the binaries.

Verification scripts are `scripts/panng_package_smoke.py`, `scripts/panng_tui_smoke.py`, `scripts/panng_live_smoke.py --tui` and `scripts/panng_compare.py`. The comparison measures real command scheduling and request bytes with a deterministic provider; it does not claim provider billing or model-quality improvements.
