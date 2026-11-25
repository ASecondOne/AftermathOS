//! Supervised system D-Bus using the distribution's policy and service user.
//! Readiness requires a successful ListNames call, not just a socket file.

use std::collections::VecDeque;
use std::fs::{self, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::sys::signal::{kill, Signal};
use nix::sys::socket::{connect, socket, AddressFamily, SockFlag, SockType, UnixAddr};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::Pid;
use serde_json::{json, Value};

const START_LIMIT: usize = 5;
const START_WINDOW: Duration = Duration::from_secs(30);
const READY_TIMEOUT: Duration = Duration::from_secs(5);
const BOOT_LIMIT: Duration = Duration::from_secs(10);
const HEALTH_INTERVAL: Duration = Duration::from_secs(5);

struct Paths {
    daemon: PathBuf,
    probe: PathBuf,
    config: PathBuf,
    socket: PathBuf,
    canonical_id: PathBuf,
    legacy_id: PathBuf,
    log: PathBuf,
}

struct Probe {
    pid: i32,
    deadline: Instant,
    timed_out: bool,
    cancelled: bool,
}

pub struct Bus {
    paths: Paths,
    state: &'static str,
    pid: Option<i32>,
    socket_inode: Option<(u64, u64)>,
    probe: Option<Probe>,
    machine_id: Option<String>,
    responding: bool,
    error: Option<String>,
    starts: VecDeque<Instant>,
    restarts: u32,
    ready_deadline: Instant,
    next_action: Instant,
}

fn executable(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

fn kill_group(pid: i32) {
    let _ = kill(Pid::from_raw(-pid), Signal::SIGKILL);
    let _ = kill(Pid::from_raw(pid), Signal::SIGKILL);
}

/// Refuse a second bus or unexpected file. Only a refused, stale socket may
/// be removed. A nonblocking connection also handles a full listener backlog.
fn clear_stale_socket(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Ok(m) if m.file_type().is_socket() => {}
        other => {
            return Err(format!(
                "refusing to replace system bus path {}: {other:?}",
                path.display()
            ))
        }
    }
    let fd = socket(
        AddressFamily::Unix,
        SockType::Stream,
        SockFlag::SOCK_CLOEXEC | SockFlag::SOCK_NONBLOCK,
        None,
    )
    .map_err(|e| e.to_string())?;
    let addr = UnixAddr::new(path).map_err(|e| e.to_string())?;
    match connect(fd.as_raw_fd(), &addr) {
        Err(Errno::ECONNREFUSED) => fs::remove_file(path).map_err(|e| e.to_string()),
        Err(Errno::ENOENT) => Ok(()),
        Ok(()) | Err(Errno::EINPROGRESS | Errno::EAGAIN) => Err(format!(
            "another listener owns {}; refusing to start a second system bus",
            path.display()
        )),
        Err(e) => Err(format!("check system bus socket {}: {e}", path.display())),
    }
}

impl Bus {
    fn new(paths: Paths) -> Self {
        Self {
            paths,
            state: "starting",
            pid: None,
            socket_inode: None,
            probe: None,
            machine_id: None,
            responding: false,
            error: None,
            starts: VecDeque::new(),
            restarts: 0,
            ready_deadline: Instant::now(),
            next_action: Instant::now(),
        }
    }

    pub fn boot(cmdline: &str) -> Self {
        let mut bus = Self::new(Paths {
            daemon: aftermath_ctl::desktop::DBUS_DAEMON.into(),
            probe: aftermath_ctl::desktop::DBUS_SEND.into(),
            config: "/usr/share/dbus-1/system.conf".into(),
            socket: "/run/dbus/system_bus_socket".into(),
            canonical_id: "/etc/machine-id".into(),
            legacy_id: "/var/lib/dbus/machine-id".into(),
            log: "/run/aftermath/dbus.log".into(),
        });
        if cmdline
            .split_whitespace()
            .any(|a| a == "aftermath.dbus=off")
        {
            bus.state = "disabled";
            crate::log("system D-Bus disabled by aftermath.dbus=off");
            return bus;
        }
        if cmdline
            .split_whitespace()
            .any(|a| a == "aftermath.test=dbus-fail")
        {
            bus.paths.daemon = "/usr/bin/false".into();
        }
        if !executable(&bus.paths.daemon)
            || !executable(&bus.paths.probe)
            || !bus.paths.config.is_file()
        {
            bus.fail("missing", "dbus-daemon, dbus-send or the packaged system.conf is missing (cargo am rootfs --update)".into());
            return bus;
        }
        let prepared = (|| {
            let runtime = bus
                .paths
                .socket
                .parent()
                .ok_or("system bus socket has no parent")?;
            fs::create_dir_all(runtime).map_err(|e| e.to_string())?;
            fs::set_permissions(runtime, fs::Permissions::from_mode(0o755))
                .map_err(|e| e.to_string())?;
            clear_stale_socket(&bus.paths.socket)?;
            crate::machine_id::prepare(&bus.paths.canonical_id, &bus.paths.legacy_id)
        })();
        match prepared {
            Ok(id) => bus.machine_id = Some(id),
            Err(e) => {
                bus.fail("failed", e);
                return bus;
            }
        }
        bus.start();
        let deadline = Instant::now() + BOOT_LIMIT;
        while matches!(bus.state, "starting" | "stopping" | "restarting") {
            bus.reap_owned();
            bus.tick();
            if Instant::now() >= deadline {
                bus.stop_for_retry(
                    "system bus boot deadline reached; continuing to consoles".into(),
                );
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        bus
    }

    fn address(&self) -> String {
        format!("unix:path={}", self.paths.socket.display())
    }

    fn spawn(&self, program: &Path, args: &[String], quiet: bool) -> Result<i32, String> {
        fs::create_dir_all(self.paths.log.parent().ok_or("D-Bus log has no parent")?)
            .map_err(|e| e.to_string())?;
        let out = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.paths.log)
            .map_err(|e| e.to_string())?;
        let err = out.try_clone().map_err(|e| e.to_string())?;
        let mut cmd = Command::new(program);
        cmd.args(args)
            .env_clear()
            .env("PATH", "/usr/local/sbin:/usr/local/bin:/usr/bin")
            .stdin(Stdio::null())
            .stdout(if quiet {
                Stdio::null()
            } else {
                Stdio::from(out)
            })
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
            self.fail(
                "failed",
                "system bus start limit hit (5 starts in 30s)".into(),
            );
            return;
        }
        if let Err(e) = clear_stale_socket(&self.paths.socket) {
            self.fail("failed", e);
            return;
        }
        self.starts.push_back(now);
        self.socket_inode = None;
        let args = vec![
            format!("--config-file={}", self.paths.config.display()),
            "--nofork".into(),
            "--nopidfile".into(),
            "--nosyslog".into(),
            format!("--address={}", self.address()),
        ];
        match self.spawn(&self.paths.daemon, &args, false) {
            Ok(pid) => {
                self.pid = Some(pid);
                self.state = "starting";
                self.responding = false;
                self.ready_deadline = now + READY_TIMEOUT;
                self.next_action = now;
                self.error = None;
                crate::log(&format!("system D-Bus started in foreground (pid {pid})"));
            }
            Err(e) => self.schedule_restart(e),
        }
    }

    fn fail(&mut self, state: &'static str, error: String) {
        self.state = state;
        self.responding = false;
        crate::warn(&format!("system D-Bus: {error}; consoles remain available"));
        self.error = Some(error);
    }

    fn schedule_restart(&mut self, error: String) {
        self.restarts += 1;
        self.next_action = Instant::now() + Duration::from_secs(1);
        self.fail("restarting", format!("{error}; retrying in 1s"));
    }

    fn stop_for_retry(&mut self, error: String) {
        self.fail("stopping", error);
        if let Some(pid) = self.pid {
            kill_group(pid);
        }
        if let Some(probe) = &mut self.probe {
            probe.cancelled = true;
            kill_group(probe.pid);
        }
        if self.pid.is_none() {
            self.schedule_restart("system bus unavailable".into());
        }
    }

    pub fn tick(&mut self) {
        let now = Instant::now();
        if let Some(p) = &mut self.probe {
            if now >= p.deadline && !p.timed_out {
                p.timed_out = true;
                kill_group(p.pid);
            }
            return;
        }
        if self.state == "restarting" && now >= self.next_action {
            self.start();
        }
        if self.pid.is_none()
            || !matches!(self.state, "starting" | "running")
            || now < self.next_action
        {
            return;
        }
        if self.state == "starting" && now >= self.ready_deadline {
            self.stop_for_retry("system bus did not answer ListNames within 5s".into());
            return;
        }
        let args = vec![
            format!("--bus={}", self.address()),
            "--print-reply".into(),
            "--reply-timeout=1000".into(),
            "--dest=org.freedesktop.DBus".into(),
            "/org/freedesktop/DBus".into(),
            "org.freedesktop.DBus.ListNames".into(),
        ];
        match self.spawn(&self.paths.probe, &args, true) {
            Ok(pid) => {
                self.probe = Some(Probe {
                    pid,
                    deadline: now + Duration::from_secs(2),
                    timed_out: false,
                    cancelled: false,
                })
            }
            Err(e) => self.stop_for_retry(e),
        }
    }

    /// Before the engine event loop only: reap our own daemon and probe.
    fn reap_owned(&mut self) {
        for pid in [self.pid, self.probe.as_ref().map(|p| p.pid)]
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

    pub fn child_exited(&mut self, pid: i32, status: &WaitStatus) -> bool {
        if self.pid == Some(pid) {
            self.pid = None;
            // An unaccepted Unix connection can retain the old listener
            // after its process exits. Unlink only the socket observed on
            // our healthy daemon, never a replacement created by another bus.
            if let Some(identity) = self.socket_inode.take() {
                if fs::symlink_metadata(&self.paths.socket)
                    .is_ok_and(|m| m.file_type().is_socket() && (m.dev(), m.ino()) == identity)
                {
                    if let Err(e) = fs::remove_file(&self.paths.socket) {
                        crate::warn(&format!("remove exited system bus socket: {e}"));
                    }
                }
            }
            let _ = kill(Pid::from_raw(-pid), Signal::SIGKILL);
            if let Some(p) = &mut self.probe {
                p.cancelled = true;
                kill_group(p.pid);
            }
            self.schedule_restart(format!("daemon exited: {status:?}"));
            return true;
        }
        if !self.probe.as_ref().is_some_and(|p| p.pid == pid) {
            return false;
        }
        let probe = self.probe.take().expect("matched probe");
        if probe.cancelled {
            return true;
        }
        if !probe.timed_out && matches!(status, WaitStatus::Exited(_, 0)) {
            if !self.responding {
                self.socket_inode = fs::symlink_metadata(&self.paths.socket)
                    .ok()
                    .filter(|m| m.file_type().is_socket())
                    .map(|m| (m.dev(), m.ino()));
            }
            if !self.responding {
                crate::log("system D-Bus ready (ListNames answered)");
            }
            self.state = "running";
            self.responding = true;
            self.error = None;
            self.next_action = Instant::now() + HEALTH_INTERVAL;
        } else if self.state == "starting" && Instant::now() < self.ready_deadline {
            self.next_action = Instant::now() + Duration::from_millis(100);
        } else {
            self.stop_for_retry(format!(
                "ListNames probe {} ({status:?})",
                if probe.timed_out {
                    "timed out"
                } else {
                    "failed"
                }
            ));
        }
        true
    }

    pub fn status(&self) -> Value {
        json!({"state": self.state, "pid": self.pid, "responding": self.responding,
            "provider": self.paths.daemon, "address": self.address(), "config": self.paths.config,
            "machine_id": self.machine_id, "restarts": self.restarts, "error": self.error,
            "log": self.paths.log, "session_bus": "graphical sessions use dbus-run-session; no systemd user manager",
            "systemd_activation": false})
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Harness {
        bus: Bus,
        root: PathBuf,
    }
    impl Harness {
        fn new(scenario: &str) -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let root = std::env::temp_dir().join(format!(
                "am-bus-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&root).unwrap();
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
            fs::write(root.join("scenario"), scenario).unwrap();
            for name in ["daemon", "probe"] {
                crate::test_support::install(&root.join(name));
            }
            fs::write(root.join("system.conf"), "unused by the fake daemon").unwrap();
            let mut bus = Bus::new(Paths {
                daemon: root.join("daemon"),
                probe: root.join("probe"),
                config: root.join("system.conf"),
                socket: root.join("bus"),
                canonical_id: root.join("machine-id"),
                legacy_id: root.join("legacy-id"),
                log: root.join("bus.log"),
            });
            bus.machine_id = Some(
                crate::machine_id::prepare(&bus.paths.canonical_id, &bus.paths.legacy_id).unwrap(),
            );
            Self { bus, root }
        }
        fn until(&mut self, predicate: impl Fn(&Bus) -> bool) {
            let deadline = Instant::now() + Duration::from_secs(4);
            loop {
                self.bus.reap_owned();
                self.bus.tick();
                if predicate(&self.bus) {
                    return;
                }
                assert!(
                    Instant::now() < deadline,
                    "timed out: {}",
                    self.bus.status()
                );
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }
    impl Drop for Harness {
        fn drop(&mut self) {
            for pid in [self.bus.pid, self.bus.probe.as_ref().map(|p| p.pid)]
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
    fn failed_readiness_never_reports_a_healthy_bus() {
        let mut h = Harness::new("fail");
        h.bus.start();
        assert!(!h.bus.responding);
        h.until(|b| b.probe.is_some());
        h.bus.ready_deadline = Instant::now();
        h.until(|b| b.state == "restarting");
        assert!(!h.bus.responding);
        assert!(h.bus.pid.is_none());
        assert_eq!(h.bus.restarts, 1);
    }

    #[test]
    fn daemon_restarts_with_the_same_identity_and_hits_start_limit() {
        let mut h = Harness::new("success");
        h.bus.start();
        h.until(|b| b.state == "running");
        let first = h.bus.pid.unwrap();
        let id = h.bus.machine_id.clone();
        kill_group(first);
        h.until(|b| b.state == "restarting");
        assert!(!h.bus.responding);
        h.bus.next_action = Instant::now();
        h.until(|b| b.state == "running");
        assert_ne!(h.bus.pid.unwrap(), first);
        assert_eq!(h.bus.machine_id, id);
        h.bus.starts = (0..START_LIMIT).map(|_| Instant::now()).collect();
        kill_group(h.bus.pid.unwrap());
        h.until(|b| b.state == "restarting");
        h.bus.next_action = Instant::now();
        h.until(|b| b.state == "failed");
        assert!(h.bus.error.as_deref().unwrap().contains("start limit"));
        assert!(h.bus.pid.is_none());
    }

    #[test]
    fn hung_health_probe_is_killed_and_the_bus_is_restarted() {
        let mut h = Harness::new("stall-health");
        h.bus.start();
        h.until(|b| b.state == "running");
        h.bus.next_action = Instant::now();
        h.until(|b| b.probe.is_some());
        h.bus.probe.as_mut().unwrap().deadline = Instant::now();
        h.until(|b| b.state == "restarting");
        assert!(!h.bus.responding);
        assert!(h.bus.pid.is_none() && h.bus.probe.is_none());
    }

    #[test]
    fn refuses_live_listeners_and_regular_files_but_removes_stale_sockets() {
        let h = Harness::new("success");
        let path = &h.bus.paths.socket;
        let listener = UnixListener::bind(path).unwrap();
        assert!(clear_stale_socket(path)
            .unwrap_err()
            .contains("another listener"));
        assert!(path.exists());
        // Drain the connection made by the live-listener check before
        // closing the listener; queued Unix connections can retain it.
        let (accepted, _) = listener.accept().unwrap();
        drop(accepted);
        drop(listener);
        // Other parallel tests may briefly fork with the CLOEXEC listener
        // still inherited before exec. Refusing it until then is correct.
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match clear_stale_socket(path) {
                Ok(()) => break,
                Err(e) => {
                    assert!(
                        e.contains("another listener") && Instant::now() < deadline,
                        "{e}"
                    );
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        }
        assert!(!path.exists());
        fs::write(path, "keep this file").unwrap();
        assert!(clear_stale_socket(path).is_err());
        assert_eq!(fs::read_to_string(path).unwrap(), "keep this file");
    }

    #[test]
    fn reaped_daemon_cleanup_only_unlinks_its_observed_socket() {
        for replace in [false, true] {
            let mut h = Harness::new("success");
            h.bus.start();
            let listener = UnixListener::bind(&h.bus.paths.socket).unwrap();
            h.until(|b| b.state == "running");
            let replacement = if replace {
                fs::remove_file(&h.bus.paths.socket).unwrap();
                Some(UnixListener::bind(&h.bus.paths.socket).unwrap())
            } else {
                None
            };
            kill_group(h.bus.pid.unwrap());
            h.until(|b| b.state == "restarting");
            assert_eq!(h.bus.paths.socket.exists(), replace);
            drop(replacement);
            drop(listener);
        }
    }

    #[test]
    fn disabled_bus_does_not_touch_machine_identity_or_spawn_children() {
        let b = Bus::boot("quiet aftermath.dbus=off");
        assert_eq!(b.state, "disabled");
        assert!(b.pid.is_none() && b.probe.is_none() && b.machine_id.is_none());
    }
}
