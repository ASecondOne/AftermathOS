//! Share local identities without racing PAM/shadow writers. Never log database
//! contents. Temporary files containing hashes start private, before any write.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const DBS: [&str; 4] = ["passwd", "shadow", "group", "gshadow"];
static SERIAL: AtomicU64 = AtomicU64::new(0);

struct Locks {
    _pam: File,
    shadow: Vec<(PathBuf, File)>,
}
impl Locks {
    fn acquire(etc: &Path) -> Result<Self, String> {
        let pam = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(etc.join(".pwd.lock"))
            .map_err(|e| format!("password database lock {}: {e}", etc.display()))?;
        // lckpwdf() uses a POSIX record lock, not flock(). Nonblocking so the
        // engine can report busy rather than stall its event loop.
        let mut lock: libc::flock = unsafe { std::mem::zeroed() };
        lock.l_type = libc::F_WRLCK as _;
        lock.l_whence = libc::SEEK_SET as _;
        if unsafe { libc::fcntl(pam.as_raw_fd(), libc::F_SETLK, &lock) } < 0 {
            return Err(format!("password database busy: {}", etc.display()));
        }
        let mut locks = Self {
            _pam: pam,
            shadow: Vec::new(),
        };
        for db in DBS {
            let path = etc.join(format!("{db}.lock"));
            let file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(&path)
                .map_err(|e| {
                    format!("account database busy/unavailable {}: {e}", path.display())
                })?;
            // shadow commonio lock files contain the live writer PID and NUL.
            locks.shadow.push((path, file));
            let file = &mut locks.shadow.last_mut().expect("just inserted lock").1;
            write!(file, "{}\0", std::process::id()).map_err(|e| e.to_string())?;
            file.sync_all().map_err(|e| e.to_string())?;
        }
        Ok(locks)
    }
}
impl Drop for Locks {
    fn drop(&mut self) {
        for (path, file) in &self.shadow {
            if let (Ok(current), Ok(ours)) = (fs::symlink_metadata(path), file.metadata()) {
                if current.dev() == ours.dev() && current.ino() == ours.ino() {
                    let _ = fs::remove_file(path);
                }
            }
        }
    }
}

pub fn replace(path: &Path, text: &str, default_mode: u32) -> Result<(), String> {
    let old = match fs::symlink_metadata(path) {
        Ok(m) if m.is_file() => Some(m),
        Ok(_) => {
            return Err(format!(
                "refusing non-file database/config {}",
                path.display()
            ))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let tmp = path.with_extension(format!(
        "aftermath-{}-{}",
        std::process::id(),
        SERIAL.fetch_add(1, Ordering::Relaxed)
    ));
    let mut created = false;
    let result = (|| -> std::io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&tmp)?;
        created = true;
        file.write_all(text.as_bytes())?;
        if let Some(m) = &old {
            if unsafe { libc::fchown(file.as_raw_fd(), m.uid(), m.gid()) } < 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        file.set_permissions(fs::Permissions::from_mode(
            old.as_ref().map_or(default_mode, |m| m.mode() & 0o7777),
        ))?;
        file.sync_all()?;
        fs::rename(&tmp, path)?;
        File::open(path.parent().unwrap_or(Path::new(".")))?.sync_all()
    })();
    // A pre-existing temporary file is never opened or removed by this writer.
    if result.is_err() && created {
        let _ = fs::remove_file(&tmp);
    }
    result.map_err(|e| format!("replace {}: {e}", path.display()))
}

fn read(etc: &Path) -> Result<BTreeMap<&'static str, String>, String> {
    let mut result = BTreeMap::new();
    for db in DBS {
        match fs::read_to_string(etc.join(db)) {
            Ok(text) => {
                result.insert(db, text);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && db.ends_with("shadow") => {}
            Err(e) => return Err(format!("read {}: {e}", etc.join(db).display())),
        }
    }
    Ok(result)
}

pub fn share(host: &Path, target: &Path) -> Result<(), String> {
    let a = fs::metadata(host).map_err(|e| e.to_string())?;
    let b = fs::metadata(target).map_err(|e| e.to_string())?;
    if a.dev() == b.dev() && a.ino() == b.ino() {
        return Ok(());
    }
    let _host_lock = Locks::acquire(host)?;
    let _target_lock = Locks::acquire(target)?;
    let host = read(host)?;
    let guest = read(target)?;
    let user_names = crate::users::shared_names(&host["passwd"]);
    let group_names = crate::users::shared_names(&host["group"]);
    for (db, names) in [("passwd", &user_names), ("group", &group_names)] {
        let host_ids: BTreeMap<_, _> = host[db]
            .lines()
            .filter_map(|l| {
                let f: Vec<_> = l.split(':').collect();
                (f.len() >= 3 && names.contains(f[0])).then(|| (f[0], f[2]))
            })
            .collect();
        for line in guest[db].lines() {
            let f: Vec<_> = line.split(':').collect();
            if f.len() < 3 {
                return Err(format!("invalid target {db} database"));
            }
            if host_ids
                .iter()
                .any(|(name, id)| (*name == f[0]) != (*id == f[2]))
            {
                return Err(format!("conflicting {db} identity in {}", target.display()));
            }
        }
    }
    for (db, names) in [
        ("passwd", &user_names),
        ("shadow", &user_names),
        ("group", &group_names),
        ("gshadow", &group_names),
    ] {
        if let Some(text) = host.get(db) {
            let merged = crate::users::merge(text, guest.get(db).map_or("", String::as_str), names);
            replace(
                &target.join(db),
                &merged,
                if db.ends_with("shadow") { 0o640 } else { 0o644 },
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sharing_preserves_service_users_permissions_and_busy_locks() {
        let root = std::env::temp_dir().join(format!(
            "am-account-sync-{}-{}",
            std::process::id(),
            SERIAL.fetch_add(1, Ordering::Relaxed)
        ));
        let host = root.join("host");
        let guest = root.join("guest");
        fs::create_dir_all(&host).unwrap();
        fs::create_dir_all(&guest).unwrap();
        for dir in [&host, &guest] {
            fs::write(dir.join("passwd"), "root:x:0:0::/root:/bin/bash\n").unwrap();
            fs::write(dir.join("group"), "root:x:0:\n").unwrap();
        }
        fs::write(
            host.join("passwd"),
            "root:x:0:0::/root:/bin/bash\nalice:x:1000:1000::/home/alice:/bin/bash\n",
        )
        .unwrap();
        fs::write(host.join("group"), "root:x:0:\nalice:x:1000:\n").unwrap();
        fs::write(host.join("shadow"), "root:!:::::::\nalice:!hash:::::::\n").unwrap();
        fs::write(guest.join("shadow"), "daemon:*:::::::\n").unwrap();
        fs::set_permissions(guest.join("shadow"), fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(
            guest.join("passwd"),
            "root:x:0:0::/root:/bin/bash\ndaemon:x:1:1::/:/usr/bin/nologin\n",
        )
        .unwrap();
        fs::write(guest.join("passwd.lock"), "busy").unwrap();
        assert!(share(&host, &guest).is_err());
        assert!(guest.join("passwd.lock").exists());
        assert!(!host.join("passwd.lock").exists());
        fs::remove_file(guest.join("passwd.lock")).unwrap();
        share(&host, &guest).unwrap();
        assert!(fs::read_to_string(guest.join("passwd"))
            .unwrap()
            .contains("daemon:x:1:1"));
        assert!(fs::read_to_string(guest.join("shadow"))
            .unwrap()
            .contains("alice:!hash:"));
        assert_eq!(
            fs::metadata(guest.join("shadow")).unwrap().mode() & 0o777,
            0o600
        );
        assert!(!guest.join("shadow.lock").exists());
        fs::write(guest.join("group"), "other:x:1000:\n").unwrap();
        assert!(share(&host, &guest).is_err());
        fs::remove_dir_all(root).unwrap();
    }
}
