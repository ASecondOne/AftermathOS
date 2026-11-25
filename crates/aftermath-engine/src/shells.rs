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
    by_pid: HashMap<i32, Console>,
    mode: String,
    log_dir: Option<PathBuf>,
    quick_deaths: u32,
}

struct Console {
    tty: String,
    login: bool,
}

impl Shells {
    /// Start a shell on every console. False if none could be started.
    pub fn start_all(&mut self, mode: &str, log_dir: Option<PathBuf>) -> bool {
        self.mode = mode.to_owned();
        self.log_dir = log_dir;
        let login_ttys = aftermath_ctl::sessions::login_ttys(Path::new("/")).unwrap_or_else(|e| {
            crate::warn(&format!("login terminal configuration: {e}"));
            Vec::new()
        });
        for tty in active_consoles() {
            if login_ttys.contains(&tty) {
                continue;
            }
            if let Some(pid) = spawn(&tty, mode, self.log_dir.as_deref()) {
                self.by_pid.insert(pid, Console { tty, login: false });
            }
        }
        for tty in login_ttys {
            if let Some(pid) = spawn_login(&tty) {
                self.by_pid.insert(pid, Console { tty, login: true });
            }
        }
        !self.by_pid.is_empty()
    }

    /// A child exited: respawn it if it was a console shell (true).
    pub fn child_exited(&mut self, pid: i32) -> bool {
        let Some(console) = self.by_pid.remove(&pid) else {
            return false;
        };
        log(&format!("console on {} exited, respawning", console.tty));
        sleep(Duration::from_millis(300));
        let spawned = if console.login {
            spawn_login(&console.tty)
        } else {
            spawn(&console.tty, &self.mode, self.log_dir.as_deref())
        };
        match spawned {
            Some(p) => {
                self.by_pid.insert(p, console);
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

/// Never wrap login in the console recorder: authentication input must not
/// become part of the development root-shell transcript.
fn spawn_login(tty: &str) -> Option<i32> {
    let path = format!("/dev/{tty}");
    let result = (|| -> std::io::Result<_> {
        let input = File::options().read(true).write(true).open(&path)?;
        let output = input.try_clone()?;
        let error = input.try_clone()?;
        let mut cmd = Command::new("/usr/bin/agetty");
        cmd.args([
            "--noclear",
            "--login-program",
            "/usr/bin/login",
            "-",
            "linux",
        ])
        .env_clear()
        .env("PATH", "/usr/local/sbin:/usr/local/bin:/usr/bin")
        .env("TERM", "linux")
        .stdin(Stdio::from(input))
        .stdout(Stdio::from(output))
        .stderr(Stdio::from(error));
        // SAFETY: only async-signal-safe terminal/session syscalls after fork.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        cmd.spawn()
    })();
    match result {
        Ok(child) => {
            log(&format!("authenticated login available on {path}"));
            Some(child.id() as i32)
        }
        Err(e) => {
            crate::warn(&format!("cannot start login on {path}: {e}"));
            None
        }
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
         Development root shell. `amctl status` for an overview, `poweroff` / `reboot` to leave.\n{root_note}{log_note}\n"
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

/// A temporary SquashFS upper layer is writable but does not save changes.
fn root_note() -> String {
    if aftermath_ctl::overlay::status()["persistent"] == true {
        format!("{PURPLE}Changes to the system are saved (host overlay disk).{RESET}\n")
    } else {
        format!("{RED}Temporary writable root: changes are lost at poweroff.{RESET}\n")
    }
}
