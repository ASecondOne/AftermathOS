//! Seed the Aftermath palette/wallpaper once, preserving existing user choices.
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, ExitCode};
use std::time::Duration;

fn main() -> ExitCode {
    let result = if std::env::args().nth(1).as_deref() == Some("--prepare") {
        prepare()
    } else {
        apply()
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Aftermath desktop defaults: {error}");
            ExitCode::FAILURE
        }
    }
}

fn config() -> std::io::Result<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .ok_or_else(|| std::io::Error::other("no user configuration directory"))
}

/// Record freshness before Plasma has created any of its normal user settings.
fn prepare() -> std::io::Result<()> {
    let config = config()?;
    let directory = config.join("aftermath");
    fs::create_dir_all(&directory)?;
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
    let marker = directory.join("style-choice");
    if marker.exists() {
        return Ok(());
    }
    let fresh = !config.join("kdeglobals").exists()
        && !config
            .join("plasma-org.kde.plasma.desktop-appletsrc")
            .exists();
    if fresh {
        fs::copy(
            "/usr/share/color-schemes/Aftermath.colors",
            config.join("kdeglobals"),
        )?;
        if !config.join("plasmarc").exists() {
            fs::write(config.join("plasmarc"), "[Theme]\nname=default\n")?;
        }
    }
    fs::write(marker, if fresh { "fresh" } else { "preserve" })
}

/// Run after the workspace starts, when the real wallpaper D-Bus API is ready.
fn apply() -> std::io::Result<()> {
    let marker = config()?.join("aftermath/style-choice");
    if fs::read_to_string(&marker).unwrap_or_default() != "fresh" {
        return Ok(());
    }
    if !Command::new("/usr/bin/plasma-apply-colorscheme")
        .arg("Aftermath")
        .status()?
        .success()
    {
        return Err(std::io::Error::other(
            "could not select the Aftermath palette",
        ));
    }
    for _ in 0..30 {
        if Command::new("/usr/bin/plasma-apply-wallpaperimage")
            .arg("/usr/share/wallpapers/Aftermath/contents/images/1920x1080.svg")
            .status()?
            .success()
        {
            fs::write(marker, "done")?;
            return Ok(());
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    Err(std::io::Error::other(
        "workspace wallpaper service never became ready",
    ))
}
