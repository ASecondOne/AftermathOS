//! Session logs, so errors can be read after the fact without pasting:
//!
//! * every console shell runs under util-linux `script`, which copies
//!   everything shown on that console into `<dir>/<tty>.log`
//! * a thread copies the kernel log (/dev/kmsg, including all Aftermath
//!   messages) into `<dir>/kmsg.log`
//!
//! `<dir>` is `/aftermath/store/logs/<boot time>` on the strata disk when it
//! is mounted (persistent; read it on the build host with `cargo am logs`),
//! otherwise `/run/aftermath/logs/<boot time>` (RAM, this boot only).

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const STORE: &str = "/aftermath/store";
const KEEP_BOOTS: usize = 30;

pub fn session_dir() -> Option<PathBuf> {
    let base = if is_mountpoint(STORE) {
        PathBuf::from(STORE).join("logs")
    } else {
        PathBuf::from("/run/aftermath/logs")
    };
    let dir = base.join(timestamp(SystemTime::now()));
    if let Err(e) = fs::create_dir_all(&dir) {
        crate::warn(&format!("session logs: mkdir {}: {e}", dir.display()));
        return None;
    }
    prune(&base);
    let latest = base.join("latest");
    let _ = fs::remove_file(&latest);
    let _ = std::os::unix::fs::symlink(dir.file_name()?, &latest);
    Some(dir)
}

/// Copy the kernel log into `<dir>/kmsg.log` for the rest of this boot.
pub fn start_kmsg_tap(dir: &Path) {
    let path = dir.join("kmsg.log");
    let spawned = std::thread::Builder::new()
        .name("kmsg-tap".into())
        .spawn(move || {
            let (Ok(mut kmsg), Ok(mut out)) = (
                File::open("/dev/kmsg"),
                OpenOptions::new().create(true).append(true).open(&path),
            ) else {
                return;
            };
            let mut buf = vec![0u8; 8192];
            loop {
                match kmsg.read(&mut buf) {
                    Ok(0) => return,
                    Ok(n) => {
                        let line = format_record(&String::from_utf8_lossy(&buf[..n]));
                        let _ = out.write_all(line.as_bytes());
                    }
                    // EPIPE: records were overwritten before we read them; go on.
                    Err(e) if e.raw_os_error() == Some(libc::EPIPE) => continue,
                    Err(_) => return,
                }
            }
        });
    if let Err(e) = spawned {
        crate::warn(&format!("kmsg tap: {e}"));
    }
}

/// `6,123,9163848,-;text\n SUBSYSTEM=..` -> `[    9.163848] text\n`
fn format_record(rec: &str) -> String {
    let (head, rest) = rec.split_once(';').unwrap_or(("", rec));
    let usec: u64 = head
        .split(',')
        .nth(2)
        .and_then(|t| t.parse().ok())
        .unwrap_or(0);
    let text = rest.lines().next().unwrap_or("");
    format!("[{:5}.{:06}] {text}\n", usec / 1_000_000, usec % 1_000_000)
}

fn prune(base: &Path) {
    let mut boots: Vec<PathBuf> = fs::read_dir(base)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir() && !p.is_symlink())
        .collect();
    boots.sort();
    if boots.len() > KEEP_BOOTS {
        for old in &boots[..boots.len() - KEEP_BOOTS] {
            let _ = fs::remove_dir_all(old);
        }
    }
}

fn is_mountpoint(p: &str) -> bool {
    fs::read_to_string("/proc/self/mountinfo")
        .unwrap_or_default()
        .lines()
        .any(|l| l.split(' ').nth(4) == Some(p))
}

/// `YYYY-MM-DD_HH-MM-SS` (UTC: the VM clock is UTC).
pub fn timestamp(t: SystemTime) -> String {
    let secs = t
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as i64;
    let (days, rem) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}_{:02}-{:02}-{:02}",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn timestamps() {
        assert_eq!(timestamp(UNIX_EPOCH), "1970-01-01_00-00-00");
        // 2026-11-24 20:36:05 UTC
        let t = UNIX_EPOCH + Duration::from_secs(1_795_552_565);
        assert_eq!(timestamp(t), "2026-11-24_20-36-05");
        let leap = UNIX_EPOCH + Duration::from_secs(951_782_400); // 2000-02-29
        assert_eq!(timestamp(leap), "2000-02-29_00-00-00");
    }

    #[test]
    fn kmsg_records() {
        assert_eq!(
            format_record("6,812,9163848,-;aftermath-engine: up\n SUBSYSTEM=x\n"),
            "[    9.163848] aftermath-engine: up\n"
        );
    }
}
