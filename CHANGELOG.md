# Changelog

All notable changes to Chimera are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/) (while below 1.0, breaking changes bump the minor version).

## [Unreleased]

### Security
- The HTTP API checks every request: the `Host` header must name this server (stops DNS rebinding), and
  requests that change something are refused when they come from a page of another origin (stops cross-site
  request forgery, e.g. a web page cancelling sessions). Before, any web page could cancel your sessions.
- Listening on a non-loopback address (e.g. `--bind 0.0.0.0:7878`) now requires an access token, generated
  into `data/token` and printed as a URL to open; it can be set with `auth_token` or `CHIMERA_TOKEN`. The TUI
  takes `--token`.
- The settings page can only name `*_API_KEY` variables as API keys, so it can no longer send an arbitrary
  environment variable to a chosen URL. Jev only falls back to `OPENROUTER_API_KEY`/`TYPESAFE_API_KEY` when
  talking to that provider.
- The Linux process guard also removes `DBUS_SESSION_BUS_ADDRESS`, `XDG_RUNTIME_DIR`, `TMUX`, `TMUX_PANE` and
  `STY`, so agents cannot act on your desktop session or terminal multiplexer by default. The docs now say
  what the guard and the deny lists do and do not cover.
- Updated rustls, rustls-webpki and webpki-root-certs to fix six advisories reported by `cargo audit`.

### Changed
- A runtime in `chimera.yaml` with a built-in name is merged key by key over the built-in definition instead
  of replacing it, so overriding `models` no longer drops `--no-chrome` and the deny list.
- `prompt_llm` keys in `chimera.yaml` are all optional.

### Fixed
- Live session views no longer miss events: each event carries its sequence number as the SSE id, a
  reconnecting browser (or TUI) only receives what it missed, and a client that falls behind the server's
  buffer gets the missed events re-sent instead of silently losing them. The end of the history replay is
  signalled by the server (`replay_done`) instead of a 400 ms timer, and the session page shows
  "reconnecting…" while the connection is down. `/api/sessions/{id}/events` takes `?after=`.
- A message sent while a session is winding down (after `<final>` or Cancel, while turns still stop) is no
  longer dropped: it is delivered once the session has stopped and starts a new round.
- A persona whose every runtime/model is rate limited now waits for the earliest reset and then continues,
  instead of failing the turn and losing its messages.
- A persona whose runtime does not exist no longer leaves the session "running" forever; its turn fails
  with "unknown runtime". Saving a team or starting a session with an unknown runtime is now rejected.
- A turn that crashes is finished as a failed turn instead of leaving the persona stuck.
- A process that escaped the turn's process group (e.g. via `setsid`) and kept stderr open no longer keeps
  the turn running.
- Messages are no longer lost when the server stops mid-turn: the interrupted turn's messages go back to the
  persona's inbox, and messages in flight are saved so they are restored after a crash.
- Usage-limit messages with absurd reset times ("try again in 99999999999999999 days") no longer crash the turn.

### Removed
- The `tokio-stream` dependency.

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
