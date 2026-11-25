//! Root shells on every active console, respawned when they exit.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::Duration;

use crate::{log, PURPLE, RED, RESET};

const SHELL: &str = "/usr/bin/bash";
/// util-linux `script`: runs the shell on a pty and copies the console output
/// into the session log (see logs.rs).
const SCRIPT: &str = "/usr/bin/script";

#[derive(Default)]
pub struct Shells {
    by_pid: HashMap<i32, String>,
    mode: String,
    log_dir: Option<PathBuf>,
    quick_deaths: u32,
}

impl Shells {
    /// Start a shell on every console. False if none could be started.
    pub fn start_all(&mut self, mode: &str, log_dir: Option<PathBuf>) -> bool {
        self.mode = mode.to_owned();
        self.log_dir = log_dir;
        for tty in active_consoles() {
            if let Some(pid) = spawn(&tty, mode, self.log_dir.as_deref()) {
                self.by_pid.insert(pid, tty);
            }
        }
        !self.by_pid.is_empty()
    }

    /// A child exited: respawn it if it was a console shell (true).
    pub fn child_exited(&mut self, pid: i32) -> bool {
        let Some(tty) = self.by_pid.remove(&pid) else {
            return false;
        };
        log(&format!("shell on {tty} exited, respawning"));
        sleep(Duration::from_millis(300));
        match spawn(&tty, &self.mode, self.log_dir.as_deref()) {
            Some(p) => {
                self.by_pid.insert(p, tty);
            }
            None => self.quick_deaths += 1,
        }
        if self.by_pid.is_empty() && self.quick_deaths > 5 {
            log("shells cannot be started any more - exiting so PID 1 can fall back");
            std::process::exit(4);
        }
        true
    }
}

/// `console=` devices; `tty0` (= current VT) becomes tty1.
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

fn spawn(tty: &str, mode: &str, log_dir: Option<&Path>) -> Option<i32> {
    let dev = format!("/dev/{tty}");
    let open = || File::options().read(true).write(true).open(&dev);
    let (Ok(i), Ok(o), Ok(e)) = (open(), open(), open()) else {
        log(&format!("cannot open {dev}"));
        return None;
    };
    let note = if mode == "host" {
        format!("  {RED}[Arch host mode: strata disabled]{RESET}")
    } else {
        String::new()
    };
    let root_note = root_note();
    let log_file = log_dir.map(|d| d.join(format!("{tty}.log")));
    let log_note = match &log_file {
        Some(f) if Path::new(SCRIPT).exists() => {
            format!("{PURPLE}This console is logged to {}{RESET}\n", f.display())
        }
        _ => String::new(),
    };
    let banner = format!(
        "\n{RED}AFTERMATH{RESET} {PURPLE}(experimental){RESET}{note}\n\
         Root shell, no login yet. `amctl status` for an overview, `poweroff` / `reboot` to leave.\n{root_note}{log_note}\n"
    );
    if let Ok(mut f) = OpenOptions::new().write(true).open(&dev) {
        let _ = f.write_all(banner.as_bytes());
    }
    let mut cmd = shell_command(log_file.as_deref());
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
        .env("PATH", path_for(mode))
        .env("USER", "root")
        .env("SHELL", SHELL)
        .env("AFTERMATH_MODE", mode)
        .current_dir("/root")
        .stdin(Stdio::from(i))
        .stdout(Stdio::from(o))
        .stderr(Stdio::from(e));
    if mode == "full" {
        // Strata .desktop files (overlaid by aftermath-strata). No MANPATH:
        // Arch's /etc/profile unsets it; see README (man pages).
        cmd.env(
            "XDG_DATA_DIRS",
            "/usr/local/share:/usr/share:/aftermath/cross/share",
        );
        // bash remembers where it found a command (`hash`). Forget that
        // before every prompt, so `amctl default` and new launchers take
        // effect in shells that are already open. Exported, so nested
        // shells get it too; Arch's bash.bashrc only appends to it.
        cmd.env("PROMPT_COMMAND", "hash -r");
    }
    // SAFETY: only async-signal-safe libc calls between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            libc::ioctl(0, libc::TIOCSCTTY, 1);
            Ok(())
        });
    }
    match cmd.spawn() {
        Ok(c) => {
            log(&format!("root shell started on {dev}"));
            Some(c.id() as i32)
        }
        Err(e) => {
            log(&format!("failed to start {SHELL} on {dev}: {e}"));
            None
        }
    }
}

/// Host commands win, except commands pinned to another stratum
/// (/aftermath/cross/pin, e.g. `apt` -> ubuntu); commands only another
/// stratum has come last (/aftermath/cross/bin). Arch's /etc/profile only
/// appends to PATH, so this survives `bash -l`.
fn path_for(mode: &str) -> &'static str {
    if mode == "full" {
        "/aftermath/cross/pin:/usr/local/sbin:/usr/local/bin:/usr/bin:/aftermath/cross/bin"
    } else {
        "/usr/local/sbin:/usr/local/bin:/usr/bin"
    }
}

/// `bash -l`, wrapped in `script` (flushes every write, appends across
/// respawns, passes the exit status through) when there is a session log.
fn shell_command(log_file: Option<&Path>) -> Command {
    match log_file {
        Some(f) if Path::new(SCRIPT).exists() => {
            let mut c = Command::new(SCRIPT);
            c.args(["--quiet", "--flush", "--append", "--return", "--log-out"])
                .arg(f)
                .args(["--command", &format!("{SHELL} -l")]);
            c
        }
        _ => {
            let mut c = Command::new(SHELL);
            c.arg("-l");
            c
        }
    }
}

/// Is / the persistent overlay (aftermath-early) or the plain RAM image?
fn root_note() -> String {
    let info = fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
    let overlay = info.lines().any(|l| {
        let mut halves = l.split(" - ");
        let left = halves.next().unwrap_or("");
        let fstype = halves
            .next()
            .and_then(|r| r.split(' ').next())
            .unwrap_or("");
        left.split(' ').nth(4) == Some("/") && fstype == "overlay"
    });
    if overlay {
        format!("{PURPLE}Changes to the system are saved (host overlay disk).{RESET}\n")
    } else {
        format!("{RED}RAM root: changes to the system are lost at poweroff.{RESET}\n")
    }
}
