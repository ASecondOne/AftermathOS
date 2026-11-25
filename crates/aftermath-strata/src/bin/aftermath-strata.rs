//! aftermath-strata: brings the strata up. Run once at boot by
//! aftermath-engine (`aftermath-strata setup`); a non-zero exit makes the
//! engine continue in plain Arch host mode (plan B).
//!
//! 1. find the strata disk (ext4, label AFTERMATH_STRATA) and mount it at
//!    /aftermath/store
//! 2. bind /aftermath/store/<name> to /aftermath/strata/<name> (the path in
//!    /etc/aftermath/strata.toml)
//! 3. share /proc /sys /dev /run /tmp /home /root into every stratum
//! 4. share /etc pieces: the host /etc is bound read-only at
//!    /run/aftermath/host-etc; resolv.conf and hosts in the stratum point
//!    there, hostname is copied, and the host's human users (root,
//!    uid 1000-59999) are merged into passwd/shadow/group/gshadow
//! 5. generate the cross launchers in /aftermath/cross/{pin,bin} plus the
//!    index, and overlay strata man pages / .desktop files into
//!    /aftermath/cross/share/{man,applications}
//!
//! `--no-disk` skips 1-2 (strata already present under their path), and
//! `AFTERMATH_PREFIX` relocates everything: both are for the unprivileged
//! user-namespace test.

use std::collections::BTreeSet;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};

use nix::mount::{mount, MsFlags};

use aftermath_strata::config::{Config, Stratum};
use aftermath_strata::cross::{self, Providers, Slot};
use aftermath_strata::status::STATUS;
use aftermath_strata::{at, users, BIN_DIRS, CONFIG, CROSS_DIR, STRAT_BIN};

pub const DISK_LABEL: &str = "AFTERMATH_STRATA";
const SHARED_KERNEL: [&str; 3] = ["proc", "sys", "dev"];
const SHARED_DIRS: [&str; 4] = ["run", "tmp", "home", "root"];

fn log_at(level: u8, msg: &str) {
    let line = format!("<{level}>aftermath-strata: {msg}\n");
    if std::env::var_os("AFTERMATH_KMSG").is_some() {
        if let Ok(mut k) = fs::OpenOptions::new().write(true).open("/dev/kmsg") {
            let _ = k.write_all(line.as_bytes());
            return;
        }
    }
    eprint!("{}", &line[3..]);
}
fn log(msg: &str) {
    log_at(6, msg)
}
fn warn(msg: &str) {
    log_at(3, msg)
}

struct Opts {
    no_disk: bool,
    strat_bin: String,
}

fn main() {
    let mut args = std::env::args().skip(1);
    let cmd = args.next().unwrap_or_default();
    let mut target = String::new();
    let mut disabled = false;
    let mut opts = Opts {
        no_disk: false,
        strat_bin: STRAT_BIN.into(),
    };
    while let Some(a) = args.next() {
        match a.as_str() {
            "--no-disk" => opts.no_disk = true,
            "--strat-bin" => opts.strat_bin = args.next().unwrap_or_default(),
            "--disabled" => disabled = true,
            other if !other.starts_with('-') && target.is_empty() => target = other.to_owned(),
            other => fail(&format!("unknown option {other}")),
        }
    }
    match cmd.as_str() {
        "setup" => match setup(&opts) {
            Ok(up) => log(&format!("strata up: {}", up.join(", "))),
            Err(e) => fail(&e),
        },
        // After a package manager ran in a stratum (strat calls this): pick up
        // new or removed commands. No mounts are touched.
        "refresh" => match refresh(&opts) {
            Ok(n) => println!("aftermath-strata: {n} cross-stratum launcher(s)"),
            Err(e) => fail(&e),
        },
        // Runtime control, used by aftermath-engine for `amctl strata ...`.
        "start" => match start_one(&opts, &target) {
            Ok(()) => println!("stratum `{target}` is up"),
            Err(e) => fail(&e),
        },
        "stop" => match stop_one(&opts, &target, disabled) {
            Ok(()) => println!("stratum `{target}` stopped"),
            Err(e) => fail(&e),
        },
        _ => {
            eprintln!(
                "usage: aftermath-strata setup|refresh|start <name>|stop <name> [--disabled] \
                 [--no-disk] [--strat-bin PATH]"
            );
            std::process::exit(2);
        }
    }
}

fn fail(msg: &str) -> ! {
    warn(&format!("FAILED: {msg}"));
    std::process::exit(1)
}

/// strata.toml + state.toml switches + the user's defaults (user.toml).
fn load_config() -> Result<Config, String> {
    Config::load_system()
}

/// Strata disk on /aftermath/store, the pacman cache on it, and the
/// read-only host /etc. Idempotent (also used by `start`).
fn prepare(opts: &Opts, need_disk: bool) -> Result<PathBuf, String> {
    let store = at("/aftermath/store");

    if !opts.no_disk && need_disk && !is_mountpoint(&store) {
        let dev = find_strata_disk()
            .ok_or_else(|| format!("no strata disk (ext4 labelled {DISK_LABEL}) found"))?;
        mkdir(&store)?;
        mount(
            Some(dev.as_path()),
            &store,
            Some("ext4"),
            MsFlags::MS_NOATIME,
            None::<&str>,
        )
        .map_err(|e| format!("mount {} on {}: {e}", dev.display(), store.display()))?;
        // The image is built from a user-owned dir: make its root root-owned.
        let _ = std::os::unix::fs::chown(&store, Some(0), Some(0));
        let _ = fs::set_permissions(&store, fs::Permissions::from_mode(0o755));
        log(&format!(
            "strata disk {} mounted on {}",
            dev.display(),
            store.display()
        ));
    }

    // The host root lives in RAM: keep pacman's package cache on the strata
    // disk instead (`pacman -Syu` downloads hundreds of MB). Best effort.
    if !opts.no_disk && is_mountpoint(&store) {
        let cache = store.join("host/pacman-pkg");
        let target = at("/var/cache/pacman/pkg");
        if mkdir(&cache).is_ok() && mkdir(&target).is_ok() && !is_mountpoint(&target) {
            match bind(&cache, &target, false) {
                Ok(()) => log(&format!(
                    "pacman cache on the strata disk ({})",
                    cache.display()
                )),
                Err(e) => warn(&format!("pacman cache: {e}")),
            }
        }
    }

    // The host /etc, read-only, for the shared pieces. Must exist before /run
    // is bound into the strata (mount propagation is private in the initramfs).
    let host_etc = at("/run/aftermath/host-etc");
    mkdir(&host_etc)?;
    if !is_mountpoint(&host_etc) {
        bind(&at("/etc"), &host_etc, false)?;
        let ro = MsFlags::MS_BIND | MsFlags::MS_REMOUNT | MsFlags::MS_RDONLY;
        if let Err(e) = mount(None::<&str>, &host_etc, None::<&str>, ro, None::<&str>) {
            warn(&format!(
                "could not make {} read-only: {e}",
                host_etc.display()
            ));
        }
    }
    Ok(store)
}

fn setup(opts: &Opts) -> Result<Vec<String>, String> {
    let cfg = load_config()?;
    let guests: Vec<Stratum> = cfg.guests().cloned().collect();
    let store = prepare(opts, !guests.is_empty())?;

    let mut status = String::new();
    let mut up = Vec::new();
    for s in &guests {
        match start_stratum(s, opts, &store) {
            Ok(()) => {
                log(&format!(
                    "stratum `{}` ({}) up at {}",
                    s.name, s.kind, s.path
                ));
                status.push_str(&format!("{} up\n", s.name));
                up.push(s.name.clone());
            }
            Err(e) => {
                warn(&format!("stratum `{}` FAILED: {e}", s.name));
                status.push_str(&format!("{} failed: {e}\n", s.name));
            }
        }
    }
    for s in cfg
        .stratum
        .iter()
        .filter(|s| !s.enabled && s.kind != "host")
    {
        status.push_str(&format!("{} disabled\n", s.name));
    }
    let _ = fs::write(at(STATUS), &status);
    if !guests.is_empty() && up.is_empty() {
        return Err("no stratum could be started".into());
    }

    let n = cross_links(&cfg, &up, &opts.strat_bin)?;
    log(&format!("{n} cross-stratum launcher(s) in {CROSS_DIR}"));
    cross_share(&cfg, &up);
    up.insert(0, format!("{} (host)", cfg.host_name()));
    Ok(up)
}

/// /run/aftermath/strata: one `<name> <state>[: detail]` line per stratum.
fn read_status() -> Vec<(String, String)> {
    fs::read_to_string(at(STATUS))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split_once(' '))
        .map(|(n, st)| (n.to_owned(), st.to_owned()))
        .collect()
}

fn write_status(name: &str, state: &str) {
    let mut lines = read_status();
    match lines.iter_mut().find(|(n, _)| n == name) {
        Some(l) => l.1 = state.to_owned(),
        None => lines.push((name.to_owned(), state.to_owned())),
    }
    let text: String = lines.iter().map(|(n, s)| format!("{n} {s}\n")).collect();
    let _ = fs::write(at(STATUS), text);
}

fn up_list() -> Vec<String> {
    read_status()
        .into_iter()
        .filter(|(_, s)| s == "up")
        .map(|(n, _)| n)
        .collect()
}

/// `aftermath-strata start <name>`: bring one stratum up at runtime
/// (`amctl strata start|enable|restart`).
fn start_one(opts: &Opts, name: &str) -> Result<(), String> {
    let cfg = load_config()?;
    let s = cfg
        .get(name)
        .ok_or_else(|| format!("no stratum `{name}` in {CONFIG}"))?
        .clone();
    if s.kind == "host" {
        return Err(format!("`{name}` is the host stratum; it is always up"));
    }
    let store = prepare(opts, true)?;
    if let Err(e) = start_stratum(&s, opts, &store) {
        write_status(name, &format!("failed: {e}"));
        return Err(e);
    }
    write_status(name, "up");
    log(&format!("stratum `{name}` ({}) up at {}", s.kind, s.path));
    cross_links(&cfg, &up_list(), &opts.strat_bin)?;
    Ok(())
}

/// `aftermath-strata stop <name>`: lazily detach the stratum's mount tree
/// (processes still running in it keep it alive until they exit) and drop
/// its cross launchers. Its files on the strata disk are untouched.
fn stop_one(opts: &Opts, name: &str, disabled: bool) -> Result<(), String> {
    let cfg = load_config()?;
    let s = cfg
        .get(name)
        .ok_or_else(|| format!("no stratum `{name}` in {CONFIG}"))?;
    if s.kind == "host" {
        return Err(format!("`{name}` is the host stratum; it can't be stopped"));
    }
    let root = at(&s.path);
    // Every share inside first (deepest first), then the stratum itself.
    let mut mounts = mounts_below(&root);
    mounts.sort_by_key(|m| std::cmp::Reverse(m.as_os_str().len()));
    for m in &mounts {
        let _ = nix::mount::umount2(m, nix::mount::MntFlags::MNT_DETACH);
    }
    if is_mountpoint(&root) {
        nix::mount::umount2(&root, nix::mount::MntFlags::MNT_DETACH)
            .map_err(|e| format!("umount {}: {e}", root.display()))?;
    }
    write_status(name, if disabled { "disabled" } else { "stopped" });
    log(&format!("stratum `{name}` stopped"));
    cross_links(&cfg, &up_list(), &opts.strat_bin)?;
    Ok(())
}

/// Mount points strictly below `root` (from /proc/self/mountinfo).
fn mounts_below(root: &Path) -> Vec<PathBuf> {
    let info = fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
    info.lines()
        .filter_map(|l| l.split(' ').nth(4))
        .map(|p| PathBuf::from(p.replace("\\040", " ")))
        .filter(|p| p != root && p.starts_with(root))
        .collect()
}

fn refresh(opts: &Opts) -> Result<usize, String> {
    let cfg = load_config()?;
    if !at(STATUS).exists() {
        return Err("strata were not set up at boot".into());
    }
    cross_links(&cfg, &up_list(), &opts.strat_bin)
}

fn start_stratum(s: &Stratum, opts: &Opts, store: &Path) -> Result<(), String> {
    if !cross::valid_cmd_name(&s.name) {
        return Err("invalid stratum name".into());
    }
    let root = at(&s.path);
    if !opts.no_disk {
        let src = store.join(&s.name);
        if !src.join("usr").is_dir() {
            return Err(format!(
                "{} is missing on the strata disk (run `cargo am stratum {}`)",
                src.display(),
                s.name
            ));
        }
        mkdir(&root)?;
        if !is_mountpoint(&root) {
            bind(&src, &root, false)?;
        }
    }
    if !root.join("usr/bin").is_dir() {
        return Err(format!("{} has no usr/bin", root.display()));
    }
    for d in SHARED_KERNEL {
        share(&PathBuf::from("/").join(d), &root.join(d))?;
    }
    for d in SHARED_DIRS {
        share(&at(d), &root.join(d))?;
    }
    share_etc(&root)?;
    if s.kind == "debian" {
        debian_tweaks(&root);
    }
    Ok(())
}

/// apt settings Aftermath needs in every Debian/Ubuntu stratum. Written at
/// every boot (so strata built before a fix get it too); best effort.
const APT_CONF: &str = "\
// Written by aftermath-strata at boot; edits are overwritten.
//
// apt 2.7 (Ubuntu noble) has a retry-queue bug: when the first try of a
// download fails (e.g. the network is not up yet) the delayed retry can loop
// and print thousands of
//   W: Tried to start delayed item http://... InRelease, but failed
// Retrying right away instead of via the delayed queue avoids it.
Acquire::Retries::Delay \"false\";
";

/// No services start inside a stratum (there is no service manager there).
const POLICY_RC_D: &str =
    "#!/bin/sh\n# Aftermath: never start services from package scripts.\nexit 101\n";

fn debian_tweaks(root: &Path) {
    let conf = root.join("etc/apt/apt.conf.d/99aftermath");
    if conf.parent().is_some_and(Path::is_dir)
        && fs::read_to_string(&conf).ok().as_deref() != Some(APT_CONF)
    {
        if let Err(e) = replace(&conf, APT_CONF, 0o644) {
            warn(&format!("{}: {e}", conf.display()));
        }
    }
    let policy = root.join("usr/sbin/policy-rc.d");
    if !policy.exists() {
        if let Err(e) = replace(&policy, POLICY_RC_D, 0o755) {
            warn(&format!("{}: {e}", policy.display()));
        }
    }
}

/// rbind `src` on `dst`, then make it a slave so unmounts in the stratum
/// never propagate back to the host.
fn share(src: &Path, dst: &Path) -> Result<(), String> {
    if !src.exists() || is_mountpoint(dst) {
        return Ok(());
    }
    mkdir(dst)?;
    bind(src, dst, true)?;
    let _ = mount(
        None::<&str>,
        dst,
        None::<&str>,
        MsFlags::MS_SLAVE | MsFlags::MS_REC,
        None::<&str>,
    );
    Ok(())
}

fn share_etc(root: &Path) -> Result<(), String> {
    let etc = root.join("etc");
    mkdir(&etc)?;
    for f in ["resolv.conf", "hosts"] {
        let p = etc.join(f);
        if fs::symlink_metadata(&p).is_ok() {
            fs::remove_file(&p).map_err(|e| format!("{}: {e}", p.display()))?;
        }
        symlink(format!("/run/aftermath/host-etc/{f}"), &p)
            .map_err(|e| format!("{}: {e}", p.display()))?;
    }
    if let Ok(h) = fs::read_to_string(at("/etc/hostname")) {
        let _ = fs::write(etc.join("hostname"), h);
    }

    let read = |base: &Path, f: &str| fs::read_to_string(base.join(f)).unwrap_or_default();
    let host = at("/etc");
    let user_names = users::shared_names(&read(&host, "passwd"));
    let group_names = users::shared_names(&read(&host, "group"));
    for (f, names) in [
        ("passwd", &user_names),
        ("shadow", &user_names),
        ("group", &group_names),
        ("gshadow", &group_names),
    ] {
        if !host.join(f).exists() {
            continue;
        }
        let merged = users::merge(&read(&host, f), &read(&etc, f), names);
        replace(
            &etc.join(f),
            &merged,
            if f.ends_with("shadow") { 0o640 } else { 0o644 },
        )?;
    }
    Ok(())
}

/// Atomic replace keeping the old mode/owner (shadow is root:shadow 0640 on Ubuntu).
fn replace(path: &Path, text: &str, default_mode: u32) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    let tmp = path.with_extension("aftermath-new");
    fs::write(&tmp, text).map_err(|e| format!("{}: {e}", tmp.display()))?;
    let (mode, owner) = match fs::metadata(path) {
        Ok(m) => (m.mode() & 0o7777, Some((m.uid(), m.gid()))),
        Err(_) => (default_mode, None),
    };
    let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(mode));
    if let Some((u, g)) = owner {
        let _ = std::os::unix::fs::chown(&tmp, Some(u), Some(g));
    }
    fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
}

fn commands(root: &Path) -> BTreeSet<String> {
    let mut set = BTreeSet::new();
    for d in BIN_DIRS {
        let Ok(rd) = fs::read_dir(root.join(d)) else {
            continue;
        };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(true);
            if !is_dir && cross::valid_cmd_name(&name) {
                set.insert(name);
            }
        }
    }
    set
}

fn cross_links(cfg: &Config, up: &[String], strat_bin: &str) -> Result<usize, String> {
    let mut providers = Providers::new();
    providers.insert(cfg.host_name().to_owned(), commands(&at("/")));
    for name in up {
        if let Some(s) = cfg.get(name) {
            providers.insert(name.clone(), commands(&at(&s.path)));
        }
    }
    let plan = cross::plan(cfg, &providers);

    let base = at(CROSS_DIR);
    for slot in ["pin", "bin"] {
        let d = base.join(slot);
        let _ = fs::remove_dir_all(&d);
        mkdir(&d)?;
    }
    for l in &plan {
        let dir = if l.slot == Slot::Pin { "pin" } else { "bin" };
        let p = base.join(dir).join(&l.cmd);
        symlink(strat_bin, &p).map_err(|e| format!("{}: {e}", p.display()))?;
    }
    let idx = base.join("index");
    fs::write(&idx, cross::write_index(&plan)).map_err(|e| format!("{}: {e}", idx.display()))?;
    Ok(plan.len())
}

/// Read-only overlays of the strata's man pages and .desktop files, so the
/// host's `man` (MANPATH) and desktop (XDG_DATA_DIRS) see them. Best effort.
fn cross_share(cfg: &Config, up: &[String]) {
    for sub in ["man", "applications"] {
        let lowers: Vec<String> = cfg
            .order()
            .iter()
            .filter(|n| up.contains(n))
            .filter_map(|n| cfg.get(n))
            .map(|s| at(&s.path).join("usr/share").join(sub))
            .filter(|p| p.is_dir())
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        if lowers.is_empty() {
            continue;
        }
        let target = at(CROSS_DIR).join("share").join(sub);
        if mkdir(&target).is_err() || is_mountpoint(&target) {
            continue;
        }
        let res = if lowers.len() == 1 {
            bind(Path::new(&lowers[0]), &target, false)
        } else {
            let data = format!("lowerdir={}", lowers.join(":"));
            mount(
                Some("aftermath-cross"),
                &target,
                Some("overlay"),
                MsFlags::MS_RDONLY,
                Some(data.as_str()),
            )
            .map_err(|e| e.to_string())
        };
        if let Err(e) = res {
            warn(&format!("cross {sub}: {e}"));
        }
    }
}

/// Look for an ext4 filesystem labelled AFTERMATH_STRATA on any block device.
fn find_strata_disk() -> Option<PathBuf> {
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
            let ok = fs::File::open(dev)
                .and_then(|mut f| f.read_exact(&mut buf))
                .is_ok();
            ok && ext4_label(&buf).as_deref() == Some(DISK_LABEL)
        })
}

/// ext2/3/4 superblock at 1024: magic 0xEF53 at +56, volume name at +120.
fn ext4_label(buf: &[u8]) -> Option<String> {
    let sb = buf.get(1024..1024 + 136)?;
    if sb[56..58] != [0x53, 0xEF] {
        return None;
    }
    let raw = &sb[120..136];
    let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
    Some(String::from_utf8_lossy(&raw[..end]).into_owned())
}

fn bind(src: &Path, dst: &Path, recursive: bool) -> Result<(), String> {
    let mut flags = MsFlags::MS_BIND;
    if recursive {
        flags |= MsFlags::MS_REC;
    }
    mount(Some(src), dst, None::<&str>, flags, None::<&str>)
        .map_err(|e| format!("bind {} -> {}: {e}", src.display(), dst.display()))
}

fn mkdir(p: &Path) -> Result<(), String> {
    fs::create_dir_all(p).map_err(|e| format!("mkdir {}: {e}", p.display()))
}

fn is_mountpoint(p: &Path) -> bool {
    let Ok(canon) = fs::canonicalize(p) else {
        return false;
    };
    let target = canon.to_string_lossy();
    fs::read_to_string("/proc/self/mountinfo")
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split(' ').nth(4))
        .any(|m| m.replace("\\040", " ") == target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn superblock_label() {
        let mut buf = vec![0u8; 2048];
        buf[1024 + 56] = 0x53;
        buf[1024 + 57] = 0xEF;
        buf[1024 + 120..1024 + 136].copy_from_slice(b"AFTERMATH_STRATA");
        assert_eq!(ext4_label(&buf).as_deref(), Some(DISK_LABEL));
        buf[1024 + 56] = 0;
        assert_eq!(ext4_label(&buf), None);
    }
}
