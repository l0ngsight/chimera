# Publishing Chimera

Plan for releasing Chimera on crates.io, plus prebuilt binaries, so people can run
`cargo install <crate>` or `cargo binstall <crate>` and get a `chimera` binary.

crates.io only hosts source: `cargo install` compiles on the user's machine. Prebuilt binaries are
attached to GitHub Releases. `cargo binstall` finds them there via metadata in `Cargo.toml` and
falls back to compiling when it can't.

## Current state

| Item | Status |
|---|---|
| License (MIT), description, repository, keywords, categories, readme | done (`Cargo.toml`, `LICENSE`) |
| `include` list: sources, web assets, sample teams, docs | done; 33 files, 148 KiB compressed |
| `cargo package --locked` builds from the packaged sources | done, checked in CI |
| Every dependency comes from crates.io (no path or git dependencies) | done |
| Minimum Rust version declared (`rust-version = "1.88"`) and tested in CI | done |
| CI: fmt, clippy, tests on Linux and macOS, Rust 1.88 | done (`.github/workflows/ci.yml`) |
| Crate name | **blocked: `chimera` is taken on crates.io** |

## 1. Decisions to make first

1. **Crate name.** `chimera` belongs to an unrelated crate ("Unified async Rust SDK for AI coding
   agents"). Free as of 2026-10-18: `chimera-agents`, `chimera-orchestrator`, `agent-chimera`.
   Rename only the package; keep the command as `chimera` with a `[[bin]]` section:
   ```toml
   [package]
   name = "chimera-agents"

   [[bin]]
   name = "chimera"
   path = "src/main.rs"
   ```
   Users then run `cargo install chimera-agents` and get `chimera`.
2. **Sample teams in a public release.** `teams/scb-software-engineering.yaml` names SCB and its
   approved Copilot model set. Before publishing, decide whether that belongs in a public crate.
   Options: publish it as is, rename it to a generic "Copilot-only enterprise team", or drop it from
   `SAMPLE_TEAMS` and `include`.
3. **Copyright holder** in `LICENSE`. It currently reads "l0ngsight"; change it if the work should be
   credited to a person or an organisation.
4. **Supported platforms.** Linux and macOS are tested. Windows is untested: agents and tests rely
   on `sh`, and the process guard has no Windows backend. Either document "Linux and macOS only" or
   add a Windows build job and fix what breaks.
5. **Version.** Start at `0.1.0`, since the API and the config format may still change. Breaking
   changes then bump the minor version (`0.2.0`).

## 2. One-time setup

1. **crates.io account.** Sign in at crates.io with the GitHub account that will own the crate and
   verify an email address (publishing requires one).
2. **First publish, by hand.** Trusted publishing can only be configured for a crate that already
   exists, so the first version is published from a laptop:
   ```sh
   cargo login            # token from crates.io → Account Settings → API Tokens (scope: publish-new)
   cargo publish --dry-run
   cargo publish
   ```
   Then delete that token.
3. **Trusted publishing** (no long-lived token in GitHub). On crates.io, open the crate's
   Settings → Trusted Publishing and add a GitHub publisher: repository `l0ngsight/chimera`, workflow
   `release.yml`, environment `release`. In GitHub, create the `release` environment and require a
   reviewer for it, so no publish happens without a human approval.
4. **Co-owners.** `cargo owner --add github:<org>:<team>` or a second maintainer, so the crate is not
   tied to one account.
5. **Branch protection** on `main`: require the CI checks and a pull request.

## 3. Release automation

Add `.github/workflows/release.yml`, triggered by pushing a tag `v*`:

1. **Check:** run the CI jobs on the tag, and check that the tag matches the version in `Cargo.toml`.
2. **Build binaries** for:
   - `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl`: static, so they run on any distro.
     This works because reqwest already uses rustls rather than OpenSSL.
   - `x86_64-apple-darwin` and `aarch64-apple-darwin`.

   Archive each as `chimera-<target>.tar.gz` with a SHA-256 checksum, and upload them to the GitHub
   Release. Tools that do this:
   - `dist` (formerly cargo-dist): generates the whole workflow plus shell installers and a
     Homebrew formula.
   - `taiki-e/upload-rust-binary-action`: one step per target if you'd rather keep the workflow
     hand-written.
3. **Publish to crates.io** in the `release` environment: authenticate with
   `rust-lang/crates-io-auth-action` (OIDC trusted publishing), then run `cargo publish --locked`.
4. **cargo-binstall metadata** in `Cargo.toml`, so `cargo binstall` downloads the release archive:
   ```toml
   [package.metadata.binstall]
   pkg-url = "{ repo }/releases/download/v{ version }/chimera-{ target }{ archive-suffix }"
   bin-dir = "chimera{ binary-ext }"
   pkg-fmt = "tgz"
   ```

For version bumps and changelogs, `release-plz` opens a "release" PR with the version bump and the
`CHANGELOG.md` entry generated from commits; merging it tags the release. `cargo-release` does the
same from a laptop.

## 4. Before the first release

- [ ] Decide the crate name and rename the package (decision 1); update the README install section.
- [ ] Decide what happens to the SCB sample team (decision 2).
- [ ] Add a "Install" section to the README: `cargo install <crate>`, `cargo binstall <crate>`,
      release downloads, and the prerequisites (the agent CLIs themselves, optional `bwrap` on Linux).
- [ ] Add `CHANGELOG.md` (Keep a Changelog format).
- [ ] Add `cargo deny check` (licences, advisories, duplicate crates) or `cargo audit` to CI.
- [ ] Run `cargo publish --dry-run` and check the README renders on crates.io (relative links
      are rewritten to point at the repository).
- [ ] Smoke-test each prebuilt binary: `chimera --version`, start the server, open the UI, run a dry-run
      session.
- [ ] Publish `0.1.0`, then confirm `cargo install <crate>` works on a clean machine.

## Notes

- Publishing is permanent: a version can be yanked (hidden from new resolutions) but never deleted
  or overwritten. Everything in the package is public, so check `cargo package --list` before each
  release.
- The crate size limit on crates.io is 10 MB; Chimera's package is about 150 KiB.
- Bumping `rust-version` is a breaking change for anyone on the old compiler: do it in a minor
  release and note it in the changelog.
