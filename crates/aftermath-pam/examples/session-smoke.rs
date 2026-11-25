//! Guest-only Rust integration test. Never installed in normal images.
//! It drives the real login/PAM stack through private pseudoterminals.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use aftermath_ctl::proto::{self, Request};
use serde_json::Value;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
const USER: &str = "am-session-test";
const PASSWORD: &str = "private-vm-test-password";
const OWN_PASSWORD: &str = "RustGuest-ChangedPass!4206";
const PROBE: &str = "/usr/lib/aftermath/session-smoke";
const STRATUM_ETC: &str = "/run/aftermath/account-test-stratum/etc";

fn require(ok: bool, message: &str) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(std::io::Error::other(message).into())
    }
}
fn sessions() -> Result<Value> {
    let reply = proto::call(
        aftermath_ctl::SOCKET,
        &Request {
            subsystem: "sessions".into(),
            verb: "list".into(),
            args: vec![],
        },
    )?;
    require(reply.ok, "engine could not list sessions")?;
    Ok(reply.data)
}
fn count() -> Result<usize> {
    Ok(sessions()?["sessions"]
        .as_array()
        .ok_or("invalid session list")?
        .len())
}

struct Login {
    child: Child,
    terminal: File,
    received: Vec<u8>,
}
impl Login {
    fn start() -> Result<Self> {
        Self::program("/usr/bin/login", &[])
    }
    fn program(program: &str, args: &[&str]) -> Result<Self> {
        let (mut master, mut slave) = (-1, -1);
        // SAFETY: valid output pointers, default termios and window size.
        require(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    std::ptr::null(),
                )
            } == 0,
            "openpty failed",
        )?;
        let terminal = unsafe { File::from_raw_fd(master) };
        let input = unsafe { File::from_raw_fd(slave) };
        let output = input.try_clone()?;
        let error = input.try_clone()?;
        unsafe {
            let flags = libc::fcntl(master, libc::F_GETFL);
            require(
                flags >= 0 && libc::fcntl(master, libc::F_SETFL, flags | libc::O_NONBLOCK) == 0,
                "cannot make test PTY nonblocking",
            )?;
        }
        let mut cmd = Command::new(program);
        cmd.args(args)
            .env_clear()
            .env("TERM", "linux")
            .env("LANG", "C")
            .stdin(Stdio::from(input))
            .stdout(Stdio::from(output))
            .stderr(Stdio::from(error));
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        Ok(Self {
            child: cmd.spawn()?,
            terminal,
            received: Vec::new(),
        })
    }
    fn wait_for(&mut self, pattern: &str) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut buf = [0u8; 4096];
        loop {
            if let Some(at) = self
                .received
                .windows(pattern.len())
                .position(|w| w == pattern.as_bytes())
            {
                self.received.drain(..at + pattern.len());
                return Ok(());
            }
            match self.terminal.read(&mut buf) {
                Ok(0) => return Err(format!("login closed before {pattern}").into()),
                Ok(n) => {
                    self.received.extend_from_slice(&buf[..n]);
                    require(
                        [PASSWORD, OWN_PASSWORD].iter().all(|secret| {
                            !self
                                .received
                                .windows(secret.len())
                                .any(|w| w == secret.as_bytes())
                        }),
                        "password echoed into terminal output",
                    )?;
                    require(
                        self.received.len() < 128 * 1024,
                        "unexpectedly large login output",
                    )?;
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    require(
                        self.child.try_wait()?.is_none(),
                        &format!("terminal command exited before {pattern}"),
                    )?;
                }
                Err(e) => {
                    let output = String::from_utf8_lossy(&self.received)
                        .replace(PASSWORD, "[redacted]")
                        .replace(OWN_PASSWORD, "[redacted]");
                    return Err(format!("login terminal before {pattern}: {e}; {output}").into());
                }
            }
            require(
                Instant::now() < deadline,
                &format!("timed out waiting for {pattern}"),
            )?;
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    fn line(&mut self, text: &str) -> Result<()> {
        self.terminal.write_all(text.as_bytes())?;
        self.terminal.write_all(b"\n")?;
        Ok(())
    }
    fn authenticate(&mut self, password: &str) -> Result<()> {
        self.wait_for("login:")?;
        self.line(USER)?;
        self.wait_for("Password:")?;
        self.line(password)
    }
    fn logout(&mut self) -> Result<()> {
        self.line("logout")?;
        self.finish()
    }
    fn finish(&mut self) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = self.child.try_wait()? {
                require(status.success(), "terminal command failed")?;
                break;
            }
            require(
                Instant::now() < deadline,
                "login monitor did not close its PAM session",
            )?;
            std::thread::sleep(Duration::from_millis(20));
        }
        Ok(())
    }
}
impl Drop for Login {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(Some(_))) {
            return;
        }
        // All signals target this test's own terminal/session processes.
        let foreground = unsafe { libc::tcgetpgrp(self.terminal.as_raw_fd()) };
        if foreground > 1 {
            unsafe {
                libc::kill(-foreground, libc::SIGKILL);
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn user_probe() -> Result<()> {
    require(
        unsafe { libc::getuid() == 1000 && libc::geteuid() == 1000 },
        "login did not drop uid",
    )?;
    require(
        unsafe { libc::getgid() != 0 && libc::getegid() != 0 },
        "login retained the root group",
    )?;
    let mut groups = [0; 64];
    let n = unsafe { libc::getgroups(groups.len() as i32, groups.as_mut_ptr()) };
    require(
        n >= 0 && !groups[..n.max(0) as usize].contains(&0),
        "login retained a root supplementary group",
    )?;
    require(
        std::env::var("USER")? == USER && std::env::var("HOME")? == format!("/home/{USER}"),
        "invalid account environment",
    )?;
    let runtime = std::env::var("XDG_RUNTIME_DIR")?;
    require(
        runtime == "/run/user/1000",
        "missing managed runtime directory",
    )?;
    require(
        std::env::var("XDG_SESSION_TYPE")? == "tty"
            && std::env::var("XDG_SESSION_CLASS")? == "user",
        "invalid session type",
    )?;
    require(
        std::env::var("XDG_SESSION_ID")?.starts_with('c'),
        "missing session ID",
    )?;
    let m = fs::symlink_metadata(&runtime)?;
    require(
        m.is_dir() && m.uid() == 1000 && m.mode() & 0o777 == 0o700,
        "invalid runtime ownership or mode",
    )?;
    let denied = proto::call(
        aftermath_ctl::SOCKET,
        &Request {
            subsystem: "sessions".into(),
            verb: "open".into(),
            args: vec![USER.into(), std::process::id().to_string(), "tty2".into()],
        },
    )?;
    require(
        !denied.ok
            && denied
                .error
                .unwrap_or_default()
                .contains("permission denied"),
        "unprivileged registration was allowed",
    )?;
    for verb in ["add", "lock", "unlock", "sync"] {
        let denied = proto::call(
            aftermath_ctl::SOCKET,
            &Request {
                subsystem: "users".into(),
                verb: verb.into(),
                args: vec![USER.into()],
            },
        )?;
        require(
            !denied.ok
                && denied
                    .error
                    .unwrap_or_default()
                    .contains("permission denied"),
            "unprivileged account mutation allowed",
        )?;
    }
    require(
        amctl(&["users", "list", "--json"], true)?["users"].is_array(),
        "ordinary user cannot list users",
    )?;
    let changed = Path::new(&std::env::var("HOME")?).join(".password-smoke-done");
    if !changed.exists() {
        let mut prompt = Login::program("/usr/bin/amctl", &["users", "passwd"])?;
        prompt.wait_for("Current password:")?;
        prompt.line(PASSWORD)?;
        prompt.wait_for("New password:")?;
        prompt.line(OWN_PASSWORD)?;
        prompt.wait_for("Retype new password:")?;
        prompt.line(OWN_PASSWORD)?;
        prompt.wait_for("password updated successfully")?;
        prompt.finish()?;
        fs::write(changed, b"done")?;
    }
    fs::write(Path::new(&runtime).join("user-file"), b"session state")?;
    let link = Path::new(&runtime).join("must-not-follow");
    if fs::symlink_metadata(&link).is_err() {
        symlink("/run/aftermath/keep-after-logout", link)?;
    }
    println!("AM_USER_READY");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    require(line.trim() == "logout", "unexpected test input")
}

fn amctl(args: &[&str], success: bool) -> Result<Value> {
    let output = Command::new("/usr/bin/amctl").args(args).output()?;
    require(
        output.status.success() == success,
        "unexpected amctl account command result",
    )?;
    let text = String::from_utf8(output.stdout)?;
    require(
        !text.contains(PASSWORD)
            && !text.contains(OWN_PASSWORD)
            && !text.contains("$y$")
            && !text.contains("$6$"),
        "secret in account output",
    )?;
    Ok(serde_json::from_str(&text)?)
}

fn set_password(args: &[&str]) -> Result<()> {
    let mut prompt = Login::program("/usr/bin/amctl", args)?;
    prompt.wait_for("New password:")?;
    prompt.line(PASSWORD)?;
    prompt.wait_for("Retype new password:")?;
    prompt.line(PASSWORD)?;
    prompt.wait_for("password updated successfully")?;
    prompt.finish()
}

fn prepare_account_namespace() -> Result<()> {
    let etc = Path::new(STRATUM_ETC);
    fs::create_dir_all(etc)?;
    // A private account namespace tests merging without installing a distro or
    // attaching a saved disk. UID/GID 1001 must be reserved for its own user.
    for (db, data, mode) in [
        ("passwd", "root:x:0:0::/root:/bin/bash\ndaemon:x:1:1::/:/bin/false\nforeign:x:1001:1001::/home/foreign:/bin/bash\n", 0o644),
        ("group", "root:x:0:\ndaemon:x:1:\nforeign:x:1001:\n", 0o644),
        ("shadow", "root:!:::::::\ndaemon:*:::::::\nforeign:!:::::::\n", 0o640),
        ("gshadow", "root:!::\ndaemon:!::\nforeign:!::\n", 0o600),
    ] {
        fs::write(etc.join(db), data)?;
        fs::set_permissions(etc.join(db), fs::Permissions::from_mode(mode))?;
    }
    writeln!(OpenOptions::new().append(true).open("/etc/aftermath/strata.toml")?,
        "\n[[stratum]]\nname = \"am-account-test\"\npath = \"/run/aftermath/account-test-stratum\"\nkind = \"test\"\nenabled = false\n")?;
    fs::write("/run/aftermath/strata", "am-account-test up\n")?;
    Ok(())
}

fn shared_password_matches() -> Result<bool> {
    let host = fs::read_to_string("/etc/shadow")?;
    let guest = fs::read_to_string(Path::new(STRATUM_ETC).join("shadow"))?;
    let prefix = format!("{USER}:");
    Ok(host.lines().find(|l| l.starts_with(&prefix))
        == guest.lines().find(|l| l.starts_with(&prefix)))
}

/// Restore signed package modes stripped by unprivileged fixture extraction.
/// Files are root-owned here; production pacstrap already preserves these bits.
fn restore_package_helpers() -> Result<()> {
    for (path, mode) in [
        ("/usr/bin/passwd", 0o4755),
        ("/usr/bin/unix_chkpwd", 0o6755),
    ] {
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    }
    Ok(())
}

fn root_probe() -> Result<()> {
    restore_package_helpers()?;
    require(
        !fs::read_to_string("/etc/passwd")?
            .lines()
            .any(|l| l.starts_with(&format!("{USER}:")) || l.split(':').nth(2) == Some("1000")),
        "session smoke requires a fresh test rootfs without uid 1000",
    )?;
    prepare_account_namespace()?;
    let u = amctl(&["users", "add", USER, "--locked", "--json"], true)?;
    require(
        u["user"]["uid"] == 1000
            && u["user"]["gid"] == 1000
            && u["user"]["password_status"] == "locked"
            && u["user"]["shell"] == "/usr/bin/bash",
        "new user identity/defaults wrong",
    )?;
    let home = fs::metadata(format!("/home/{USER}"))?;
    require(
        home.uid() == 1000 && home.mode() & 0o777 == 0o700,
        "new home not private",
    )?;
    let shadow = fs::read("/etc/shadow")?;
    amctl(&["users", "unlock", USER, "--json"], false)?;
    amctl(&["users", "add", USER, "--locked", "--json"], false)?;
    amctl(&["users", "add", "../invalid", "--locked", "--json"], false)?;
    amctl(&["users", "add", "no-terminal", "--json"], false)?;
    require(
        fs::read("/etc/shadow")? == shadow && !Path::new("/home/no-terminal").exists(),
        "rejected operation changed accounts",
    )?;
    set_password(&["users", "passwd", USER])?;
    set_password(&["users", "add", "am-interactive-test"])?;
    require(
        amctl(&["users", "show", "am-interactive-test", "--json"], true)?["user"]["uid"] == 1002,
        "creation reused a stratum UID/GID",
    )?;
    // Exercise the real lckpwdf-compatible POSIX lock across processes.
    let pam_lock = OpenOptions::new()
        .read(true)
        .write(true)
        .open(Path::new(STRATUM_ETC).join(".pwd.lock"))?;
    let mut lock: libc::flock = unsafe { std::mem::zeroed() };
    lock.l_type = libc::F_WRLCK as _;
    lock.l_whence = libc::SEEK_SET as _;
    require(
        unsafe { libc::fcntl(pam_lock.as_raw_fd(), libc::F_SETLK, &lock) } == 0,
        "cannot hold simulated PAM lock",
    )?;
    let result = amctl(&["users", "lock", USER, "--json"], true)?;
    require(
        result["strata_synced"] == false && !shared_password_matches()?,
        "busy stratum was silently overwritten",
    )?;
    amctl(&["users", "sync", "--json"], false)?;
    drop(pam_lock);
    amctl(&["users", "sync", "--json"], true)?;
    require(
        shared_password_matches()?,
        "retry did not copy password lock",
    )?;
    amctl(&["users", "lock", USER, "--json"], true)?;
    {
        let mut locked = Login::start()?;
        locked.authenticate(PASSWORD)?;
        locked.wait_for("Login incorrect")?;
        require(count()? == 0, "password-locked account logged in")?;
    }
    amctl(&["users", "unlock", USER, "--json"], true)?;
    require(
        amctl(&["users", "show", USER, "--json"], true)?["user"]["password_status"] == "set",
        "password unlock failed",
    )?;
    require(
        Command::new("/usr/bin/usermod")
            .args(["--shell", PROBE, "--", USER])
            .status()?
            .success(),
        "set test probe shell failed",
    )?;
    writeln!(
        OpenOptions::new().append(true).open("/etc/shells")?,
        "{PROBE}"
    )?;
    println!("AM_ACCOUNT_MANAGEMENT_OK");
    fs::write("/run/aftermath/keep-after-logout", b"root sentinel")?;
    require(
        count()? == 0,
        "unexpected pre-existing authenticated session",
    )?;
    let mut first = Login::start()?;
    first.authenticate("deliberately-wrong")?;
    first.wait_for("Login incorrect")?;
    require(
        count()? == 0 && !Path::new("/run/user/1000").exists(),
        "failed authentication created session state",
    )?;
    println!("AM_PASSWORD_REJECTED");
    first.authenticate(PASSWORD)?;
    first.wait_for("AM_USER_READY")?;
    let deadline = Instant::now() + Duration::from_secs(10);
    while !shared_password_matches()? {
        require(
            Instant::now() < deadline,
            "own password change did not sync automatically",
        )?;
        std::thread::sleep(Duration::from_millis(25));
    }
    let guest = fs::read_to_string(Path::new(STRATUM_ETC).join("passwd"))?;
    require(
        guest.contains("daemon:x:1:1:") && guest.contains("foreign:x:1001:1001:"),
        "sharing removed stratum accounts",
    )?;
    require(
        fs::metadata(Path::new(STRATUM_ETC).join("shadow"))?.mode() & 0o777 == 0o640,
        "shadow permissions changed",
    )?;
    println!("AM_ACCOUNT_SHARING_OK");
    require(
        count()? == 1,
        "first authenticated login was not registered",
    )?;
    let inode = fs::metadata("/run/user/1000")?.ino();
    let mut second = Login::start()?;
    second.authenticate(OWN_PASSWORD)?;
    second.wait_for("AM_USER_READY")?;
    require(
        count()? == 2 && fs::metadata("/run/user/1000")?.ino() == inode,
        "concurrent logins did not share the runtime directory",
    )?;
    println!("AM_CONCURRENT_LOGINS_OK");
    first.logout()?;
    require(
        count()? == 1 && Path::new("/run/user/1000/user-file").exists(),
        "first logout removed another session's runtime",
    )?;
    second.logout()?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let v = sessions()?;
        if count()? == 0 && !Path::new("/run/user/1000").exists() && v["cleanup_pending"] == 0 {
            break;
        }
        require(
            Instant::now() < deadline,
            "last logout did not clean up runtime state",
        )?;
        std::thread::sleep(Duration::from_millis(25));
    }
    require(
        fs::read("/run/aftermath/keep-after-logout")? == b"root sentinel",
        "runtime cleanup followed a symlink",
    )?;
    let tty2 = fs::read_dir("/proc")?.flatten().any(|e| {
        fs::read(e.path().join("cmdline")).is_ok_and(|cmd| cmd.windows(6).any(|w| w == b"agetty"))
            && fs::read_link(e.path().join("fd/0")).is_ok_and(|p| p == Path::new("/dev/tty2"))
    });
    require(tty2, "no authenticated login terminal on tty2")?;
    println!("AM_LOGOUT_CLEANUP_OK");
    Ok(())
}

fn main() -> ExitCode {
    let permitted = fs::read_to_string("/proc/cmdline").is_ok_and(|s| {
        s.split_whitespace()
            .any(|a| a == "aftermath.test=session-smoke")
    });
    if !permitted {
        eprintln!("This helper only runs inside the private session-smoke guest.");
        return ExitCode::FAILURE;
    }
    let root = unsafe { libc::geteuid() } == 0;
    let result = if root { root_probe() } else { user_probe() };
    let code = match result {
        Ok(()) => {
            if root {
                println!("AM_SESSION_SMOKE_OK");
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("AM_SESSION_SMOKE_FAILED: {e}");
            ExitCode::FAILURE
        }
    };
    if root {
        std::io::stdout().flush().ok();
        unsafe {
            libc::kill(1, libc::SIGUSR2);
        }
    }
    code
}
