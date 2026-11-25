//! The persistent host root (see aftermath-early): status, and
//! `amctl overlay reset` = wipe the saved changes on the next boot.
//!
//! Reset writes a marker file `reset-requested` on the overlay disk;
//! aftermath-early sees it before building the overlay, empties
//! upper/ and work/ and removes the marker. Nothing is deleted while the
//! system is running (the overlay is in use).

use std::fs;
use std::path::{Path, PathBuf};

use nix::mount::{mount, umount, MsFlags};
use serde_json::{json, Value};

use crate::disks;

/// Where aftermath-early leaves the overlay disk mounted.
pub const HOSTDISK: &str = "/aftermath/hostdisk";
pub const RESET_MARKER: &str = "reset-requested";
const TMP_MNT: &str = "/run/aftermath/hostdisk-tmp";

pub fn status() -> Value {
    let root = disks::mount_of(Path::new("/"));
    let disk = disks::mount_of(Path::new(HOSTDISK)).map(|(s, _)| s);
    let persistent = root.as_ref().is_some_and(|(_, t)| t == "overlay") && disk.is_some();
    let pending = Path::new(HOSTDISK).join(RESET_MARKER).exists();
    let usage = disk.as_ref().and_then(|_| usage(Path::new(HOSTDISK)));
    json!({
        "persistent": persistent,
        "device": disk,
        "reset_pending": pending,
        "used_bytes": usage.map(|u| u.0),
        "size_bytes": usage.map(|u| u.1),
        "warning": fs::read_to_string("/aftermath/image/boot-warning").ok(),
    })
}

fn usage(p: &Path) -> Option<(u64, u64)> {
    let st = nix::sys::statvfs::statvfs(p).ok()?;
    let f = st.fragment_size() as u64;
    let total = st.blocks() as u64 * f;
    Some((total - st.blocks_free() as u64 * f, total))
}

/// Request (or with `cancel`, withdraw) a reset on the next boot. Works on
/// a persistent root (disk mounted at /aftermath/hostdisk) and on a "clean
/// image" boot (the disk is found by label and mounted briefly). Root only.
pub fn request_reset(cancel: bool) -> Result<String, String> {
    let apply = |dir: &Path| -> Result<(), String> {
        let m = dir.join(RESET_MARKER);
        if cancel {
            match fs::remove_file(&m) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(format!("{}: {e}", m.display())),
            }
        } else {
            fs::write(&m, "requested by amctl overlay reset\n")
                .map_err(|e| format!("{}: {e}", m.display()))?;
            nix::unistd::sync();
            Ok(())
        }
    };
    if disks::mount_of(Path::new(HOSTDISK)).is_some() {
        apply(Path::new(HOSTDISK))?;
        return Ok(HOSTDISK.into());
    }
    let dev: PathBuf = disks::find_disk(disks::HOST_LABEL).ok_or_else(|| {
        format!(
            "no overlay disk (ext4 label {}) attached: nothing to reset",
            disks::HOST_LABEL
        )
    })?;
    fs::create_dir_all(TMP_MNT).map_err(|e| format!("{TMP_MNT}: {e}"))?;
    mount(
        Some(dev.as_path()),
        TMP_MNT,
        Some("ext4"),
        MsFlags::MS_NOATIME,
        None::<&str>,
    )
    .map_err(|e| format!("mount {}: {e}", dev.display()))?;
    let res = apply(Path::new(TMP_MNT));
    let _ = umount(TMP_MNT);
    res.map(|()| dev.display().to_string())
}
