//! Supervise standalone login1 and the graphical greeter. Desktop failures
//! leave the development consoles and the rest of the engine running.
use std::collections::VecDeque;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use aftermath_ctl::graphical::Settings;
use nix::sys::signal::{kill, Signal};
use nix::sys::wait::WaitStatus;
use nix::unistd::{chown, Gid, Pid, Uid, User};
use serde_json::{json, Value};

const LOGIN1: &str = "/usr/lib/elogind/elogind";
const SDDM: &str = "/usr/bin/sddm";

#[derive(Default)]
struct Service {
    pid: Option<i32>,
    starts: VecDeque<Instant>,
    restarts: u32,
    stopping: Option<Instant>,
}
impl Service {
    fn spawn(&mut self, program: &str, args: &[&str], log: &str) -> Result<(), String> {
        let now = Instant::now();
        self.starts
            .retain(|at| now.duration_since(*at) < Duration::from_secs(30));
        if self.starts.len() >= 3 {
            return Err(format!(
                "{program}: restart limit reached; consoles remain available"
            ));
        }
        let out = OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)
            .map_err(|e| e.to_string())?;
        let mut command = Command::new(program);
        command
            .args(args)
            .env_clear()
            .env("PATH", "/usr/local/sbin:/usr/local/bin:/usr/bin")
            .env("LANG", "C.UTF-8")
            .env("SYSTEMD_LOG_TARGET", "console")
            .env("ELOGIND_LOG_TARGET", "console")
            .stdin(Stdio::null())
            .stdout(Stdio::from(out.try_clone().map_err(|e| e.to_string())?))
            .stderr(Stdio::from(out));
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn().map_err(|e| format!("{program}: {e}"))?;
        if !self.starts.is_empty() {
            self.restarts += 1;
        }
        self.starts.push_back(now);
        self.pid = Some(child.id() as i32);
        self.stopping = None;
        Ok(())
    }
    fn stop(&mut self) {
        if let Some(pid) = self.pid {
            if self.stopping.is_none() {
                let _ = kill(Pid::from_raw(pid), Signal::SIGTERM);
                self.stopping = Some(Instant::now());
            }
            if self
                .stopping
                .is_some_and(|t| t.elapsed() > Duration::from_secs(3))
            {
                let _ = kill(Pid::from_raw(-pid), Signal::SIGKILL);
                let _ = kill(Pid::from_raw(pid), Signal::SIGKILL);
            }
        }
    }
    fn exited(&mut self, pid: i32) -> bool {
        if self.pid == Some(pid) {
            self.pid = None;
            self.stopping = None;
            true
        } else {
            false
        }
    }
}

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
}

impl Graphical {
    pub fn new(cmdline: &str) -> Self {
        let config = aftermath_ctl::graphical::settings().unwrap_or_default();
        let disabled = !config.enabled
            || cmdline
                .split_whitespace()
                .any(|a| a == "aftermath.desktop=off" || a == "aftermath.test=session-smoke");
        Self {
            config,
            state: if disabled {
                "disabled"
            } else {
                "waiting-for-foundations"
            },
            error: None,
            prepared: false,
            login1: Service::default(),
            greeter: Service::default(),
            probe: None,
            ready: false,
            next_probe: Instant::now(),
            provider_deadline: None,
        }
    }
    pub fn status(&self) -> Value {
        json!({"state": self.state, "error": self.error, "login1": {"provider": LOGIN1, "pid": self.login1.pid, "responding": self.ready, "restarts": self.login1.restarts},
            "display_manager": {"provider": SDDM, "pid": self.greeter.pid, "restarts": self.greeter.restarts}, "session": "Plasma Wayland", "user_bus": "dbus-run-session per graphical session", "log": "/run/aftermath/sddm.log"})
    }
    fn fail(&mut self, error: String) {
        crate::warn(&format!("graphical boot: {error}"));
        self.error = Some(error);
        self.state = "failed";
        self.greeter.stop();
    }
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
            return;
        }
        if !self.prepared {
            if let Err(e) = prepare(&self.config) {
                self.fail(e);
                return;
            }
            self.prepared = true;
        }
        if self.login1.pid.is_none() && !self.ready {
            if let Err(e) = self.login1.spawn(LOGIN1, &[], "/run/aftermath/elogind.log") {
                self.fail(e);
                return;
            }
            self.provider_deadline = Some(Instant::now() + Duration::from_secs(15));
            self.state = "starting-login1";
        }
        if let Some((pid, deadline)) = self.probe {
            if Instant::now() > deadline {
                let _ = kill(Pid::from_raw(pid), Signal::SIGKILL);
            }
        } else if Instant::now() >= self.next_probe {
            match Command::new("/usr/bin/dbus-send")
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
            {
                Ok(child) => {
                    self.probe = Some((child.id() as i32, Instant::now() + Duration::from_secs(2)))
                }
                Err(e) => {
                    self.fail(e.to_string());
                    return;
                }
            }
        }
        if !self.ready {
            self.greeter.stop();
            if self.provider_deadline.is_some_and(|at| Instant::now() > at) {
                self.login1.stop();
            }
            return;
        }
        self.provider_deadline = None;
        if self.greeter.pid.is_none() {
            match self.greeter.spawn(SDDM, &[], "/run/aftermath/sddm.log") {
                Ok(()) => {
                    self.state = "starting-greeter";
                    crate::log("graphical login started: SDDM with Plasma Wayland");
                }
                Err(e) => self.fail(e),
            }
        } else if self.state == "starting-greeter"
            && fs::read_dir("/proc")
                .into_iter()
                .flatten()
                .flatten()
                .any(|entry| {
                    fs::read_link(entry.path().join("exe"))
                        .ok()
                        .and_then(|p| {
                            p.file_name()
                                .map(|n| n.to_string_lossy().starts_with("sddm-greeter"))
                        })
                        .unwrap_or(false)
                })
        {
            self.state = "running";
        }
    }
    pub fn child_exited(&mut self, pid: i32, status: &WaitStatus) -> bool {
        if self.probe.is_some_and(|(p, _)| p == pid) {
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
            self.state = "restarting-login1";
            return true;
        }
        if self.greeter.exited(pid) {
            self.state = "restarting-greeter";
            return true;
        }
        false
    }
}

fn native(program: &str, args: &[&str]) -> Result<(), String> {
    let output = Command::new(program)
        .args(args)
        .env_clear()
        .env("PATH", "/usr/bin")
        .output()
        .map_err(|e| e.to_string())?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "{program}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn prepare(config: &Settings) -> Result<(), String> {
    aftermath_ctl::users::require_aftermath()?;
    if !Path::new(LOGIN1).is_file() || !Path::new(SDDM).is_file() {
        return Err("elogind or SDDM is missing; rebuild the desktop image".into());
    }
    if User::from_name("sddm")
        .map_err(|e| e.to_string())?
        .is_none()
    {
        native(
            "/usr/bin/useradd",
            &[
                "--system",
                "--user-group",
                "--home-dir",
                "/var/lib/sddm",
                "--shell",
                "/usr/bin/nologin",
                "--",
                "sddm",
            ],
        )?;
    }
    let sddm = User::from_name("sddm")
        .map_err(|e| e.to_string())?
        .ok_or("no SDDM service account")?;
    for path in ["/var/lib/sddm", "/var/lib/sddm/.config"] {
        fs::create_dir_all(path).map_err(|e| e.to_string())?;
        chown(path, Some(sddm.uid), Some(sddm.gid)).map_err(|e| e.to_string())?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o750)).map_err(|e| e.to_string())?;
    }
    fs::create_dir_all("/usr/share/aftermath/xsessions").map_err(|e| e.to_string())?;
    if config.development_accounts && !Path::new("/etc/aftermath/desktop-seeded").exists() {
        if User::from_name("bob").map_err(|e| e.to_string())?.is_none() {
            crate::users::add("bob")?;
        }
        let bob = User::from_name("bob")
            .map_err(|e| e.to_string())?
            .ok_or("no Bob account")?;
        if !aftermath_ctl::users::regular(bob.uid.as_raw())
            || bob.gid == Gid::from_raw(0)
            || bob.uid == Uid::from_raw(0)
        {
            return Err("bob must be a regular unprivileged account".into());
        }
        // Explicit temporary development defaults requested by the user. Secrets
        // go only to chpasswd's private stdin, never logs, arguments or IPC.
        let mut command = Command::new("/usr/bin/chpasswd")
            .env_clear()
            .env("PATH", "/usr/bin")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| e.to_string())?;
        let result = command
            .stdin
            .take()
            .ok_or("no password tool input")?
            .write_all(b"bob:bob\nroot:bob\n");
        let status = command.wait().map_err(|e| e.to_string())?;
        result.map_err(|e| e.to_string())?;
        if !status.success() {
            return Err("temporary account password initialization failed".into());
        }
        fs::write(
            "/etc/aftermath/desktop-seeded",
            "development accounts initialized once\n",
        )
        .map_err(|e| e.to_string())?;
        crate::log("initialized temporary development accounts: bob and root");
    }
    Ok(())
}

pub fn system_accounts() {
    if aftermath_ctl::graphical::settings().is_ok_and(|c| c.enabled)
        && aftermath_ctl::users::require_aftermath().is_ok()
        && Path::new("/usr/bin/systemd-sysusers").is_file()
    {
        if let Err(error) = native("/usr/bin/systemd-sysusers", &[]) {
            crate::warn(&format!("system accounts: {error}"));
        }
        // Arch's dbus package normally applies this in its installation hook.
        // Also do it here for the private, unprivileged integration image.
        let helper = Path::new("/usr/lib/dbus-daemon-launch-helper");
        if fs::symlink_metadata(helper).is_ok_and(|m| m.is_file()) {
            if let Ok(Some(dbus)) = User::from_name("dbus") {
                let result = chown(helper, Some(Uid::from_raw(0)), Some(dbus.gid))
                    .map_err(|e| e.to_string())
                    .and_then(|_| {
                        fs::set_permissions(helper, fs::Permissions::from_mode(0o4750))
                            .map_err(|e| e.to_string())
                    });
                if let Err(error) = result {
                    crate::warn(&format!("D-Bus activation helper: {error}"));
                }
            }
        }
    }
}
