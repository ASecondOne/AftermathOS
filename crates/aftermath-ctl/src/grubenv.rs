//! GRUB environment block: marking the boot as good for GRUB's boot
//! counter, and `amctl boot next` (one-shot `aftermath_next`).
//!
//! GRUB arms `boot_success=0` only when launching Linux and counts each
//! unconfirmed attempt once on return to its menu (scripts/src/grub.rs).
//! Menu-only reboots do not count. Once the system is up we set
//! `boot_success=1` and `boot_fails=0` in /boot/grub/grubenv on the boot
//! partition. If that never happens, GRUB picks the fallback entries.
//!
//! The grubenv file is exactly 1024 bytes and GRUB writes it in place
//! (it cannot allocate new blocks), so we do the same: rewrite the same
//! 1024 bytes without truncating, then fsync.

use std::fs::{self, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
use std::path::Path;

use nix::mount::{mount, umount, MsFlags};

const MNT: &str = "/run/aftermath/esp";
const ENV: &str = "boot/grub/grubenv";
const SIZE: usize = 1024;
const HEADER: &str = "# GRUB Environment Block\n";

/// Find the partition holding boot/grub/grubenv and set the success flags.
/// Returns the device it updated, or None if there is no grubenv anywhere.
pub fn mark_boot_success() -> io::Result<Option<String>> {
    with_grubenv(|env| update(env, &[("boot_success", "1"), ("boot_fails", "0")]))
        .map(|r| r.map(|(dev, ())| dev))
}

/// GRUB menu entry ids `amctl boot next` may pick (see scripts/src/grub.rs).
pub const NEXT_TARGETS: &[(&str, &str)] = &[
    ("normal", "aftermath"),
    ("host", "aftermath-host"),
    ("busybox", "aftermath-busybox"),
    ("clean", "aftermath-clean"),
    ("serial", "aftermath-serial"),
    ("debug", "aftermath-debug"),
];

/// One-shot: the next boot uses `target` (GRUB clears `aftermath_next`
/// when it launches Linux, not when displaying its menu). `none` cancels.
pub fn set_next(target: &str) -> Result<String, String> {
    let entry = if target == "none" {
        ""
    } else {
        NEXT_TARGETS
            .iter()
            .find(|(t, _)| *t == target)
            .map(|(_, e)| *e)
            .ok_or_else(|| {
                let all: Vec<&str> = NEXT_TARGETS.iter().map(|(t, _)| *t).collect();
                format!(
                    "unknown boot target `{target}` (one of: {}, none)",
                    all.join(", ")
                )
            })?
    };
    match with_grubenv(|env| update(env, &[("aftermath_next", entry)])) {
        Ok(Some((dev, ()))) => Ok(dev),
        Ok(None) => Err("no GRUB environment found (ISO or `--direct` boot): \
                         boot next only works when booting via GRUB from a disk"
            .into()),
        Err(e) => Err(format!("grubenv: {e}")),
    }
}

/// All variables of the grubenv, and the device it is on.
/// Variables of a grubenv, in file order.
pub type Vars = Vec<(String, String)>;

pub fn read_all() -> io::Result<Option<(String, Vars)>> {
    with_grubenv(|env| Ok(parse(&String::from_utf8_lossy(&fs::read(env)?))))
}

fn parse(text: &str) -> Vars {
    text.lines()
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect()
}

/// Mount each partition in turn (vfat) at a private spot and run `f` on the
/// first one with boot/grub/grubenv. Needs root.
pub fn with_grubenv<T>(
    mut f: impl FnMut(&Path) -> io::Result<T>,
) -> io::Result<Option<(String, T)>> {
    fs::create_dir_all(MNT)?;
    for e in fs::read_dir("/sys/class/block")?.flatten() {
        if !e.path().join("partition").exists() {
            continue;
        }
        let dev = format!("/dev/{}", e.file_name().to_string_lossy());
        if mount(
            Some(dev.as_str()),
            MNT,
            Some("vfat"),
            MsFlags::MS_NOATIME,
            None::<&str>,
        )
        .is_err()
        {
            continue;
        }
        let env = Path::new(MNT).join(ENV);
        let res = if env.exists() {
            f(&env).map(|t| Some((dev, t)))
        } else {
            Ok(None)
        };
        let _ = umount(MNT);
        match res {
            Ok(None) => continue,
            other => return other,
        }
    }
    Ok(None)
}

/// Set `updates` in a GRUB environment block, keeping other variables.
pub fn update(path: &Path, updates: &[(&str, &str)]) -> io::Result<()> {
    let data = fs::read(path)?;
    if data.len() != SIZE || !data.starts_with(HEADER.as_bytes()) {
        return Err(io::Error::other("not a 1024-byte GRUB environment block"));
    }
    let mut vars = parse(&String::from_utf8_lossy(&data));
    for (k, v) in updates {
        match vars.iter_mut().find(|(key, _)| key == k) {
            Some(slot) => slot.1 = (*v).to_owned(),
            None => vars.push(((*k).to_owned(), (*v).to_owned())),
        }
    }
    let mut out = String::from(HEADER);
    for (k, v) in &vars {
        out.push_str(&format!("{k}={v}\n"));
    }
    if out.len() > SIZE {
        return Err(io::Error::other("grubenv would exceed 1024 bytes"));
    }
    let mut block = out.into_bytes();
    block.resize(SIZE, b'#');

    let mut f = OpenOptions::new().write(true).open(path)?; // no truncate
    f.seek(SeekFrom::Start(0))?;
    f.write_all(&block)?;
    f.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_in_place() {
        let p = std::env::temp_dir().join(format!("am-grubenv-{}", std::process::id()));
        let mut block = format!("{HEADER}boot_success=0\nboot_fails=2\n").into_bytes();
        block.resize(SIZE, b'#');
        fs::write(&p, &block).unwrap();
        update(
            &p,
            &[("boot_fails", "0"), ("aftermath_next", "aftermath-host")],
        )
        .unwrap();
        let data = fs::read(&p).unwrap();
        assert_eq!(data.len(), SIZE);
        let vars = parse(&String::from_utf8_lossy(&data));
        assert!(vars.contains(&("boot_fails".into(), "0".into())));
        assert!(vars.contains(&("boot_success".into(), "0".into())));
        assert!(vars.contains(&("aftermath_next".into(), "aftermath-host".into())));
        let _ = fs::remove_file(&p);
    }
}
