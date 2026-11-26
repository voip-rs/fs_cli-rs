# FreeSWITCH CLI Client Project

## Project Overview
Rust-based interactive CLI client for FreeSWITCH using ESL (Event Socket Layer).

## Build System
- Local: `cargo build --release`
- Container: `./build.sh` cross-compiles all targets on Debian Bullseye in one
  pass and extracts them to `dist/` (`fs_cli.amd64`, `fs_cli.arm64`, `fs_cli.exe`)
- CI/CD: `.github/workflows/build.yml` builds multi-arch on push/tag

## Development Commands
- `cargo check --message-format=short` - Fast syntax check
- `cargo clippy --fix --allow-dirty --message-format=short` - Linting
- `cargo test --message-format=short` - Tests

## Git Conventions
- Conventional commits: `feat:`, `fix:`, `build:`, `release:`, etc.
- Version bumps: `release: version X.Y.Z`

## Release Process
Use the `/release` command (`.claude/commands/release.md`).

## Project Structure
- Binary: `fs_cli` (see `Cargo.toml`)
- `src/main.rs` — entry point, connection helpers, error formatting
- `src/batch.rs` — `-x`/`-X` batch loop, log streaming
- `src/jobs.rs` — bgapi jobs: submit, Job-UUID correlation, result printing
- `src/session.rs` — interactive session loop, event consumer, reconnection
- `src/readline.rs` — readline thread, function keys, CompletionRequest
- `src/commands.rs` — command execution and formatting
- `src/console_complete.rs` — FreeSWITCH console_complete API
- `src/completion.rs` — tab completion (rustyline helper)
- `src/channel_info.rs` — UUID completion from channel list
- `src/log_display.rs` — log event formatting and display
- `src/originate_check.rs` — what the switch installs for a typed `originate`, and the opt-in rewrite
- `src/config.rs` + `src/args.rs` — YAML profiles, CLI args (see `fs_cli.yaml`)
- `src/legacy_config.rs` — C fs_cli `fs_cli.conf` reader, batch-relevant keys only
- `src/esl_debug.rs` — client-side debug levels (0-7)
- Multi-arch releases: Linux AMD64/ARM64, Windows AMD64
- Release naming: `fs_cli_${version}_{amd64|arm64}.debian-compatible`, `fs_cli_${version}_amd64.windows.exe`

## Configuration (v0.2+)
- YAML config with profiles: see `fs_cli.yaml` for example
- No embedded example file: a missing config is created by serializing `ProfileConfig::default()`
- Usage: `fs_cli [profile]`, `--config path`, `--list-profiles`
- Locations: `~/.config/fs_cli.yaml`, `~/.fs_cli.yaml`, `/etc/freeswitch/fs_cli.yaml`,
  then legacy `~/.fs_cli_conf`, `/etc/fs_cli.conf` (warns, batch keys only)

## Dependencies
- `freeswitch-esl-tokio`, `rustyline` (git deps)
- `tokio`, `serde_yaml`, `crossterm`
