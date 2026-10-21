# Changelog

All notable changes to Chimera are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/) (while below 1.0, breaking changes bump the minor version).

## [Unreleased]

### Fixed
- A persona whose runtime does not exist no longer leaves the session "running" forever; its turn fails
  with "unknown runtime". Saving a team or starting a session with an unknown runtime is now rejected.
- A turn that crashes is finished as a failed turn instead of leaving the persona stuck.
- A process that escaped the turn's process group (e.g. via `setsid`) and kept stderr open no longer keeps
  the turn running.
- Messages are no longer lost when the server stops mid-turn: the interrupted turn's messages go back to the
  persona's inbox, and messages in flight are saved so they are restored after a crash.
- Usage-limit messages with absurd reset times ("try again in 99999999999999999 days") no longer crash the turn.

### Security
- Updated rustls, rustls-webpki and webpki-root-certs to fix six advisories reported by `cargo audit`.

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
