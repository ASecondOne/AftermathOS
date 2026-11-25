//! Desktop supervision: udev, system D-Bus, elogind and graphical login.
//! PID 1 remains aftermath-init; Plasma uses its classic startup path.

use std::collections::VecDeque;
use std::fs::{self, OpenOptions};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use aftermath_ctl::desktop::{UDEVADM_PATHS, UDEV_DAEMONS};
use nix::sys::signal::{kill, Signal};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::Pid;
use serde_json::{json, Value};

use crate::control::Subsystem;

pub struct Desktop {
    devices: Devices,
    bus: crate::bus::Bus,
    graphical: crate::graphical::Graphical,
}

impl Desktop {
    pub fn boot(cmdline: &str) -> Self {
        // The bus prepares machine identity first. Udev link rules can use
        // that identity for stable per-machine device configuration.
        let bus = crate::bus::Bus::boot(cmdline);
        let devices = Devices::boot(cmdline);
        Self {
            devices,
            bus,
            graphical: crate::graphical::Graphical::new(cmdline),
        }
    }
}

impl Subsystem for Desktop {
    fn name(&self) -> &'static str {
        "desktop"
    }
    fn about(&self) -> &'static str {
        "desktop prerequisites, device discovery, D-Bus and graphical login"
    }
    fn queries(&self) -> &'static [&'static str] {
        &["check", "devices", "bus", "status"]
    }
    fn call(&mut self, verb: &str, args: &[String]) -> Result<Value, String> {
        if !args.is_empty() {
            return Err("desktop queries take no engine arguments".into());
        }
        match verb {
            "status" => Ok(self.graphical.status()),
            "devices" => Ok(self.devices.status()),
            "bus" => Ok(self.bus.status()),
            "check" => {
                let mut report = aftermath_ctl::desktop::check(Path::new("/"))?;
                report["devices"] = self.devices.status();
                report["bus"] = self.bus.status();
                report["graphical"] = self.graphical.status();
                report["ready"] = json!(
                    report["missing_or_unknown"] == 0
                        && report["graphical"]["state"] == "running"
                        && report["bus"]["responding"] == true
                        && self.devices.state == "running"
                );
                report["scope"] = json!("File inventory plus live device, system bus, login1 and graphical-login supervision. Hardware/session behavior still requires runtime validation.");
                report["next_step"] = json!("Inspect `amctl desktop status` and /run/aftermath/sddm.log. Sign in through SDDM to start Plasma Wayland; authenticated console login remains available on tty2.");
                Ok(report)
            }
            _ => Err(
                "expected `desktop check`, `desktop devices`, `desktop bus` or `desktop status`"
                    .into(),
            ),
        }
    }
    fn summary(&self) -> Option<Value> {
        Some(
            json!({"ready": self.devices.state == "running" && self.bus.status()["responding"] == true && self.graphical.status()["state"] == "running", "devices": self.devices.status(), "bus": self.bus.status(), "graphical": self.graphical.status()}),
        )
    }
    fn tick(&mut self) {
        self.devices.advance();
        self.bus.tick();
        self.graphical
            .advance(self.devices.state == "running" && self.bus.status()["responding"] == true);
    }
    fn fallback_reason(&self) -> Option<String> {
        self.graphical.fallback_reason().map(str::to_owned)
    }
    fn child_exited(&mut self, pid: i32, status: &WaitStatus) -> bool {
        self.bus.child_exited(pid, status)
            || self.devices.child_exited(pid, status)
            || self.graphical.child_exited(pid, status)
    }
}

const START_WINDOW: Duration = Duration::from_secs(30);
const START_LIMIT: usize = 5;
const BOOT_LIMIT: Duration = Duration::from_secs(25);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Ping,
    Subsystems,
    Devices,
    Settle,
    Complete,
}

impl Phase {
    fn args(self) -> &'static [&'static str] {
        match self {
            Self::Ping => &["control", "--timeout=1", "--ping"],
            Self::Subsystems => &["trigger", "--type=subsystems", "--action=add"],
            Self::Devices => &["trigger", "--type=devices", "--action=add"],
            Self::Settle => &["settle", "--timeout=10"],
            Self::Complete => &[],
        }
    }
    fn name(self) -> &'static str {
        match self {
            Self::Ping => "waiting-for-udev",
            Self::Subsystems => "triggering-subsystems",
            Self::Devices => "triggering-devices",
            Self::Settle => "settling",
            Self::Complete => "complete",
        }
    }
    fn next(self) -> Self {
        match self {
            Self::Ping => Self::Subsystems,
            Self::Subsystems => Self::Devices,
            Self::Devices => Self::Settle,
            _ => Self::Complete,
        }
    }
    fn timeout(self) -> Duration {
        Duration::from_secs(match self {
            Self::Ping => 2,
            Self::Settle => 12,
            _ => 5,
        })
    }
}

struct Task {
    pid: i32,
    deadline: Instant,
    timed_out: bool,
    cancelled: bool,
}

struct Devices {
    daemon_path: Option<PathBuf>,
    adm_path: Option<PathBuf>,
    runtime: PathBuf,
    log: PathBuf,
    daemon: Option<i32>,
    task: Option<Task>,
    state: &'static str,
    phase: Phase,
    coldplug: &'static str,
    responding: bool,
    startup_deadline: Instant,
    retry_at: Instant,
    starts: VecDeque<Instant>,
    restarts: u32,
    error: Option<String>,
}

fn find_program(paths: &[&str]) -> Option<PathBuf> {
    paths.iter().find_map(|p| {
        let m = fs::metadata(p).ok()?;
        (m.is_file() && m.permissions().mode() & 0o111 != 0).then(|| PathBuf::from(p))
    })
}

fn kill_group(pid: i32) {
    let _ = kill(Pid::from_raw(-pid), Signal::SIGKILL);
    let _ = kill(Pid::from_raw(pid), Signal::SIGKILL);
}

impl Devices {
    fn new(
        daemon_path: Option<PathBuf>,
        adm_path: Option<PathBuf>,
        runtime: PathBuf,
        log: PathBuf,
    ) -> Self {
        Self {
            daemon_path,
            adm_path,
            runtime,
            log,
            daemon: None,
            task: None,
            state: "starting",
            phase: Phase::Ping,
            coldplug: "pending",
            responding: false,
            startup_deadline: Instant::now(),
            retry_at: Instant::now(),
            starts: VecDeque::new(),
            restarts: 0,
            error: None,
        }
    }

    /// Run before DHCP: device rules may rename NICs. Every wait is bounded;
    /// failure leaves the daemon (if alive) handling hotplug and boot continues.
    pub fn boot(cmdline: &str) -> Self {
        let mut s = Self::new(
            find_program(UDEV_DAEMONS),
            find_program(UDEVADM_PATHS),
            "/run/udev".into(),
            "/run/aftermath/udev.log".into(),
        );
        if cmdline
            .split_whitespace()
            .any(|a| a == "aftermath.udev=off")
        {
            s.state = "disabled";
            s.coldplug = "not-started";
            crate::log("udev disabled by aftermath.udev=off");
            return s;
        }
        if cmdline
            .split_whitespace()
            .any(|a| a == "aftermath.test=udev-fail")
        {
            s.daemon_path = Some("/usr/bin/false".into());
        }
        if s.daemon_path.is_none() || s.adm_path.is_none() {
            s.state = "missing";
            s.fail(
                "udev daemon or udevadm not installed; continuing with kernel device nodes".into(),
            );
            return s;
        }
        s.start();
        let deadline = Instant::now() + BOOT_LIMIT;
        while matches!(s.state, "starting" | "restarting") {
            s.reap_owned();
            s.advance();
            if Instant::now() >= deadline {
                if let Some(t) = &mut s.task {
                    t.cancelled = true;
                    kill_group(t.pid);
                }
                s.fail("device discovery boot deadline reached; starting consoles anyway".into());
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        s
    }

    /// Only used before the main event loop starts (and by the test harness).
    /// Afterwards the engine's single reaper dispatches through child_exited.
    fn reap_owned(&mut self) {
        for pid in [self.daemon, self.task.as_ref().map(|t| t.pid)]
            .into_iter()
            .flatten()
        {
            if let Ok(st @ (WaitStatus::Exited(..) | WaitStatus::Signaled(..))) =
                waitpid(Pid::from_raw(pid), Some(WaitPidFlag::WNOHANG))
            {
                self.child_exited(pid, &st);
            }
        }
    }

    fn spawn(&self, program: &Path, args: &[&str]) -> Result<i32, String> {
        if let Some(parent) = self.log.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let out = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log)
            .map_err(|e| e.to_string())?;
        let err = out.try_clone().map_err(|e| e.to_string())?;
        let mut cmd = Command::new(program);
        cmd.args(args)
            .env_clear()
            .env("PATH", "/usr/local/sbin:/usr/local/bin:/usr/bin")
            .env("SYSTEMD_LOG_TARGET", "console")
            .env("SYSTEMD_LOG_LEVEL", "info")
            .stdin(Stdio::null())
            .stdout(Stdio::from(out))
            .stderr(Stdio::from(err));
        // SAFETY: setsid is async-signal-safe; no allocations after fork.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        cmd.spawn()
            .map(|c| c.id() as i32)
            .map_err(|e| format!("{}: {e}", program.display()))
    }

    fn start(&mut self) {
        let now = Instant::now();
        self.starts
            .retain(|t| now.duration_since(*t) < START_WINDOW);
        if self.starts.len() >= START_LIMIT {
            self.state = "failed";
            self.fail(
                "udev start limit hit (5 starts in 30s); continuing without device discovery"
                    .into(),
            );
            return;
        }
        self.starts.push_back(now);
        self.responding = false;
        self.coldplug = "pending";
        self.phase = Phase::Ping;
        self.startup_deadline = now + Duration::from_secs(5);
        self.retry_at = now;
        let result = fs::create_dir_all(&self.runtime)
            .map_err(|e| e.to_string())
            .and_then(|()| self.spawn(self.daemon_path.as_deref().expect("provider checked"), &[]));
        match result {
            Ok(pid) => {
                self.daemon = Some(pid);
                self.state = "starting";
                self.error = None;
                crate::log(&format!("udev started in foreground (pid {pid})"));
            }
            Err(e) => self.schedule_restart(e),
        }
    }

    fn schedule_restart(&mut self, error: String) {
        self.state = "restarting";
        self.responding = false;
        self.coldplug = "pending";
        self.retry_at = Instant::now() + Duration::from_secs(1);
        self.restarts += 1;
        crate::warn(&format!(
            "udev: {error}; retrying in 1s (consoles remain available)"
        ));
        self.error = Some(error);
    }

    fn fail(&mut self, error: String) {
        if !matches!(self.state, "missing" | "failed") {
            self.state = "degraded";
        }
        self.coldplug = "failed";
        crate::warn(&format!("udev: {error}; continuing boot"));
        self.error = Some(error);
    }

    fn advance(&mut self) {
        let now = Instant::now();
        if let Some(t) = &mut self.task {
            if now >= t.deadline && !t.timed_out {
                t.timed_out = true;
                kill_group(t.pid);
            }
            return; // wait for the single reaper before reusing this slot
        }
        if self.state == "restarting" && now >= self.retry_at {
            self.start();
        }
        if self.daemon.is_none() || self.coldplug != "pending" || now < self.retry_at {
            return;
        }
        if self.phase == Phase::Ping && now >= self.startup_deadline {
            self.fail("udev did not answer its readiness ping within 5s".into());
            return;
        }
        match self.spawn(
            self.adm_path.as_deref().expect("udevadm checked"),
            self.phase.args(),
        ) {
            Ok(pid) => {
                self.task = Some(Task {
                    pid,
                    deadline: now + self.phase.timeout(),
                    timed_out: false,
                    cancelled: false,
                })
            }
            Err(e) => self.fail(e),
        }
    }

    fn status(&self) -> Value {
        json!({
            "state": self.state, "pid": self.daemon, "provider": self.daemon_path,
            "udevadm": self.adm_path, "responding": self.responding,
            "coldplug": self.coldplug, "phase": self.phase.name(),
            "restarts": self.restarts, "error": self.error, "log": self.log,
            "seat_access": "elogind supplies seat/device control to authenticated graphical sessions",
        })
    }
}

impl Devices {
    fn child_exited(&mut self, pid: i32, st: &WaitStatus) -> bool {
        if self.daemon == Some(pid) {
            self.daemon = None;
            // The leader was already reaped; only its surviving workers need
            // cleanup. Never send a direct signal to a reaped leader's PID.
            let _ = kill(Pid::from_raw(-pid), Signal::SIGKILL);
            if let Some(t) = &mut self.task {
                t.cancelled = true;
                kill_group(t.pid);
            }
            self.schedule_restart(format!("daemon exited: {st:?}"));
            return true;
        }
        if !self.task.as_ref().is_some_and(|t| t.pid == pid) {
            return false;
        }
        let task = self.task.take().expect("task matched");
        if task.cancelled || self.coldplug != "pending" {
            return true;
        }
        if task.timed_out || !matches!(st, WaitStatus::Exited(_, 0)) {
            if self.phase == Phase::Ping && Instant::now() < self.startup_deadline {
                self.retry_at = Instant::now() + Duration::from_millis(100);
            } else {
                self.fail(format!(
                    "{} {} ({st:?})",
                    self.phase.name(),
                    if task.timed_out {
                        "timed out"
                    } else {
                        "failed"
                    }
                ));
            }
            return true;
        }
        if self.phase == Phase::Ping {
            self.responding = true;
        }
        self.phase = self.phase.next();
        if self.phase == Phase::Complete {
            self.state = "running";
            self.coldplug = "complete";
            self.error = None;
            crate::log("udev ready; coldplug complete (subsystems, devices, settle)");
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Harness {
        desktop: Devices,
        root: PathBuf,
    }
    impl Harness {
        fn new(scenario: &str) -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let root = std::env::temp_dir().join(format!(
                "am-udev-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&root).unwrap();
            fs::write(root.join("scenario"), scenario).unwrap();
            for name in ["daemon", "adm"] {
                crate::test_support::install(&root.join(name));
            }
            let desktop = Devices::new(
                Some(root.join("daemon")),
                Some(root.join("adm")),
                root.join("run/udev"),
                root.join("udev.log"),
            );
            Self { desktop, root }
        }
        fn until(&mut self, predicate: impl Fn(&Devices) -> bool) {
            let deadline = Instant::now() + Duration::from_secs(4);
            loop {
                self.desktop.reap_owned();
                self.desktop.advance();
                if predicate(&self.desktop) {
                    return;
                }
                assert!(
                    Instant::now() < deadline,
                    "timed out: {}",
                    self.desktop.status()
                );
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        fn calls(&self) -> String {
            fs::read_to_string(self.root.join("adm.calls")).unwrap()
        }
    }
    impl Drop for Harness {
        fn drop(&mut self) {
            for pid in [
                self.desktop.daemon,
                self.desktop.task.as_ref().map(|t| t.pid),
            ]
            .into_iter()
            .flatten()
            {
                kill_group(pid);
                let _ = waitpid(Pid::from_raw(pid), None);
            }
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn readiness_precedes_coldplug_and_settle() {
        let mut h = Harness::new("once-not-ready");
        h.desktop.start();
        h.until(|s| s.state == "running");
        assert!(h.desktop.responding);
        assert_eq!(h.desktop.coldplug, "complete");
        assert_eq!(
            h.calls().lines().collect::<Vec<_>>(),
            [
                "control --timeout=1 --ping",
                "control --timeout=1 --ping",
                "trigger --type=subsystems --action=add",
                "trigger --type=devices --action=add",
                "settle --timeout=10",
            ]
        );
    }

    #[test]
    fn failed_coldplug_keeps_daemon_alive_without_claiming_success() {
        let mut h = Harness::new("fail-trigger");
        h.desktop.start();
        h.until(|s| s.state == "degraded");
        assert_eq!(h.desktop.coldplug, "failed");
        assert!(h.desktop.responding);
        assert!(kill(Pid::from_raw(h.desktop.daemon.unwrap()), None).is_ok());
        assert!(!h.calls().contains("settle"));
    }

    #[test]
    fn stuck_settle_is_killed_and_does_not_block_supervision() {
        let mut h = Harness::new("stall-settle");
        h.desktop.start();
        h.until(|s| s.phase == Phase::Settle && s.task.is_some());
        h.desktop.task.as_mut().unwrap().deadline = Instant::now();
        h.until(|s| s.state == "degraded");
        assert!(h.desktop.error.as_deref().unwrap().contains("timed out"));
        assert!(h.desktop.task.is_none());
        assert!(h.desktop.daemon.is_some());
    }

    #[test]
    fn daemon_restarts_and_repeats_coldplug_then_obeys_start_limit() {
        let mut h = Harness::new("success");
        h.desktop.start();
        h.until(|s| s.state == "running");
        let first = h.desktop.daemon.unwrap();
        kill_group(first);
        h.until(|s| s.state == "restarting");
        assert!(!h.desktop.responding);
        h.desktop.retry_at = Instant::now();
        h.until(|s| s.state == "running");
        assert_ne!(h.desktop.daemon.unwrap(), first);
        assert_eq!(h.desktop.restarts, 1);
        assert_eq!(h.calls().matches("settle --timeout=10").count(), 2);
        h.desktop.starts = (0..START_LIMIT).map(|_| Instant::now()).collect();
        kill_group(h.desktop.daemon.unwrap());
        h.until(|s| s.state == "restarting");
        h.desktop.retry_at = Instant::now();
        h.until(|s| s.state == "failed");
        assert!(h.desktop.error.as_deref().unwrap().contains("start limit"));
        assert!(h.desktop.daemon.is_none());
    }

    #[test]
    fn disabled_mode_never_starts_a_daemon() {
        let d = Devices::boot("quiet aftermath.udev=off");
        assert_eq!(d.state, "disabled");
        assert!(d.daemon.is_none() && d.task.is_none());
    }
}
