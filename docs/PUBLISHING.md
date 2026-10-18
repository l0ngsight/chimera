# Publishing Chimera

Chimera is published on crates.io as [`chimera-ai`](https://crates.io/crates/chimera-ai). It installs a
`chimera` command (`chimera` was already taken on crates.io).

- `cargo install chimera-ai` compiles from source. crates.io only hosts the source code.
- Prebuilt binaries are attached to each GitHub Release by `.github/workflows/release.yml`.
  `cargo binstall chimera-ai` finds them through the `[package.metadata.binstall]` section in `Cargo.toml`.

## Status

| Item | Status |
|---|---|
| `0.1.0` published on crates.io | done 2026-10-18, by hand; installs and runs (`cargo install chimera-ai --locked`) |
| Package metadata, MIT license, `include` list, Rust 1.88 minimum | done |
| CI: fmt, clippy, tests on Linux and macOS and Rust 1.88, `cargo package` | done (`.github/workflows/ci.yml`) |
| Release workflow: binaries, GitHub Release, crates.io trusted publishing | done (`.github/workflows/release.yml`); first run with `0.1.1` |
| README Install section, `CHANGELOG.md`, `cargo binstall` metadata | done |
| One-time GitHub and crates.io settings | **to do, see below** |
| Dependency licence and advisory check (`cargo deny` or `cargo audit`) in CI | to do |
| Windows | not supported (agents and tests rely on `sh`; the process guard has no Windows backend) |

## One-time setup (clicks)

Do these in order. Each one takes a minute or two.

### 1. Make `main` the default branch (GitHub)

The repository was created with the `claude/…` working branch as its default.

1. Open https://github.com/l0ngsight/chimera/settings (repository **Settings → General**).
2. Under **Default branch**, click the **⇄** (switch to another branch) button.
3. Choose **main**, click **Update**, and confirm with **I understand, update the default branch**.

### 2. Create the `release` environment with an approval gate (GitHub)

The publish job runs in this environment, so nothing reaches crates.io until you approve it.

1. **Settings → Environments → New environment**. Name it exactly `release` and click **Configure environment**.
2. Tick **Required reviewers** and add yourself (`kiryl-martsinkevich`). Leave **Prevent self-review**
   unticked, or you could never approve your own release.
3. Under **Deployment branches and tags**, choose **Selected branches and tags**, click
   **Add deployment branch or tag rule**, pick **Ref type: Tag**, enter `v*`, and click **Add rule**.
4. Click **Save protection rules**.

### 3. Turn on trusted publishing (crates.io)

This lets the release workflow publish without any API token stored in GitHub.

1. Open https://crates.io/crates/chimera-ai while signed in, and open the crate's **Settings** tab.
2. Under **Trusted Publishing**, click **Add** and choose **GitHub**.
3. Fill in:
   - Repository owner: `l0ngsight`
   - Repository name: `chimera`
   - Workflow filename: `release.yml`
   - Environment: `release`
4. Save.

### 4. Revoke the token used for the first publish (crates.io)

1. https://crates.io/settings/tokens (**Account Settings → API Tokens**).
2. **Revoke** the token you created for `cargo publish`.
3. On your machine, run `cargo logout` so the token is also removed from `~/.cargo/credentials.toml`.

### 5. Protect `main` (GitHub)

1. **Settings → Rules → Rulesets → New ruleset → New branch ruleset**.
2. Name it `main`. Set **Enforcement status** to **Active**.
3. **Target branches → Add target → Include default branch**. This needs step 1 done first; otherwise
   use **Include by pattern** with `main`.
4. Tick:
   - **Restrict deletions**
   - **Block force pushes**
   - **Require a pull request before merging**. Set required approvals to `0` while you are the only
     maintainer.
   - **Require status checks to pass**. Click **Add checks** and add the five CI jobs:
     `Format and clippy`, `Test (ubuntu-latest, stable)`, `Test (macos-latest, stable)`,
     `Test (ubuntu-latest, 1.88)` and `Package for crates.io`.
5. Click **Create**.

### Optional

- **Co-owner on crates.io**, so the crate is not tied to one account: `cargo owner --add <github-user>`.
- **Delete the old working branch** `claude/chimera-multi-agent-orchestration-rjvjmh` once `main` is the
  default and nothing else needs it.

## Making a release

1. In a PR:
   - bump `version` in `Cargo.toml`;
   - run `cargo build` so `Cargo.lock` picks up the new version;
   - in `CHANGELOG.md`, rename `## [Unreleased]` to `## [X.Y.Z] - date` and add an empty `[Unreleased]`
     section above it.

   Merge it once CI is green.
2. Tag the merge commit and push the tag:
   ```sh
   git checkout main && git pull
   git tag vX.Y.Z
   git push origin vX.Y.Z
   ```
3. The **Release** workflow then runs:
   - it checks that the tag matches `Cargo.toml` and runs the tests;
   - it builds the four binaries and creates the GitHub Release, with the changelog section as release notes;
   - it then waits for you.
4. Open the run under **Actions → Release**, click **Review deployments**, tick `release` and click
   **Approve and deploy**. The crate is published to crates.io. If that version is already there, this step
   is skipped.

To check that the binaries build without releasing anything, run the workflow by hand: **Actions → Release →
Run workflow**. Without a tag it only checks and builds.

## Notes

- Publishing is permanent: a version can be yanked (hidden from new installs) but never deleted or
  overwritten. Everything in the package is public, so check `cargo package --list` before each release.
- Bumping `rust-version` is a breaking change for anyone on the old compiler: do it in a minor release
  and note it in the changelog.
- The crate size limit on crates.io is 10 MB; Chimera's package is about 150 KiB.
