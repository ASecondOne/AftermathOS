//! Graphical boot orchestration. A failed or hung login screen releases tty1.
//! Native PAM/elogind own authenticated sessions and their runtime directories.
mod accounts;
mod appearance;
mod service;
pub use accounts::system_accounts;

use aftermath_ctl::graphical::Settings;
use nix::sys::{
    signal::{kill, Signal},
    wait::WaitStatus,
};
use nix::unistd::Pid;
use serde_json::{json, Value};
use service::Service;
use std::collections::VecDeque;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const LOGIN1: &str = "/usr/lib/elogind/elogind";
const SDDM: &str = "/usr/bin/sddm";

/// All desktop timers run in the engine's nonblocking event loop.
pub struct Graphical {
    config: Settings,
    state: &'static str,
    error: Option<String>,
    prepared: bool,
    login1: Service,
    greeter: Service,
    probe: Option<(i32, Instant)>,
    ready: bool,
    next_probe: Instant,
    provider_deadline: Option<Instant>,
    boot_started: Instant,
    no_display_since: Option<Instant>,
    last_greeter: Option<u32>,
    greeter_starts: VecDeque<Instant>,
    force_failure: bool,
}

impl Graphical {
    /// Apply the same desktop policy used by console allocation.
    pub fn new(cmdline: &str) -> Self {
        Self {
            config: aftermath_ctl::graphical::settings().unwrap_or_default(),
            state: if aftermath_ctl::graphical::enabled_for_boot(cmdline) {
                "waiting-for-foundations"
            } else {
                "disabled"
            },
            error: None,
            prepared: false,
            login1: Service::default(),
            greeter: Service::default(),
            probe: None,
            ready: false,
            next_probe: Instant::now(),
            provider_deadline: None,
            boot_started: Instant::now(),
            no_display_since: None,
            last_greeter: None,
            greeter_starts: VecDeque::new(),
            force_failure: cmdline
                .split_whitespace()
                .any(|arg| arg == "aftermath.test=desktop-fail"),
        }
    }

    /// Describe process health without equating a daemon PID with a visible UI.
    pub fn status(&self) -> Value {
        json!({
            "state": self.state,
            "error": self.error,
            "login1": { "provider": LOGIN1, "pid": self.login1.pid,
                "responding": self.ready, "restarts": self.login1.restarts },
            "display_manager": { "provider": SDDM, "pid": self.greeter.pid,
                "restarts": self.greeter.restarts, "protocol": "wayland" },
            "session": "Plasma Wayland",
            "user_bus": "dbus-run-session per graphical session",
            "log": "/run/aftermath/sddm.log"
        })
    }

    /// Reveal fallback only after the display manager has released its devices.
    pub fn fallback_reason(&self) -> Option<&str> {
        if self.state == "failed" && self.greeter.pid.is_none() {
            self.error.as_deref()
        } else {
            None
        }
    }

    /// Fail once; reaping a stopped child must not revive a failed desktop.
    fn fail(&mut self, error: String) {
        crate::warn(&format!("graphical boot: {error}"));
        crate::logs::desktop_event(&format!("desktop failed: {error}"));
        self.error = Some(error);
        self.state = "failed";
        self.greeter.stop();
    }

    /// Bound foundation, provider and greeter startup independently.
    pub fn advance(&mut self, foundations: bool) {
        if self.state == "disabled" {
            return;
        }
        if self.state == "failed" {
            self.greeter.stop();
            return;
        }
        if !foundations {
            self.greeter.stop();
            if self.boot_started.elapsed() > Duration::from_secs(45) {
                self.fail("device discovery or system D-Bus never became ready".into());
            }
            return;
        }
        if self.force_failure {
            self.fail("requested private desktop failure test".into());
            return;
        }
        if !self.prepared {
            if let Err(error) =
                accounts::prepare(&self.config).and_then(|_| appearance::prepare(&self.config))
            {
                self.fail(error);
                return;
            }
            self.prepared = true;
        }
        if self.login1.pid.is_none() && !self.ready {
            if let Err(error) = self.login1.spawn(LOGIN1, "/run/aftermath/elogind.log") {
                self.fail(error);
                return;
            }
            self.provider_deadline = Some(Instant::now() + Duration::from_secs(15));
            self.state = "starting-login1";
        }
        if let Err(error) = self.advance_probe() {
            self.fail(error);
            return;
        }
        if !self.ready {
            self.greeter.stop();
            if self.provider_deadline.is_some_and(|at| Instant::now() > at) {
                self.login1.stop();
            }
            return;
        }
        self.provider_deadline = None;
        self.advance_greeter();
    }

    /// Ask the actual system bus for login1 ownership using a bounded child.
    fn advance_probe(&mut self) -> Result<(), String> {
        if let Some((pid, deadline)) = self.probe {
            if Instant::now() > deadline {
                let _ = kill(Pid::from_raw(pid), Signal::SIGKILL);
            }
        } else if Instant::now() >= self.next_probe {
            let child = Command::new("/usr/bin/dbus-send")
                .args([
                    "--system",
                    "--print-reply",
                    "--reply-timeout=1000",
                    "--dest=org.freedesktop.DBus",
                    "/org/freedesktop/DBus",
                    "org.freedesktop.DBus.GetNameOwner",
                    "string:org.freedesktop.login1",
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .map_err(|error| error.to_string())?;
            self.probe = Some((child.id() as i32, Instant::now() + Duration::from_secs(2)));
        }
        Ok(())
    }

    /// Detect missing UI and fast greeter crashes even while SDDM itself lives.
    fn advance_greeter(&mut self) {
        if self.greeter.pid.is_none() {
            if let Err(error) = self.greeter.spawn(SDDM, "/run/aftermath/sddm.log") {
                self.fail(error);
                return;
            }
            self.state = "starting-greeter";
            self.no_display_since = Some(Instant::now());
            crate::logs::desktop_event("SDDM starting with a Wayland greeter");
            return;
        }
        let (greeter, session) = display_processes();
        if session {
            self.greeter_starts.clear();
        }
        if let Some(pid) = greeter {
            if self.last_greeter != Some(pid) {
                let now = Instant::now();
                self.greeter_starts
                    .retain(|at| now.duration_since(*at) < Duration::from_secs(30));
                self.greeter_starts.push_back(now);
                self.last_greeter = Some(pid);
                if self.greeter_starts.len() > 6 {
                    self.fail("greeter repeatedly crashed inside SDDM".into());
                    return;
                }
            }
        }
        if greeter.is_some() || session {
            self.state = "running";
            self.no_display_since = None;
        } else if self
            .no_display_since
            .get_or_insert_with(Instant::now)
            .elapsed()
            > Duration::from_secs(35)
        {
            self.fail("SDDM has no greeter or graphical session after 35 seconds".into());
        }
    }

    /// Central reaping reports probe results and supervised daemon exits here.
    pub fn child_exited(&mut self, pid: i32, status: &WaitStatus) -> bool {
        if self.probe.is_some_and(|(owned, _)| owned == pid) {
            self.probe = None;
            self.ready = matches!(status, WaitStatus::Exited(_, 0));
            self.next_probe = Instant::now()
                + if self.ready {
                    Duration::from_secs(3)
                } else {
                    Duration::from_millis(250)
                };
            return true;
        }
        if self.login1.exited(pid) {
            self.ready = false;
            self.greeter.stop();
            if self.state != "failed" {
                self.state = "restarting-login1";
            }
            return true;
        }
        if self.greeter.exited(pid) {
            if self.state != "failed" {
                self.state = "restarting-greeter";
            }
            return true;
        }
        false
    }
}

/// Greeter disappearance during user login is healthy while KWin/Plasma runs.
fn display_processes() -> (Option<u32>, bool) {
    let mut greeter = None;
    let mut session = false;
    for entry in fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Ok(executable) = fs::read_link(entry.path().join("exe")) else {
            continue;
        };
        let name = executable
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        if name.starts_with("sddm-greeter") {
            greeter = entry.file_name().to_string_lossy().parse().ok();
        }
        if matches!(name, "kwin_wayland" | "plasmashell")
            && entry
                .metadata()
                .is_ok_and(|metadata| metadata.uid() >= 1000)
        {
            session = true;
        }
    }
    (greeter, session)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reaping_a_stopped_greeter_preserves_failed_state_and_fallback() {
        let root =
            std::env::temp_dir().join(format!("aftermath-greeter-reap-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let daemon = root.join("daemon");
        crate::test_support::install(&daemon);
        let mut graphical = Graphical::new("aftermath.desktop=off");
        graphical.state = "running";
        graphical
            .greeter
            .spawn(daemon.to_str().unwrap(), root.join("log").to_str().unwrap())
            .unwrap();
        let pid = graphical.greeter.pid.unwrap();
        graphical.fail("test failure".into());
        let status = nix::sys::wait::waitpid(Pid::from_raw(pid), None).unwrap();
        assert!(graphical.child_exited(pid, &status));
        assert_eq!(graphical.state, "failed");
        assert_eq!(graphical.fallback_reason(), Some("test failure"));
        fs::remove_dir_all(root).unwrap();
    }
}
