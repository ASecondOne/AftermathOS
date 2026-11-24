//! Find disks by ext4 label (AFTERMATH_HOST, AFTERMATH_STRATA) without blkid.

use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

pub const HOST_LABEL: &str = "AFTERMATH_HOST";

pub fn find_disk(label: &str) -> Option<PathBuf> {
    let mut names: Vec<String> = fs::read_dir("/sys/class/block")
        .ok()?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
        .into_iter()
        .map(|n| PathBuf::from("/dev").join(n))
        .find(|dev| {
            let mut buf = [0u8; 1024 + 136];
            let ok = File::open(dev)
                .and_then(|mut f| f.read_exact(&mut buf))
                .is_ok();
            ok && ext4_label(&buf).as_deref() == Some(label)
        })
}

/// ext2/3/4 superblock at 1024: magic 0xEF53 at +56, volume name at +120.
pub fn ext4_label(buf: &[u8]) -> Option<String> {
    let sb = buf.get(1024..1024 + 136)?;
    if sb[56..58] != [0x53, 0xEF] {
        return None;
    }
    let raw = &sb[120..136];
    let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
    Some(String::from_utf8_lossy(&raw[..end]).into_owned())
}

/// (mount point, source, fstype) for every mount.
pub fn mounts() -> Vec<(String, String, String)> {
    fs::read_to_string("/proc/self/mountinfo")
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let (left, right) = l.split_once(" - ")?;
            let point = left.split(' ').nth(4)?.replace("\\040", " ");
            let mut r = right.split(' ');
            let fstype = r.next()?.to_owned();
            let source = r.next()?.to_owned();
            Some((point, source, fstype))
        })
        .collect()
}

pub fn mount_of(path: &Path) -> Option<(String, String)> {
    let p = path.to_string_lossy();
    mounts()
        .into_iter()
        .rev()
        .find(|(m, _, _)| *m == p)
        .map(|(_, s, t)| (s, t))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label() {
        let mut buf = vec![0u8; 1024 + 136];
        buf[1024 + 56] = 0x53;
        buf[1024 + 57] = 0xEF;
        buf[1024 + 120..1024 + 134].copy_from_slice(b"AFTERMATH_HOST");
        assert_eq!(ext4_label(&buf).as_deref(), Some("AFTERMATH_HOST"));
        buf[1024 + 56] = 0;
        assert_eq!(ext4_label(&buf), None);
    }
}
