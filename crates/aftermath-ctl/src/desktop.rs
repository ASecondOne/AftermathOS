//! Read-only prerequisites for a host Plasma Wayland session. File presence
//! is installation evidence, never a claim that a daemon or desktop works.
//! Keep the implementation blockers in sync as engine support is added.

use std::collections::VecDeque;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};

use serde_json::{json, Value};

pub const UDEV_DAEMONS: &[&str] = &[
    "/usr/lib/systemd/systemd-udevd",
    "/usr/bin/udevd",
    "/usr/sbin/udevd",
    "/sbin/udevd",
    "/usr/lib/udev/udevd",
];
pub const UDEVADM_PATHS: &[&str] = &["/usr/bin/udevadm", "/usr/sbin/udevadm", "/sbin/udevadm"];
pub const DBUS_DAEMON: &str = "/usr/bin/dbus-daemon";
pub const DBUS_SEND: &str = "/usr/bin/dbus-send";

/// Resolve paths as the inspected root would, including absolute symlinks.
/// Otherwise an image's /bin -> /usr/bin could accidentally probe the host.
fn inside(root: &Path, path: &Path) -> io::Result<PathBuf> {
    let mut pending: VecDeque<_> = path
        .components()
        .map(|c| c.as_os_str().to_owned())
        .collect();
    let mut relative = PathBuf::new();
    let mut links = 0;
    while let Some(part) = pending.pop_front() {
        match Path::new(&part).components().next() {
            Some(Component::RootDir) => relative.clear(),
            Some(Component::CurDir) => {}
            Some(Component::ParentDir) => {
                relative.pop();
            }
            Some(Component::Normal(_)) => {
                let candidate = root.join(&relative).join(&part);
                if fs::symlink_metadata(&candidate)?.file_type().is_symlink() {
                    links += 1;
                    if links > 40 {
                        return Err(io::Error::from_raw_os_error(libc::ELOOP));
                    }
                    let target = fs::read_link(candidate)?;
                    for c in target.components().rev() {
                        pending.push_front(c.as_os_str().to_owned());
                    }
                } else {
                    relative.push(part);
                }
            }
            _ => {}
        }
    }
    Ok(root.join(relative))
}

fn executable_check(root: &Path, id: &str, paths: &[&str], action: &str) -> Value {
    let mut found = Vec::new();
    let mut errors = Vec::new();
    for path in paths {
        match inside(root, Path::new(path)).and_then(fs::metadata) {
            Ok(m) if m.is_file() && m.permissions().mode() & 0o111 != 0 => found.push(*path),
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => errors.push(format!("{path}: {e}")),
        }
    }
    let state = if !found.is_empty() {
        "present"
    } else if errors.is_empty() {
        "missing"
    } else {
        "unknown"
    };
    json!({"id": id, "state": state, "found": found, "searched": paths,
        "errors": errors, "action": action})
}

/// Inspect files only. Does not execute image programs, contact its D-Bus,
/// install packages, create accounts, or change service/boot configuration.
pub fn check(root: &Path) -> Result<Value, String> {
    let root = fs::canonicalize(root).map_err(|e| format!("{}: {e}", root.display()))?;
    if !root.is_dir() {
        return Err(format!("{} is not a root directory", root.display()));
    }
    let checks = vec![
        executable_check(&root, "device-manager", UDEV_DAEMONS,
            "Provide a udev-compatible daemon and rules; libudev alone is insufficient."),
        executable_check(&root, "device-management-tool", UDEVADM_PATHS,
            "Provide udevadm for daemon readiness, coldplug and bounded queue settling."),
        executable_check(&root, "system-bus", &[DBUS_DAEMON],
            "Provide dbus-daemon, its packaged system bus configuration and service account; broker launch integration is not implemented."),
        executable_check(&root, "system-bus-probe", &[DBUS_SEND],
            "Provide dbus-send for real system-bus readiness and health checks."),
        executable_check(&root, "aftermath-session-module", &[crate::sessions::PAM_MODULE],
            "Build and stage the Rust PAM module with `cargo am run` for authenticated console sessions."),
        executable_check(&root, "session-bus", &["/usr/bin/dbus-run-session"],
            "Provide a per-user D-Bus session launcher (dbus-run-session for the first manual test)."),
        executable_check(&root, "plasma-session", &["/usr/bin/startplasma-wayland"],
            "Provide Plasma's Wayland session in the host rootfs."),
        executable_check(&root, "wayland-compositor", &["/usr/bin/kwin_wayland"],
            "Provide KWin, its libraries and a working Mesa/GPU driver stack."),
        executable_check(&root, "login1-provider", &["/usr/lib/elogind/elogind"], "Build standalone elogind with `cargo am plasma`."),
        executable_check(&root, "graphical-login", &["/usr/bin/sddm"], "Install SDDM and its greeter dependencies."),
        executable_check(&root, "desktop-session-command", &["/usr/lib/aftermath/aftermath-desktop-session"], "Build and stage the Rust SDDM session command."),
    ];
    // These describe actual gaps in this engine, independently of installed
    // packages. In particular, finding elogind or systemd on the build host
    // must not turn the result into a claim of Aftermath compatibility.
    let blockers = json!([
        {"id": "login-and-seats", "detail": "Standalone elogind supplies login1, seat tracking and revocable devices; Rust PAM registers its sessions without taking ownership of elogind runtime directories. Test rendering, input and VT switching on the target hardware."},
        {"id": "service-lifecycle", "detail": "Services use process groups; per-service cgroups, readiness notification and socket/D-Bus activation are not implemented."},
        {"id": "user-session-manager", "detail": "The default Plasma session uses upstream classic startup with a session-scoped D-Bus. There is no systemd unit interpreter or per-user systemd1 manager; applications requiring those APIs need additional support."}
    ]);
    let missing = checks.iter().filter(|c| c["state"] != "present").count();
    Ok(json!({
        "desktop": "plasma-wayland",
        "root": root,
        "ready": false,
        "scope": "File inventory and this Aftermath build's implementation gaps; runtime behavior is not tested.",
        "checks": checks,
        "missing_or_unknown": missing,
        "blockers": blockers,
        "next_step": "Boot the updated image and inspect `amctl desktop status`; sign in through SDDM. File inventory alone cannot verify a running graphical login or desktop.",
        "runtime_validation": "Later verify DRM/Mesa acceleration, input/VT switching, D-Bus readiness, session permissions, login/logout and shutdown in a booted guest. Packages alone do not prove readiness."
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Root(PathBuf);
    impl Root {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let dir = std::env::temp_dir().join(format!(
                "am-desktop-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&dir).unwrap();
            Self(dir)
        }
        fn executable(&self, path: &str) {
            let p = self.0.join(path.trim_start_matches('/'));
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, "not executed by the checker").unwrap();
            fs::set_permissions(p, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
    impl Drop for Root {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn state<'a>(report: &'a Value, id: &str) -> &'a str {
        report["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["id"] == id)
            .unwrap()["state"]
            .as_str()
            .unwrap()
    }

    #[test]
    fn empty_image_has_missing_packages_and_engine_blockers() {
        let root = Root::new();
        let v = check(&root.0).unwrap();
        assert_eq!(v["ready"], false);
        assert_eq!(v["missing_or_unknown"], 11);
        assert!(v["blockers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["id"] == "login-and-seats"));
        assert_eq!(state(&v, "system-bus"), "missing");
    }

    #[test]
    fn installed_programs_never_imply_engine_or_runtime_support() {
        let root = Root::new();
        for p in [
            "/usr/bin/udevd",
            "/usr/bin/udevadm",
            "/usr/bin/dbus-daemon",
            "/usr/bin/dbus-send",
            "/usr/lib/security/pam_aftermath.so",
            "/usr/bin/dbus-run-session",
            "/usr/bin/startplasma-wayland",
            "/usr/bin/kwin_wayland",
            "/usr/lib/elogind/elogind",
            "/usr/bin/sddm",
            "/usr/lib/aftermath/aftermath-desktop-session",
        ] {
            root.executable(p);
        }
        let v = check(&root.0).unwrap();
        assert_eq!(v["missing_or_unknown"], 0);
        assert_eq!(v["ready"], false);
        assert!(!v["blockers"].as_array().unwrap().is_empty());
        // A present file without executable permissions is insufficient.
        fs::set_permissions(
            root.0.join("usr/bin/kwin_wayland"),
            fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        assert_eq!(
            state(&check(&root.0).unwrap(), "wayland-compositor"),
            "missing"
        );
    }

    #[test]
    fn symlinks_stay_in_the_image_and_loops_are_reported() {
        let root = Root::new();
        root.executable("/usr/bin/udevd");
        symlink("/usr/bin", root.0.join("sbin")).unwrap();
        assert_eq!(
            inside(&root.0, Path::new("/sbin/udevd")).unwrap(),
            root.0.join("usr/bin/udevd")
        );
        // This exists on the development host, but not in this image.
        symlink("/proc", root.0.join("host-proc")).unwrap();
        assert!(inside(&root.0, Path::new("/host-proc/self/status")).is_err());
        symlink("loop", root.0.join("loop")).unwrap();
        assert_eq!(
            inside(&root.0, Path::new("/loop"))
                .unwrap_err()
                .raw_os_error(),
            Some(libc::ELOOP)
        );
        symlink("/usr/bin/dbus-daemon", root.0.join("usr/bin/dbus-daemon")).unwrap();
        assert_eq!(state(&check(&root.0).unwrap(), "system-bus"), "unknown");
    }

    #[test]
    fn rejects_invalid_roots() {
        let root = Root::new();
        assert!(check(&root.0.join("missing")).is_err());
        fs::write(root.0.join("file"), "").unwrap();
        assert!(check(&root.0.join("file")).is_err());
    }
}
