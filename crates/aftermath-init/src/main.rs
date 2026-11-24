//! aftermath-init: the Aftermath PID 1.
//!
//! PID 1 is deliberately small and boring, because if it dies the kernel
//! panics. All real logic (services, strata, marking the boot as good) lives
//! in a separate process, `aftermath-engine`, which PID 1 supervises.
//!
//! Fallback chain (Tim's "always a plan B"):
//!
//! 1. **full**     engine with strata + services             (default)
//! 2. **host**     engine in plain Arch host mode, no strata (engine crashed
//!    3x in full mode, or `aftermath.mode=host`)
//! 3. **busybox**  static BusyBox shell on every console, data partitions
//!    mounted read-only under /mnt (engine crashed 3x in host mode, or
//!    `aftermath.mode=busybox` / `aftermath.rescue`)
//! 4. **last resort** if PID 1 itself ever panics, the panic is caught and
//!    it drops straight into step 3 instead of killing the kernel.
//!
//! The same binary doubles as `poweroff` / `reboot` / `halt` (by argv[0]
//! or as a subcommand) and signals PID 1, like sysvinit does.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::symlink;
use std::os::unix::process::CommandExt;
use std::panic::{self, AssertUnwindSafe};
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::mount::{mount, MsFlags};
use nix::sys::reboot::{reboot, set_cad_enabled, RebootMode};
use nix::sys::signal::{kill, sigprocmask, SigSet, SigmaskHow, Signal};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::{getpid, sethostname, sync, Pid};

const ENGINE: &str = "/usr/lib/aftermath/aftermath-engine";
const BUSYBOX: &[&str] = &["/usr/lib/aftermath/rescue/busybox", "/usr/bin/busybox"];
const BASH: &str = "/usr/bin/bash";
const RESCUE_BIN: &str = "/run/aftermath/rescue/bin";
/// Engine crashes in a row before PID 1 drops to the next fallback layer.
const MAX_FAILS: u32 = 3;
/// An engine that ran this long before dying resets the failure counter.
const STABLE_AFTER: Duration = Duration::from_secs(60);

// Colours: Aftermath is red, purple and black.
const RED: &str = "\x1b[1;31m";
const PURPLE: &str = "\x1b[1;35m";
const RESET: &str = "\x1b[0m";

fn log(msg: &str) {
    log_at(6, msg);
}

/// Fallbacks and failures: kernel level 3 (error), so they are visible on
/// the console even with the quiet default `loglevel=4`.
fn warn(msg: &str) {
    log_at(3, msg);
}

fn log_at(level: u8, msg: &str) {
    // One write() per message: every write to /dev/kmsg is one log record,
    // and it shows up on all kernel consoles (serial + screen).
    let line = format!("<{level}>aftermath-init: {msg}\n");
    match OpenOptions::new().write(true).open("/dev/kmsg") {
        Ok(mut k) => {
            let _ = k.write_all(line.as_bytes());
        }
        Err(_) => {
            let _ = std::io::stderr().write_all(line.as_bytes());
        }
    }
}

fn main() {
    let argv0 = std::env::args().next().unwrap_or_default();
    let name = Path::new(&argv0)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_owned();
    let sub = std::env::args().nth(1);

    let action = match (name.as_str(), sub.as_deref()) {
        ("poweroff", _) | (_, Some("poweroff")) => Some(Signal::SIGUSR2),
        ("halt", _) | (_, Some("halt")) => Some(Signal::SIGUSR1),
        ("reboot", _) | (_, Some("reboot")) => Some(Signal::SIGINT),
        _ => None,
    };

    if let Some(sig) = action {
        if let Err(e) = kill(Pid::from_raw(1), sig) {
            eprintln!("failed to signal PID 1: {e}");
            std::process::exit(1);
        }
        return;
    }

    if getpid().as_raw() != 1 {
        eprintln!(
            "aftermath-init must run as PID 1.\n\
             On a running Aftermath system use: aftermath-init {{poweroff|reboot|halt}}"
        );
        std::process::exit(1);
    }

    pid1();
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Full,
    Host,
    Busybox,
}

impl Mode {
    fn as_str(self) -> &'static str {
        match self {
            Mode::Full => "full",
            Mode::Host => "host",
            Mode::Busybox => "busybox",
        }
    }
}

fn handled_signals() -> SigSet {
    let mut s = SigSet::empty();
    for sig in [
        Signal::SIGCHLD,
        Signal::SIGTERM,
        Signal::SIGINT,
        Signal::SIGUSR1,
        Signal::SIGUSR2,
        Signal::SIGHUP,
    ] {
        s.add(sig);
    }
    s
}

fn pid1() -> ! {
    early_root();
    let handled = handled_signals();
    let _ = sigprocmask(SigmaskHow::SIG_BLOCK, Some(&handled), None);
    panic::set_hook(Box::new(|info| warn(&format!("PANIC in PID 1: {info}"))));

    let r = panic::catch_unwind(AssertUnwindSafe(|| {
        mount_api_filesystems();
        log(&format!("Aftermath {} starting", env!("CARGO_PKG_VERSION")));
        let _ = set_cad_enabled(false); // Ctrl-Alt-Del -> SIGINT to us
        set_hostname();
        supervise(&handled)
    }));
    // Only reachable if supervise() panicked.
    let _ = r;
    warn("PID 1 recovered from a panic: entering last-resort BusyBox rescue");
    let r = panic::catch_unwind(AssertUnwindSafe(|| {
        let mut sup = Supervisor::new(Mode::Busybox);
        sup.enter_rescue("PID 1 panic");
        sup.event_loop(&handled)
    }));
    let _ = r;
    // Absolute last resort: just keep reaping so the kernel never panics.
    loop {
        let _ = waitpid(Pid::from_raw(-1), None);
        sleep(Duration::from_secs(1));
    }
}

/// Hand over to aftermath-early once per boot (same PID, via exec): it sets
/// up the persistent host root (overlay disk) and execs us again with
/// AFTERMATH_EARLY_DONE=1. If it is missing or the exec fails we simply go
/// on with the RAM root.
fn early_root() {
    const EARLY: &str = "/usr/lib/aftermath/aftermath-early";
    if std::env::var_os("AFTERMATH_EARLY_DONE").is_some() || !Path::new(EARLY).exists() {
        return;
    }
    let err = Command::new(EARLY).exec();
    eprintln!("aftermath-init: exec {EARLY} failed: {err}; using the RAM root");
}

fn boot_mode() -> Mode {
    let cmdline = fs::read_to_string("/proc/cmdline").unwrap_or_default();
    let mut mode = Mode::Full;
    for arg in cmdline.split_whitespace() {
        match arg {
            "aftermath.mode=host" => mode = Mode::Host,
            "aftermath.mode=busybox" | "aftermath.rescue" => mode = Mode::Busybox,
            _ => {}
        }
    }
    mode
}

fn supervise(handled: &SigSet) -> ! {
    let mode = boot_mode();
    let mut sup = Supervisor::new(mode);
    if mode == Mode::Busybox {
        sup.enter_rescue("requested on the kernel command line");
    } else {
        sup.start_engine_or_fall_back();
    }
    sup.event_loop(handled)
}

struct Supervisor {
    mode: Mode,
    fails: u32,
    engine: Option<(i32, Instant)>,
    /// Rescue shells: pid -> console.
    shells: HashMap<i32, String>,
}

impl Supervisor {
    fn new(mode: Mode) -> Self {
        Self {
            mode,
            fails: 0,
            engine: None,
            shells: HashMap::new(),
        }
    }

    fn event_loop(&mut self, handled: &SigSet) -> ! {
        loop {
            let Ok(sig) = handled.wait() else { continue };
            match sig {
                Signal::SIGCHLD => self.reap(),
                Signal::SIGUSR2 | Signal::SIGTERM => shutdown(RebootMode::RB_POWER_OFF),
                Signal::SIGINT => shutdown(RebootMode::RB_AUTOBOOT),
                Signal::SIGUSR1 => shutdown(RebootMode::RB_HALT_SYSTEM),
                _ => {}
            }
        }
    }

    fn reap(&mut self) {
        loop {
            let st = match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
                Ok(WaitStatus::StillAlive) | Err(_) => return,
                Ok(st) => st,
            };
            let Some(pid) = st.pid().map(|p| p.as_raw()) else {
                continue;
            };
            if self.engine.is_some_and(|(p, _)| p == pid) {
                let ran = self
                    .engine
                    .take()
                    .map(|(_, t)| t.elapsed())
                    .unwrap_or_default();
                self.engine_died(&st, ran);
            } else if let Some(tty) = self.shells.remove(&pid) {
                log(&format!("rescue shell on {tty} exited, respawning"));
                sleep(Duration::from_millis(300));
                if let Some(p) = spawn_rescue_shell(&tty) {
                    self.shells.insert(p, tty);
                }
            }
        }
    }

    fn engine_died(&mut self, st: &WaitStatus, ran: Duration) {
        let how = match st {
            WaitStatus::Exited(_, code) => format!("exited with status {code}"),
            WaitStatus::Signaled(_, sig, _) => format!("was killed by {sig:?}"),
            other => format!("stopped ({other:?})"),
        };
        if ran > STABLE_AFTER {
            self.fails = 0;
        }
        self.fails += 1;
        warn(&format!(
            "engine ({} mode) {how} after {}s [failure {}/{MAX_FAILS}]",
            self.mode.as_str(),
            ran.as_secs(),
            self.fails
        ));
        // Clean slate: whatever the engine started (shells, services) goes.
        let _ = kill(Pid::from_raw(-1), Signal::SIGKILL);
        while matches!(
            waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)),
            Ok(WaitStatus::Exited(..) | WaitStatus::Signaled(..))
        ) {}
        sleep(Duration::from_secs(1));
        self.start_engine_or_fall_back();
    }

    /// Start the engine; on repeated failure step down the fallback chain.
    fn start_engine_or_fall_back(&mut self) {
        loop {
            if self.fails >= MAX_FAILS {
                match self.mode {
                    Mode::Full => {
                        warn("engine keeps failing: FALLBACK to plain Arch host mode (no strata)");
                        self.mode = Mode::Host;
                        self.fails = 0;
                    }
                    _ => {
                        self.enter_rescue("engine keeps failing in Arch host mode");
                        return;
                    }
                }
            }
            match spawn_engine(self.mode) {
                Ok(pid) => {
                    log(&format!(
                        "engine started in {} mode (pid {pid})",
                        self.mode.as_str()
                    ));
                    self.engine = Some((pid, Instant::now()));
                    return;
                }
                Err(e) => {
                    self.fails += 1;
                    warn(&format!(
                        "cannot start {ENGINE}: {e} [failure {}/{MAX_FAILS}]",
                        self.fails
                    ));
                    sleep(Duration::from_millis(500));
                }
            }
        }
    }

    fn enter_rescue(&mut self, why: &str) {
        self.mode = Mode::Busybox;
        warn(&format!(
            "RESCUE: {why} -> BusyBox shell, data mounted read-only under /mnt"
        ));
        mount_data_read_only();
        prepare_busybox_applets();
        for tty in active_consoles() {
            if let Some(pid) = spawn_rescue_shell(&tty) {
                self.shells.insert(pid, tty);
            }
        }
    }
}

fn spawn_engine(mode: Mode) -> std::io::Result<i32> {
    let mut cmd = Command::new(ENGINE);
    cmd.args(["--mode", mode.as_str()])
        .env_clear()
        .env("PATH", "/usr/local/sbin:/usr/local/bin:/usr/bin")
        .stdin(Stdio::null());
    unblock_signals_in_child(&mut cmd);
    cmd.spawn().map(|c| c.id() as i32)
}

fn unblock_signals_in_child(cmd: &mut Command) {
    // SAFETY: only async-signal-safe libc calls between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            let empty: libc::sigset_t = std::mem::zeroed();
            libc::sigprocmask(libc::SIG_SETMASK, &empty, std::ptr::null_mut());
            Ok(())
        });
    }
}

// ------------------------------------------------------------- rescue ----

fn busybox() -> Option<&'static str> {
    BUSYBOX.iter().copied().find(|p| Path::new(p).exists())
}

/// `busybox --list` -> symlinks in /run/aftermath/rescue/bin, so `ls`,
/// `mount`, `cp` ... work even if the Arch userland is broken.
fn prepare_busybox_applets() {
    let Some(bb) = busybox() else {
        log("no BusyBox found (install `busybox`); rescue falls back to bash");
        return;
    };
    let _ = fs::create_dir_all(RESCUE_BIN);
    let Ok(out) = Command::new(bb).arg("--list").output() else {
        return;
    };
    for applet in String::from_utf8_lossy(&out.stdout).split_whitespace() {
        let _ = symlink(bb, Path::new(RESCUE_BIN).join(applet));
    }
}

/// Mount every partition we can recognise read-only under /mnt/<name>.
fn mount_data_read_only() {
    const FSTYPES: &[&str] = &["ext4", "btrfs", "xfs", "vfat", "exfat", "ntfs3", "iso9660"];
    let Ok(rd) = fs::read_dir("/sys/class/block") else {
        return;
    };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if ["loop", "ram", "zram", "dm-"]
            .iter()
            .any(|p| name.starts_with(p))
        {
            continue;
        }
        // Skip empty devices (no CD inserted) and whole disks with partitions.
        let sys = e.path();
        let empty = fs::read_to_string(sys.join("size"))
            .map(|s| s.trim() == "0")
            .unwrap_or(true);
        let has_parts = fs::read_dir(&sys)
            .map(|rd| rd.flatten().any(|c| c.path().join("partition").exists()))
            .unwrap_or(false);
        if empty || has_parts {
            continue;
        }
        let dev = format!("/dev/{name}");
        let target = format!("/mnt/{name}");
        let _ = fs::create_dir_all(&target);
        let ok = FSTYPES.iter().any(|fs_type| {
            mount(
                Some(dev.as_str()),
                target.as_str(),
                Some(*fs_type),
                MsFlags::MS_RDONLY,
                None::<&str>,
            )
            .is_ok()
        });
        if ok {
            warn(&format!("rescue: {dev} mounted read-only on {target}"));
        } else {
            let _ = fs::remove_dir(&target);
        }
    }
}

fn spawn_rescue_shell(tty: &str) -> Option<i32> {
    let dev = format!("/dev/{tty}");
    let open = || File::options().read(true).write(true).open(&dev);
    let (Ok(i), Ok(o), Ok(e)) = (open(), open(), open()) else {
        log(&format!("cannot open {dev}"));
        return None;
    };
    let banner = format!(
        "\n{RED}*** AFTERMATH RESCUE ***{RESET}  {PURPLE}BusyBox CLI{RESET} \
         (data is read-only under /mnt; `mount -o remount,rw` to change it)\n\
         poweroff / reboot when done.\n\n"
    );
    if let Ok(mut f) = OpenOptions::new().write(true).open(&dev) {
        let _ = f.write_all(banner.as_bytes());
    }
    let mut cmd = match busybox() {
        Some(bb) => {
            let mut c = Command::new(bb);
            c.arg("sh");
            c
        }
        None if Path::new(BASH).exists() => Command::new(BASH),
        None => {
            log("no shell available for rescue (neither busybox nor bash)");
            return None;
        }
    };
    cmd.env_clear()
        .env("HOME", "/root")
        .env(
            "TERM",
            if tty.starts_with("ttyS") {
                "vt220"
            } else {
                "linux"
            },
        )
        .env("PATH", format!("{RESCUE_BIN}:/usr/bin"))
        .env("PS1", "aftermath-rescue# ")
        .stdin(Stdio::from(i))
        .stdout(Stdio::from(o))
        .stderr(Stdio::from(e));
    // SAFETY: only async-signal-safe libc calls between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            let empty: libc::sigset_t = std::mem::zeroed();
            libc::sigprocmask(libc::SIG_SETMASK, &empty, std::ptr::null_mut());
            libc::setsid();
            libc::ioctl(0, libc::TIOCSCTTY, 1);
            Ok(())
        });
    }
    match cmd.spawn() {
        Ok(c) => {
            log(&format!("rescue shell started on {dev}"));
            Some(c.id() as i32)
        }
        Err(e) => {
            log(&format!("failed to start rescue shell on {dev}: {e}"));
            None
        }
    }
}

/// Every console the kernel writes to (`console=`), as device names.
/// `tty0` means "the current VT", so a shell goes to tty1.
fn active_consoles() -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let active = fs::read_to_string("/sys/class/tty/console/active").unwrap_or_default();
    for name in active.split_whitespace() {
        let name = if name == "tty0" { "tty1" } else { name };
        if !out.iter().any(|n| n == name) && Path::new("/dev").join(name).exists() {
            out.push(name.to_owned());
        }
    }
    if out.is_empty() {
        out.push("console".into());
    }
    out
}

// --------------------------------------------------------- early boot ----

fn try_mount(src: &str, target: &str, fstype: &str, flags: MsFlags, data: Option<&str>) {
    let _ = fs::create_dir_all(target);
    match mount(Some(src), target, Some(fstype), flags, data) {
        Ok(()) | Err(Errno::EBUSY) => {}
        Err(e) => log(&format!("mount {target} ({fstype}) failed: {e}")),
    }
}

fn mount_api_filesystems() {
    let nsne = MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC;
    let nsnd = MsFlags::MS_NOSUID | MsFlags::MS_NODEV;
    try_mount("proc", "/proc", "proc", nsne, None);
    try_mount("sysfs", "/sys", "sysfs", nsne, None);
    try_mount(
        "devtmpfs",
        "/dev",
        "devtmpfs",
        MsFlags::MS_NOSUID,
        Some("mode=0755"),
    );
    try_mount(
        "devpts",
        "/dev/pts",
        "devpts",
        MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC,
        Some("gid=5,mode=620,ptmxmode=000"),
    );
    try_mount("tmpfs", "/dev/shm", "tmpfs", nsnd, Some("mode=1777"));
    try_mount("tmpfs", "/run", "tmpfs", nsnd, Some("mode=0755"));
    try_mount("tmpfs", "/tmp", "tmpfs", nsnd, Some("mode=1777"));
    try_mount("cgroup2", "/sys/fs/cgroup", "cgroup2", nsne, None);
    // Kernels with a real root= may hand us a read-only root.
    let _ = mount(
        None::<&str>,
        "/",
        None::<&str>,
        MsFlags::MS_REMOUNT,
        None::<&str>,
    );
}

fn set_hostname() {
    let name = fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "aftermath".into());
    if let Err(e) = sethostname(&name) {
        log(&format!("sethostname failed: {e}"));
    }
}

fn shutdown(mode: RebootMode) -> ! {
    log("shutting down: sending SIGTERM to all processes");
    let _ = kill(Pid::from_raw(-1), Signal::SIGTERM);
    sleep(Duration::from_secs(2));
    let _ = kill(Pid::from_raw(-1), Signal::SIGKILL);
    while waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG))
        .map(|s| s != WaitStatus::StillAlive)
        .unwrap_or(false)
    {}
    sync();
    // Best effort: everything read-only (deepest mounts first: the strata
    // and host overlay disks), then / itself.
    let mounts = fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
    for m in mounts.lines().rev().filter_map(|l| l.split(' ').nth(4)) {
        let _ = mount(
            None::<&str>,
            m,
            None::<&str>,
            MsFlags::MS_REMOUNT | MsFlags::MS_RDONLY,
            None::<&str>,
        );
    }
    let _ = mount(
        None::<&str>,
        "/",
        None::<&str>,
        MsFlags::MS_REMOUNT | MsFlags::MS_RDONLY,
        None::<&str>,
    );
    sync();
    log("bye");
    let _ = reboot(mode);
    // reboot() only returns on error; PID 1 must never exit.
    loop {
        sleep(Duration::from_secs(3600));
    }
}
