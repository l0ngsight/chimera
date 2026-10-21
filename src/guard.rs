//! Process guard: runs agent CLIs inside an OS sandbox that stops them from killing or quitting
//! programs they did not start (the user's browser, editors, the Chimera server) by accident.
//!
//! Prompt rules and per-CLI deny lists are advisory or partial; an agent can still run
//! `pkill chrome`, `lsof -ti:PORT | xargs kill` or a script that sends signals. The guard enforces
//! the signal part at the OS level for every runtime:
//!
//! - **Linux**: a private PID namespace (bubblewrap, or util-linux `unshare`). The agent only sees
//!   processes it started, so there is nothing else to signal. When the agent exits or is
//!   cancelled, everything it left running is killed with it. The variables that point at the
//!   user's desktop session are removed too (`DISPLAY`, `WAYLAND_DISPLAY`, the D-Bus session bus,
//!   `XDG_RUNTIME_DIR`, tmux and screen), so `xdotool`, `notify-send`, `systemctl --user` or
//!   `tmux kill-server` fail by default instead of acting on the user's session.
//! - **macOS**: `sandbox-exec` with a profile that denies signals outside the sandbox and Apple
//!   Events (AppleScript `quit`).
//!
//! It is not a security sandbox: files, the network and sockets stay reachable (an agent that
//! sets `DISPLAY=:0` itself can still talk to X11), because agents need them to do their work.
//!
//! Each backend is verified on the actual machine at startup (the sandbox starts, cannot signal a
//! process outside it, and can still stop its own child). The first backend that passes is used;
//! if none does, the guard reports why and stays off.

use std::process::Stdio;
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::Serialize;

/// macOS seatbelt profile. Later rules win: signals are denied, then re-allowed within the sandbox.
pub const MACOS_PROFILE: &str = concat!(
    "(version 1)\n(allow default)\n(deny signal)\n(allow signal (target same-sandbox))\n",
    "(deny appleevent-send)\n"
);
const MACOS_SIGNALS_ONLY: &str = "(version 1)\n(allow default)\n(deny signal)\n(allow signal (target same-sandbox))\n";

/// Environment variables removed from guarded processes on Linux. Without them, tools that act on
/// the user's desktop session fail instead: closing windows through X11/Wayland (`xdotool …
/// windowclose`, `wmctrl -c`), the session bus and user services (`busctl --user`, `systemctl
/// --user stop`, `notify-send`), and terminal multiplexers (`tmux kill-server`, `screen -X quit`).
pub const LINUX_HIDDEN_ENV: &[&str] =
    &["DISPLAY", "WAYLAND_DISPLAY", "DBUS_SESSION_BUS_ADDRESS", "XDG_RUNTIME_DIR", "TMUX", "TMUX_PANE", "STY"];

/// One way of sandboxing a command: `program args… <command> <command args…>`.
#[derive(Debug, Clone)]
pub struct Backend {
    pub name: &'static str,
    pub program: String,
    pub args: Vec<String>,
    pub hidden_env: &'static [&'static str],
    /// What it protects, for the status line.
    pub covers: &'static str,
}

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

/// Backends to try on this OS, best first.
pub fn platform_backends() -> Vec<Backend> {
    if cfg!(target_os = "linux") {
        vec![
            Backend {
                name: "bubblewrap",
                program: "bwrap".into(),
                args: s(&["--dev-bind", "/", "/", "--unshare-pid", "--proc", "/proc", "--die-with-parent", "--"]),
                hidden_env: LINUX_HIDDEN_ENV,
                covers: "private process namespace via bubblewrap",
            },
            Backend {
                name: "unshare",
                program: "unshare".into(),
                args: s(&["--user", "--map-current-user", "--pid", "--fork", "--mount-proc", "--kill-child"]),
                hidden_env: LINUX_HIDDEN_ENV,
                covers: "private process namespace via unshare",
            },
        ]
    } else if cfg!(target_os = "macos") {
        vec![
            Backend {
                name: "sandbox-exec",
                program: "/usr/bin/sandbox-exec".into(),
                args: vec!["-p".into(), MACOS_PROFILE.into()],
                hidden_env: &[],
                covers: "signals and Apple Events blocked via sandbox-exec",
            },
            Backend {
                name: "sandbox-exec (signals only)",
                program: "/usr/bin/sandbox-exec".into(),
                args: vec!["-p".into(), MACOS_SIGNALS_ONLY.into()],
                hidden_env: &[],
                covers: "signals blocked via sandbox-exec (Apple Events rule unsupported)",
            },
        ]
    } else {
        vec![]
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct GuardStatus {
    /// A backend passed the self-test on this machine.
    pub available: bool,
    /// The user wants it on.
    pub enabled: bool,
    /// Backend in use, if any.
    pub backend: Option<String>,
    /// Human-readable result of the self-test.
    pub detail: String,
}

pub struct Guard {
    enabled: AtomicBool,
    candidates: Vec<Backend>,
    active: RwLock<Option<Backend>>,
    detail: RwLock<String>,
}

impl Guard {
    pub fn new(enabled: bool) -> Self {
        Self::with_backends(enabled, platform_backends())
    }

    pub fn with_backends(enabled: bool, candidates: Vec<Backend>) -> Self {
        Self {
            enabled: AtomicBool::new(enabled),
            candidates,
            active: RwLock::new(None),
            detail: RwLock::new("not checked yet".into()),
        }
    }

    pub fn set_enabled(&self, on: bool) {
        self.enabled.store(on, Ordering::Relaxed);
    }

    pub fn status(&self) -> GuardStatus {
        let active = self.active.read().unwrap_or_else(|e| e.into_inner());
        GuardStatus {
            available: active.is_some(),
            enabled: self.enabled.load(Ordering::Relaxed),
            backend: active.as_ref().map(|b| b.name.to_string()),
            detail: self.detail.read().unwrap_or_else(|e| e.into_inner()).clone(),
        }
    }

    /// The backend to wrap agent commands with, when the guard is on and verified.
    pub fn wrapper(&self) -> Option<Backend> {
        if !self.enabled.load(Ordering::Relaxed) {
            return None;
        }
        self.active.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn set(&self, active: Option<Backend>, detail: String) {
        *self.active.write().unwrap_or_else(|e| e.into_inner()) = active;
        *self.detail.write().unwrap_or_else(|e| e.into_inner()) = detail;
    }

    /// Picks the first backend that passes the checks on this machine.
    pub async fn self_test(&self) {
        if self.candidates.is_empty() {
            self.set(
                None,
                "Not available on this operating system; run Chimera in a container or VM for isolation.".into(),
            );
            return;
        }
        let mut errors = vec![];
        for b in &self.candidates {
            match check(b).await {
                Ok(()) => {
                    self.set(
                        Some(b.clone()),
                        format!(
                            "Verified on this machine ({}): agents cannot kill or quit programs they did not start, and can still stop their own.",
                            b.covers
                        ),
                    );
                    return;
                }
                Err(e) => errors.push(format!("{}: {e}", b.name)),
            }
        }
        let mut detail = format!("Self-test failed, so the guard is off. {}", errors.join("; "));
        if cfg!(target_os = "linux") {
            detail.push_str(
                " Install bubblewrap (e.g. `sudo apt install bubblewrap` or `sudo dnf install bubblewrap`) and restart Chimera.",
            );
        }
        tracing::warn!("process guard unavailable: {detail}");
        self.set(None, detail);
    }
}

/// The sandbox must (1) run a command, (2) be unable to signal a process outside it and
/// (3) still be able to stop a process it started itself.
async fn check(b: &Backend) -> Result<(), String> {
    let run = |script: String| {
        let b = b.clone();
        async move {
            tokio::process::Command::new(&b.program)
                .args(&b.args)
                .args(["/bin/sh", "-c", &script])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .output()
                .await
                .map_err(|e| format!("could not run `{}`: {e}", b.program))
        }
    };
    let stderr = |o: &std::process::Output| String::from_utf8_lossy(&o.stderr).trim().to_string();

    let out = run("exit 0".into()).await?;
    if !out.status.success() {
        return Err(format!("sandbox did not start: {}", stderr(&out)));
    }

    // A real outside process to aim at; signal 0 only checks permission and never affects it.
    let mut target = tokio::process::Command::new("sleep")
        .arg("60")
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("could not start a test process: {e}"))?;
    let pid = target.id().ok_or("test process has no pid")?;
    let out = run(format!("kill -0 {pid}")).await;
    let _ = target.start_kill();
    if out?.status.success() {
        return Err("a sandboxed process could still signal a process outside the sandbox".into());
    }

    let out = run("sleep 30 & p=$!; kill $p; wait $p; exit 0".into()).await?;
    if !out.status.success() {
        return Err(format!("a sandboxed process could not stop its own child: {}", stderr(&out)));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backend(program: &str, args: &[&str]) -> Backend {
        Backend { name: "test", program: program.into(), args: s(args), hidden_env: &[], covers: "test" }
    }

    #[test]
    fn macos_profile_blocks_outside_signals_and_apple_events() {
        assert!(MACOS_PROFILE.contains("(deny signal)"));
        assert!(MACOS_PROFILE.contains("(allow signal (target same-sandbox))"));
        assert!(MACOS_PROFILE.contains("(deny appleevent-send)"));
        // Order matters in seatbelt profiles: the re-allow must come after the deny.
        assert!(MACOS_PROFILE.find("(deny signal)") < MACOS_PROFILE.find("(allow signal"));
    }

    /// A "sandbox" that does not isolate anything must fail, so the guard never claims to
    /// protect the user when it does not.
    #[tokio::test]
    async fn a_non_isolating_wrapper_fails_the_self_test() {
        let g = Guard::with_backends(true, vec![backend("env", &[])]);
        g.self_test().await;
        let st = g.status();
        assert!(!st.available, "{}", st.detail);
        assert!(st.detail.contains("could still signal"), "{}", st.detail);
        assert!(g.wrapper().is_none());
    }

    #[tokio::test]
    async fn missing_backends_are_reported_and_skipped() {
        let g = Guard::with_backends(true, vec![backend("/nonexistent/sandbox", &[])]);
        g.self_test().await;
        assert!(!g.status().available);
        assert!(g.status().detail.contains("could not run"));
    }

    /// Runs the real Linux backends when this machine supports them (bubblewrap or user
    /// namespaces); skipped silently where neither works, e.g. some CI containers.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn linux_guard_isolates_processes_when_supported() {
        let g = Guard::new(true);
        g.self_test().await;
        let st = g.status();
        if !st.available {
            eprintln!("skipping: {}", st.detail);
            return;
        }
        let b = g.wrapper().unwrap();
        // pkill/killall inside the sandbox must not reach a process outside it.
        let mut outside = tokio::process::Command::new("sleep").arg("60").kill_on_drop(true).spawn().unwrap();
        let status = tokio::process::Command::new(&b.program)
            .args(&b.args)
            .args(["/bin/sh", "-c", "pkill -x sleep; killall sleep; exit 0"])
            .status()
            .await
            .unwrap();
        assert!(status.success());
        assert!(outside.try_wait().unwrap().is_none(), "outside process was killed from inside the sandbox");
        let _ = outside.start_kill();
        g.set_enabled(false);
        assert!(g.wrapper().is_none());
    }
}
