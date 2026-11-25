//! SDDM's session command. No shell interpolation: run one fixed Wayland
//! session under a private, lifetime-bound D-Bus session daemon.
use std::os::unix::process::CommandExt;
use std::process::{Command, ExitCode};

fn main() -> ExitCode {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args != ["/usr/bin/startplasma-wayland"] {
        eprintln!("Aftermath desktop session expects /usr/bin/startplasma-wayland");
        return ExitCode::FAILURE;
    }
    let error = Command::new("/usr/bin/dbus-run-session")
        .args(["--", "/usr/bin/startplasma-wayland"])
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
