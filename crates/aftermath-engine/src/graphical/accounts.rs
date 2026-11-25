//! Native account preparation for graphical login. Passwords use private stdin.
use super::{LOGIN1, SDDM};
use aftermath_ctl::graphical::Settings;
use nix::unistd::{chown, Gid, Uid, User};
use std::{
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Command, Stdio},
};
/// Run a native account utility without a shell or inherited search path.
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

/// Prepare the service user and optional, explicitly configured live accounts.
pub(super) fn prepare(config: &Settings) -> Result<(), String> {
    aftermath_ctl::users::require_aftermath()?;
    if !Path::new(LOGIN1).is_file() || !Path::new(SDDM).is_file() {
        return Err("elogind or SDDM is missing; rebuild the desktop image".into());
    }
    prepare_sddm()?;
    if config.development_accounts && !Path::new("/etc/aftermath/desktop-seeded").exists() {
        seed_development_accounts()?;
    }
    Ok(())
}

/// SDDM owns its private configuration directory and no normal user's home.
fn prepare_sddm() -> Result<(), String> {
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
    Ok(())
}

/// Apply the requested temporary credentials once, through private stdin.
fn seed_development_accounts() -> Result<(), String> {
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
    Ok(())
}

/// Populate distribution service accounts while the Rust engine remains PID 1.
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
