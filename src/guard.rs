//! Process guard: runs agent CLIs inside an OS sandbox that stops them from killing or quitting
//! programs they did not start (the user's browser, editors, the Chimera server).
//!
//! Prompt rules and per-CLI deny lists are advisory or partial; an agent can still run
//! `pkill chrome`, `lsof -ti:PORT | xargs kill` or a script that sends signals. The guard enforces
//! it at the OS level for every runtime.
//!
//! macOS: `sandbox-exec` with a profile that allows everything except sending signals outside the
//! sandbox and sending Apple Events (AppleScript `quit`). Processes the agent starts inherit the
//! sandbox, so agents can still stop their own dev servers. The profile is verified on the actual
//! machine at startup; if the check fails the guard reports why and stays off.

use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::Serialize;

pub const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// Seatbelt profile. Later rules win: signals are denied, then re-allowed within the sandbox.
const SIGNALS: &str = "(version 1)\n\
(allow default)\n\
(deny signal)\n\
(allow signal (target same-sandbox))\n";

/// Also blocks Apple Events (AppleScript `tell application "Google Chrome" to quit`).
pub const PROFILE: &str = concat!(
    "(version 1)\n(allow default)\n(deny signal)\n(allow signal (target same-sandbox))\n",
    "(deny appleevent-send)\n"
);

/// Profiles tried in order by the self-test; the first that passes is used.
const CANDIDATES: &[(&str, &str)] =
    &[(PROFILE, "signals and Apple Events blocked"), (SIGNALS, "signals blocked (Apple Events rule unsupported)")];

#[derive(Debug, Clone, Serialize)]
pub struct GuardStatus {
    /// The sandbox works on this machine (self-test passed).
    pub available: bool,
    /// The user wants it on.
    pub enabled: bool,
    /// Human-readable result of the self-test.
    pub detail: String,
}

pub struct Guard {
    enabled: AtomicBool,
    available: AtomicBool,
    detail: RwLock<String>,
    profile: RwLock<String>,
    program: String,
}

impl Guard {
    pub fn new(enabled: bool) -> Self {
        Self::with_program(enabled, SANDBOX_EXEC)
    }

    fn with_program(enabled: bool, program: &str) -> Self {
        Self {
            enabled: AtomicBool::new(enabled),
            available: AtomicBool::new(false),
            detail: RwLock::new("not checked yet".into()),
            profile: RwLock::new(PROFILE.to_string()),
            program: program.to_string(),
        }
    }

    pub fn set_enabled(&self, on: bool) {
        self.enabled.store(on, Ordering::Relaxed);
    }

    pub fn status(&self) -> GuardStatus {
        GuardStatus {
            available: self.available.load(Ordering::Relaxed),
            enabled: self.enabled.load(Ordering::Relaxed),
            detail: self.detail.read().unwrap_or_else(|e| e.into_inner()).clone(),
        }
    }

    /// Program and leading arguments to run a guarded command, when the guard is active.
    pub fn wrapper(&self) -> Option<(String, Vec<String>)> {
        (self.enabled.load(Ordering::Relaxed) && self.available.load(Ordering::Relaxed)).then(|| {
            (
                self.program.clone(),
                vec!["-p".to_string(), self.profile.read().unwrap_or_else(|e| e.into_inner()).clone()],
            )
        })
    }

    fn set(&self, available: bool, detail: impl Into<String>) {
        self.available.store(available, Ordering::Relaxed);
        *self.detail.write().unwrap_or_else(|e| e.into_inner()) = detail.into();
    }

    /// Checks on this machine that the sandbox (1) starts, (2) blocks signals to an outside
    /// process (this server), and (3) still lets the agent stop processes it started itself.
    pub async fn self_test(&self) {
        if !cfg!(target_os = "macos") {
            self.set(
                false,
                "Only available on macOS; on other systems run Chimera in a container or VM for isolation.",
            );
            return;
        }
        let mut errors = vec![];
        for (profile, what) in CANDIDATES {
            match self.run_checks(profile).await {
                Ok(()) => {
                    *self.profile.write().unwrap_or_else(|e| e.into_inner()) = profile.to_string();
                    self.set(
                        true,
                        format!("verified on this Mac ({what}): agents cannot kill or quit programs outside their sandbox, and can still stop their own processes"),
                    );
                    return;
                }
                Err(e) => errors.push(e),
            }
        }
        let e = errors.join("; ");
        tracing::warn!("process guard unavailable: {e}");
        self.set(false, format!("Self-test failed, so the guard is off: {e}"));
    }

    async fn run_checks(&self, profile: &str) -> Result<(), String> {
        let me = std::process::id();
        let run = |script: String| {
            let program = self.program.clone();
            let profile = profile.to_string();
            async move {
                tokio::process::Command::new(program)
                    .args(["-p", &profile, "/bin/sh", "-c", &script])
                    .stdin(std::process::Stdio::null())
                    .output()
                    .await
                    .map_err(|e| format!("could not run sandbox-exec: {e}"))
            }
        };
        let out = run("exit 0".into()).await?;
        if !out.status.success() {
            return Err(format!("sandbox did not start: {}", String::from_utf8_lossy(&out.stderr).trim()));
        }
        // Signal 0 only checks permission; it never affects the target.
        let out = run(format!("kill -0 {me}")).await?;
        if out.status.success() {
            return Err("a sandboxed process could still signal an outside process".into());
        }
        let out = run("sleep 30 & pid=$!; kill $pid && wait $pid; exit 0".into()).await?;
        if !out.status.success() {
            return Err(format!(
                "a sandboxed process could not stop its own child: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(())
    }

    /// Test hook: mark the guard as verified without running the self-test.
    #[cfg(test)]
    pub fn assume_available(program: &str) -> Self {
        let g = Self::with_program(true, program);
        g.set(true, "assumed for tests");
        g
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_blocks_outside_signals_and_apple_events() {
        assert!(PROFILE.contains("(deny signal)"));
        assert!(PROFILE.contains("(allow signal (target same-sandbox))"));
        assert!(PROFILE.contains("(deny appleevent-send)"));
        // Order matters in seatbelt profiles: the re-allow must come after the deny.
        assert!(PROFILE.find("(deny signal)") < PROFILE.find("(allow signal"));
    }

    #[test]
    fn wrapper_only_when_enabled_and_verified() {
        let g = Guard::new(true);
        assert!(g.wrapper().is_none(), "not verified yet");
        let g = Guard::assume_available("/usr/bin/sandbox-exec");
        assert_eq!(g.wrapper().unwrap().0, "/usr/bin/sandbox-exec");
        g.set_enabled(false);
        assert!(g.wrapper().is_none());
    }

    #[cfg(not(target_os = "macos"))]
    #[tokio::test]
    async fn self_test_reports_unsupported_off_macos() {
        let g = Guard::new(true);
        g.self_test().await;
        let s = g.status();
        assert!(!s.available);
        assert!(s.detail.contains("macOS"));
    }

    /// A "sandbox" that does not sandbox anything must fail the self-test, so the guard is never
    /// reported as protecting the user when it isn't.
    #[cfg(unix)]
    #[tokio::test]
    async fn self_test_rejects_a_sandbox_that_does_not_block_signals() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("chimera-guard-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let fake = dir.join("sandbox-exec");
        // Ignores `-p PROFILE` and runs the command unsandboxed.
        std::fs::write(&fake, "#!/bin/sh\nshift 2\nexec \"$@\"\n").unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let g = Guard::with_program(true, fake.to_str().unwrap());
        let err = g.run_checks(PROFILE).await.unwrap_err();
        assert!(err.contains("could still signal"), "{err}");
    }
}
