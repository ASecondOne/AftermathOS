//! SDDM's session command. No shell interpolation: run one fixed Wayland
//! session under a private, lifetime-bound D-Bus session daemon.
use std::os::unix::process::CommandExt;
use std::process::{Command, ExitCode};
#[path = "desktop/audio.rs"]
mod audio;

fn main() -> ExitCode {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args == ["--inside-session"] {
        return run_session();
    }
    if args != ["/usr/bin/startplasma-wayland"] {
        eprintln!("Aftermath desktop session expects /usr/bin/startplasma-wayland");
        return ExitCode::FAILURE;
    }
    let binary = std::env::current_exe()
        .unwrap_or_else(|_| "/usr/lib/aftermath/aftermath-desktop-session".into());
    let account = match nix::unistd::User::from_uid(nix::unistd::Uid::current()) {
        Ok(Some(account)) => account,
        _ => {
            eprintln!("Aftermath desktop: cannot resolve the authenticated account");
            return ExitCode::FAILURE;
        }
    };
    let mut command = Command::new("/usr/bin/dbus-run-session");
    command
        .env("HOME", &account.dir)
        .env("USER", &account.name)
        .env("LOGNAME", &account.name)
        .current_dir(&account.dir);
    for variable in [
        "XDG_CONFIG_HOME",
        "XDG_STATE_HOME",
        "XDG_CACHE_HOME",
        "XDG_DATA_HOME",
    ] {
        if std::env::var_os(variable).is_some_and(|value| value.is_empty()) {
            command.env_remove(variable);
        }
    }
    let error = command
        .arg("--")
        .arg(binary)
        .arg("--inside-session")
        .env_remove("DBUS_SESSION_BUS_ADDRESS")
        .env_remove("DBUS_SESSION_BUS_PID")
        .env("XDG_CURRENT_DESKTOP", "KDE")
        .env("XDG_SESSION_DESKTOP", "KDE")
        .env("DESKTOP_SESSION", "plasma")
        .env("XDG_SESSION_TYPE", "wayland")
        .exec();
    eprintln!("Aftermath desktop session: {error}");
    ExitCode::FAILURE
}

/// Keep audio under the same authenticated session and D-Bus as Plasma.
fn run_session() -> ExitCode {
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
        let _ = std::fs::write(
            std::path::Path::new(&runtime).join("aftermath-desktop-session"),
            format!("Rust session supervisor running; HOME={:?}; XDG_CONFIG_HOME={:?}; XDG_STATE_HOME={:?}\n", std::env::var_os("HOME"), std::env::var_os("XDG_CONFIG_HOME"), std::env::var_os("XDG_STATE_HOME")),
        );
    }
    eprintln!("Aftermath: starting supervised audio and Plasma");
    let _ = Command::new("/usr/lib/aftermath/aftermath-desktop-defaults")
        .arg("--prepare")
        .status();
    let mut audio = match audio::Audio::start() {
        Ok(audio) => Some(audio),
        Err(error) => {
            eprintln!("Aftermath audio startup: {error}");
            None
        }
    };
    let mut plasma = match Command::new("/usr/bin/startplasma-wayland").spawn() {
        Ok(child) => child,
        Err(error) => {
            eprintln!("Aftermath Plasma startup: {error}");
            return ExitCode::FAILURE;
        }
    };
    loop {
        match plasma.try_wait() {
            Ok(Some(status)) => return ExitCode::from(status.code().unwrap_or(1) as u8),
            Err(error) => {
                eprintln!("Aftermath Plasma wait: {error}");
                return ExitCode::FAILURE;
            }
            Ok(None) => {}
        }
        if let Some(audio) = &mut audio {
            audio.tick();
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}
