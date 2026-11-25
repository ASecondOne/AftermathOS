//! Private guest-only integration probe. The host drives the actual SDDM UI.
use aftermath_ctl::proto::{self, Request};
use std::ffi::{c_char, c_int, c_void, CString};
use std::fs;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::{Duration, Instant};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
#[repr(C)]
struct Message {
    style: c_int,
    text: *const c_char,
}
#[repr(C)]
struct Response {
    text: *mut c_char,
    code: c_int,
}
#[repr(C)]
struct Conversation {
    callback:
        unsafe extern "C" fn(c_int, *mut *const Message, *mut *mut Response, *mut c_void) -> c_int,
    data: *mut c_void,
}
#[link(name = "pam")]
extern "C" {
    fn pam_start(
        service: *const c_char,
        user: *const c_char,
        conversation: *const Conversation,
        handle: *mut *mut c_void,
    ) -> c_int;
    fn pam_authenticate(handle: *mut c_void, flags: c_int) -> c_int;
    fn pam_acct_mgmt(handle: *mut c_void, flags: c_int) -> c_int;
    fn pam_end(handle: *mut c_void, status: c_int) -> c_int;
}
unsafe extern "C" fn conversation(
    n: c_int,
    messages: *mut *const Message,
    result: *mut *mut Response,
    _data: *mut c_void,
) -> c_int {
    if !(1..=16).contains(&n) {
        return 19;
    }
    let responses =
        unsafe { libc::calloc(n as usize, std::mem::size_of::<Response>()).cast::<Response>() };
    if responses.is_null() {
        return 5;
    }
    for i in 0..n as usize {
        let message = unsafe { &**messages.add(i) };
        if message.style == 1 || message.style == 2 {
            unsafe {
                (*responses.add(i)).text = libc::strdup(c"bob".as_ptr());
            }
        }
    }
    unsafe {
        *result = responses;
    }
    0
}
fn require(ok: bool, message: &str) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(std::io::Error::other(message).into())
    }
}
fn credentials(user: &str) -> Result<()> {
    let user = CString::new(user)?;
    let conv = Conversation {
        callback: conversation,
        data: std::ptr::null_mut(),
    };
    let mut handle = std::ptr::null_mut();
    require(
        unsafe { pam_start(c"sddm".as_ptr(), user.as_ptr(), &conv, &mut handle) } == 0,
        "start credential verification failed",
    )?;
    let auth = unsafe { pam_authenticate(handle, 0) };
    let account = if auth == 0 {
        unsafe { pam_acct_mgmt(handle, 0) }
    } else {
        auth
    };
    unsafe {
        pam_end(handle, account);
    }
    require(
        auth == 0 && account == 0,
        "temporary development credential failed PAM authentication",
    )
}
fn processes(prefix: &str) -> Vec<PathBuf> {
    fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            fs::read_link(path.join("exe"))
                .ok()?
                .file_name()?
                .to_str()?
                .starts_with(prefix)
                .then_some(path)
        })
        .collect()
}
fn wait(message: &str, check: impl Fn() -> bool) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(120);
    while !check() {
        require(Instant::now() < deadline, message)?;
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}
fn sessions() -> Result<serde_json::Value> {
    let reply = proto::call(
        aftermath_ctl::SOCKET,
        &Request {
            subsystem: "sessions".into(),
            verb: "list".into(),
            args: vec![],
        },
    )?;
    require(reply.ok, "sessions query failed")?;
    Ok(reply.data)
}
fn bus(
    uid: u32,
    gid: u32,
    address: &str,
    destination: &str,
    path: &str,
    method: &str,
    args: &[&str],
) -> Result<String> {
    let output = Command::new("/usr/bin/dbus-send")
        .arg(format!("--bus={address}"))
        .args(["--print-reply", "--reply-timeout=5000"])
        .arg(format!("--dest={destination}"))
        .args([path, method])
        .args(args)
        .uid(uid)
        .gid(gid)
        .env_clear()
        .env("PATH", "/usr/bin")
        .output()?;
    require(output.status.success(), "graphical user D-Bus call failed")?;
    Ok(String::from_utf8(output.stdout)?)
}
fn probe() -> Result<()> {
    wait("SDDM did not open a graphical greeter", || {
        !processes("sddm-greeter").is_empty()
    })?;
    verify_greeter()?;
    credentials("bob")?;
    credentials("root")?;
    let bob = aftermath_ctl::users::read(Path::new("/"))?
        .into_iter()
        .find(|u| u.name == "bob")
        .ok_or("no Bob account")?;
    require(bob.uid >= 1000 && bob.gid != 0, "Bob is not unprivileged")?;
    wait("SDDM did not open a graphical greeter", || {
        !processes("sddm-greeter").is_empty()
    })?;
    println!("AM_GREETER_READY");
    std::io::stdout().flush()?;
    wait(
        "Plasma shell did not launch after graphical authentication",
        || !processes("plasmashell").is_empty() && !processes("kwin_wayland").is_empty(),
    )?;
    let shell = processes("plasmashell")
        .into_iter()
        .next()
        .ok_or("Plasma disappeared")?;
    let status = fs::read_to_string(shell.join("status"))?;
    let uids = status
        .lines()
        .find_map(|l| l.strip_prefix("Uid:"))
        .ok_or("no Plasma UID")?;
    require(
        uids.split_whitespace()
            .all(|u| u.parse::<u32>().ok() == Some(bob.uid)),
        "Plasma retained privileged UIDs",
    )?;
    let runtime = PathBuf::from(format!("/run/user/{}", bob.uid));
    let meta = fs::metadata(&runtime)?;
    require(
        meta.uid() == bob.uid && meta.mode() & 0o777 == 0o700,
        "graphical runtime ownership wrong",
    )?;
    let env = fs::read(shell.join("environ"))?;
    let env: Vec<_> = env
        .split(|b| *b == 0)
        .filter_map(|s| std::str::from_utf8(s).ok())
        .collect();
    require(
        env.contains(&"XDG_SESSION_TYPE=wayland"),
        "Plasma is not a Wayland session",
    )?;
    let address = env
        .iter()
        .find_map(|s| s.strip_prefix("DBUS_SESSION_BUS_ADDRESS="))
        .ok_or("no graphical D-Bus address")?;
    for service in ["org.kde.plasmashell", "org.kde.KWin"] {
        wait("desktop services did not acquire their D-Bus names", || {
            bus(
                bob.uid,
                bob.gid,
                address,
                "org.freedesktop.DBus",
                "/org/freedesktop/DBus",
                "org.freedesktop.DBus.NameHasOwner",
                &[&format!("string:{service}")],
            )
            .is_ok_and(|s| s.contains("boolean true"))
        })?;
    }
    require(
        sessions()?["sessions"]
            .as_array()
            .ok_or("invalid sessions")?
            .iter()
            .any(|s| s["user"] == "bob" && s["type"] == "wayland" && s["seat_control"] == true),
        "graphical PAM session was not registered with elogind",
    )?;
    verify_keyboard(bob.uid, bob.gid, address)?;
    verify_audio(bob.uid, bob.gid, address, &runtime)?;
    verify_style()?;
    println!("AM_PLASMA_READY");
    std::io::stdout().flush()?;
    let mut command = String::new();
    std::io::stdin().read_line(&mut command)?;
    require(
        command.trim() == "logout",
        "desktop did not render visible content",
    )?;
    bus(
        bob.uid,
        bob.gid,
        address,
        "org.kde.Shutdown",
        "/Shutdown",
        "org.kde.Shutdown.logout",
        &[],
    )?;
    wait("logout did not clean the graphical session", || {
        processes("plasmashell").is_empty()
            && !runtime.exists()
            && !processes("sddm-greeter").is_empty()
    })?;
    require(
        sessions()?["sessions"]
            .as_array()
            .ok_or("invalid sessions")?
            .iter()
            .all(|s| s["user"] != "bob"),
        "Bob's session remains after logout",
    )?;
    println!("AM_GRAPHICAL_LOGOUT_OK");
    Ok(())
}

/// The greeter must use Wayland on tty1, with no unlocked shell on that VT.
fn verify_greeter() -> Result<()> {
    let greeter = processes("sddm-greeter")
        .into_iter()
        .next()
        .ok_or("no greeter")?;
    let environment = fs::read(greeter.join("environ"))?;
    let environment = String::from_utf8_lossy(&environment);
    require(
        environment.contains("XDG_SESSION_TYPE=wayland"),
        "SDDM greeter is not Wayland",
    )?;
    require(
        environment.contains("XDG_VTNR=1"),
        "SDDM greeter did not use tty1",
    )?;
    require(
        fs::read_to_string("/sys/class/tty/tty0/active")?.trim() == "tty1",
        "greeter is on the wrong active VT",
    )?;
    for process in processes("script").into_iter().chain(processes("bash")) {
        let stat = fs::read_to_string(process.join("stat"))?;
        let fields = stat.rsplit_once(") ").ok_or("invalid process stat")?.1;
        require(
            fields.split_whitespace().nth(4) != Some("1025"),
            "root shell occupies tty1 during desktop boot",
        )?;
    }
    require(
        processes("Xorg").is_empty(),
        "greeter started an Xorg server",
    )?;
    require(
        Command::new("/usr/bin/amctl")
            .args(["logs", "desktop"])
            .output()?
            .status
            .success(),
        "desktop logs are unavailable",
    )?;
    println!("AM_WAYLAND_GREETER_TTY1_OK");
    Ok(())
}

/// Query KWin's actual keymap rather than just checking a generated file.
fn verify_keyboard(uid: u32, gid: u32, address: &str) -> Result<()> {
    let layouts = bus(
        uid,
        gid,
        address,
        "org.kde.keyboard",
        "/Layouts",
        "org.kde.KeyboardLayouts.getLayoutsList",
        &[],
    )?;
    require(
        layouts.contains("German") && layouts.contains("no dead keys"),
        "KWin did not load German nodeadkeys",
    )?;
    println!("AM_WAYLAND_KEYBOARD_OK");
    Ok(())
}

/// Real user-owned services must expose an ALSA-backed PipeWire output sink.
fn verify_audio(uid: u32, gid: u32, address: &str, runtime: &Path) -> Result<()> {
    require(
        runtime.join("aftermath-desktop-session").exists(),
        "SDDM bypassed the Rust session command",
    )?;
    for program in ["pipewire", "wireplumber", "pipewire-pulse"] {
        wait(
            &format!("session audio daemon {program} is missing"),
            || {
                let executable = if program == "pipewire-pulse" {
                    "pipewire"
                } else {
                    program
                };
                processes(executable).iter().any(|path| {
                    fs::metadata(path).is_ok_and(|meta| meta.uid() == uid)
                        && fs::read(path.join("cmdline")).is_ok_and(|args| {
                            args.split(|byte| *byte == 0)
                                .next()
                                .and_then(|arg| std::str::from_utf8(arg).ok())
                                .and_then(|arg| Path::new(arg).file_name())
                                .is_some_and(|name| name == program)
                        })
                })
            },
        )?;
    }
    wait("WirePlumber did not expose the virtual audio card", || {
        Command::new("/usr/bin/pw-dump")
            .uid(uid)
            .gid(gid)
            .env("XDG_RUNTIME_DIR", runtime)
            .env("DBUS_SESSION_BUS_ADDRESS", address)
            .output()
            .is_ok_and(|output| {
                output.status.success()
                    && String::from_utf8_lossy(&output.stdout).contains("alsa_output")
            })
    })?;
    require(
        runtime.join("pulse/native").exists(),
        "PulseAudio compatibility socket is missing",
    )?;
    println!("AM_PIPEWIRE_AUDIO_OK");
    Ok(())
}

/// Fresh users receive the requested palette and wallpaper once.
fn verify_style() -> Result<()> {
    wait("Aftermath desktop defaults were not applied", || {
        fs::read_to_string("/home/bob/.config/aftermath/style-choice")
            .is_ok_and(|value| value == "done")
    })?;
    require(
        fs::read_to_string("/home/bob/.config/kdeglobals")
            .unwrap_or_default()
            .contains("ColorScheme=Aftermath"),
        "Aftermath color scheme missing",
    )?;
    wait("Aftermath wallpaper missing", || {
        fs::read_to_string("/home/bob/.config/plasma-org.kde.plasma.desktop-appletsrc")
            .is_ok_and(|text| text.contains("wallpapers/Aftermath"))
    })?;
    println!("AM_DESKTOP_STYLE_OK");
    Ok(())
}
fn main() -> ExitCode {
    if unsafe { libc::geteuid() } != 0
        || !fs::read_to_string("/proc/cmdline").is_ok_and(|s| {
            s.split_whitespace()
                .any(|p| p == "aftermath.test=desktop-smoke")
        })
    {
        eprintln!("This probe runs only in the private graphical smoke guest.");
        return ExitCode::FAILURE;
    }
    let result = probe();
    if let Err(e) = &result {
        eprintln!("AM_DESKTOP_SMOKE_FAILED: {e}");
        for log in [
            "/run/aftermath/elogind.log",
            "/run/aftermath/sddm.log",
            "/home/bob/.local/share/sddm/wayland-session.log",
            "/home/bob/.local/state/aftermath/audio/pipewire.log",
            "/home/bob/.local/state/aftermath/audio/wireplumber.log",
            "/home/bob/.local/state/aftermath/audio/pipewire-pulse.log",
            "/home/bob/.config/aftermath/style-choice",
            "/run/user/1000/aftermath-desktop-session",
            "/proc/asound/cards",
        ] {
            if let Ok(text) = fs::read_to_string(log) {
                eprintln!(
                    "{log}:\n{}",
                    text.lines()
                        .rev()
                        .take(80)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .collect::<Vec<_>>()
                        .join("\n")
                );
            }
        }
    } else {
        println!("AM_DESKTOP_SMOKE_OK");
    }
    std::io::stdout().flush().ok();
    unsafe {
        libc::kill(1, libc::SIGUSR2);
    }
    if result.is_ok() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
