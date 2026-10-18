# Changelog

All notable changes to Chimera are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/) (while below 1.0, breaking changes bump the minor version).

## [Unreleased]

## [0.1.1] - 2026-10-18

### Added
- Release workflow: tagging `vX.Y.Z` builds static Linux (x86_64, ARM) and macOS (Intel, Apple Silicon)
  binaries, attaches them to a GitHub Release and publishes the crate with crates.io trusted publishing.
- `cargo binstall chimera-ai` support.
- Install section in the README.

### Changed
- Release binaries are stripped of debug symbols.

## [0.1.0] - 2026-10-18

First release, published as `chimera-ai` (the command is `chimera`).

### Added
- Teams of expert personas running on Claude Code, Codex, opencode, GitHub Copilot CLI, pi and Hermes, with
  messages routed along a communication graph you design in the web UI.
- Web UI with live session graphs and per-persona logs, and a terminal UI (`chimera tui`).
- Persona prompt generation through a configurable LLM API (DeepSeek by default).
- Optional quick decisions with Jev: acknowledgement filtering, recipient repair, missed-`<final>` detection,
  auto-routing of follow-ups and an opt-in effort advisor.
- Per-persona fallbacks to another runtime, model and effort when a CLI hits a usage or rate limit.
- Process guard (bubblewrap/unshare on Linux, sandbox-exec on macOS) so agents cannot stop the user's programs.
- Sample teams: Software Development Team and Copilot Engineering Team.

[Unreleased]: https://github.com/l0ngsight/chimera/compare/v0.1.1...HEAD
[0.1.1]: https://github.com/l0ngsight/chimera/releases/tag/v0.1.1
[0.1.0]: https://crates.io/crates/chimera-ai/0.1.0
