//! Account management through the native shadow tools. Password entry stays
//! in the requesting terminal; no password/hash is an engine request argument.

use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use aftermath_ctl::{facts, users, STRATA_SERVICE};
use serde_json::{json, Value};

use crate::control::Subsystem;

type Stamp = Vec<(u64, i64, i64, u64)>;
pub struct Users {
    seen: Stamp,
    next_check: Instant,
    last_error: Option<String>,
}
fn stamp() -> Stamp {
    ["passwd", "shadow", "group", "gshadow"]
        .iter()
        .map(|db| {
            fs::metadata(Path::new("/etc").join(db))
                .map(|m| (m.ino(), m.mtime(), m.mtime_nsec(), m.len()))
                .unwrap_or_default()
        })
        .collect()
}
impl Default for Users {
    fn default() -> Self {
        Self {
            seen: stamp(),
            next_check: Instant::now(),
            last_error: None,
        }
    }
}

fn account(name: &str) -> Result<users::Account, String> {
    if !users::valid_existing_name(name) {
        return Err("invalid account name".into());
    }
    users::read(Path::new("/"))?
        .into_iter()
        .find(|u| u.name == name)
        .ok_or_else(|| format!("no local account `{name}`"))
}

fn run(program: &str, args: &[String]) -> Result<(), String> {
    let output = Command::new(program)
        .args(args)
        .env_clear()
        .env("PATH", "/usr/local/sbin:/usr/local/bin:/usr/bin")
        .env("LC_ALL", "C")
        .output()
        .map_err(|e| format!("{program}: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "{program}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn sync() -> Result<(), String> {
    if facts::strata_status()
        .iter()
        .all(|(_, state)| state != "up")
    {
        return Ok(());
    }
    run(STRATA_SERVICE, &["sync-users".into()])
}

fn reply(name: &str) -> Result<Value, String> {
    let user = account(name)?;
    let sync_error = sync().err();
    Ok(
        json!({"user": user, "strata_synced": sync_error.is_none(), "sync_error": sync_error,
        "persistent": aftermath_ctl::overlay::status()["persistent"],
        "note": "Account changes persist with the host overlay disk. Password locks do not terminate existing sessions."}),
    )
}

/// Include both mounted and stored configured Linux strata when reserving IDs.
fn namespace_databases() -> Result<(Vec<String>, Vec<String>), String> {
    let mut roots = BTreeSet::from([PathBuf::from("/")]);
    if Path::new(aftermath_strata::CONFIG).exists() {
        let cfg = aftermath_strata::config::Config::load(Path::new(aftermath_strata::CONFIG))?;
        for s in &cfg.stratum {
            if matches!(s.kind.as_str(), "host" | "windows") {
                continue;
            }
            if !Path::new(&s.path).is_absolute() {
                return Err("stratum paths must be absolute before managing accounts".into());
            }
            roots.insert(PathBuf::from(&s.path));
            roots.insert(Path::new("/aftermath/store").join(&s.name));
        }
    }
    let (mut passwd, mut group) = (Vec::new(), Vec::new());
    for root in roots {
        for (path, out) in [("etc/passwd", &mut passwd), ("etc/group", &mut group)] {
            match fs::read_to_string(root.join(path)) {
                Ok(text) => out.push(text),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound && root != Path::new("/") => {}
                Err(e) => {
                    return Err(format!(
                        "read account namespace {}: {e}",
                        root.join(path).display()
                    ))
                }
            }
        }
    }
    Ok((passwd, group))
}

pub(crate) fn add(name: &str) -> Result<Value, String> {
    if !users::valid_new_name(name) {
        users::create_args(name, users::MIN_UID)?;
    }
    let (passwd, groups) = namespace_databases()?;
    if passwd
        .iter()
        .chain(&groups)
        .any(|db| db.lines().any(|l| l.split(':').next() == Some(name)))
    {
        return Err(format!(
            "`{name}` already exists as a user/group in an available userland; nothing changed"
        ));
    }
    let home = Path::new("/home").join(name);
    let parent = fs::symlink_metadata("/home").map_err(|e| e.to_string())?;
    if !parent.is_dir() || parent.uid() != 0 || parent.mode() & 0o022 != 0 {
        return Err("/home must be a root-owned directory without group/other write access".into());
    }
    match fs::symlink_metadata(&home) {
        Ok(_) => {
            return Err(format!(
                "{} already exists; refusing to adopt existing home data",
                home.display()
            ))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("inspect home directory: {e}")),
    }
    let uid = users::next_uid(&passwd, &groups)?;
    run("/usr/bin/useradd", &users::create_args(name, uid)?)?;
    let created = account(name)?;
    let m = fs::symlink_metadata(&home)
        .map_err(|e| format!("account created but home validation failed: {e}"))?;
    if created.uid != uid
        || created.gid != uid
        || created.password_status != "locked"
        || created.home != home.to_string_lossy()
        || !m.is_dir()
        || m.uid() != uid
        || m.mode() & 0o777 != 0o700
    {
        return Err(format!(
            "account created but its home/identity needs attention; password remains unset: {name}"
        ));
    }
    crate::log(&format!(
        "created local user {name} (uid {uid}, locked password)"
    ));
    reply(name)
}

impl Subsystem for Users {
    fn name(&self) -> &'static str {
        "users"
    }
    fn about(&self) -> &'static str {
        "local user accounts (list, show, add, lock, unlock, sync)"
    }
    fn queries(&self) -> &'static [&'static str] {
        &["list", "show"]
    }
    fn call(&mut self, verb: &str, args: &[String]) -> Result<Value, String> {
        if !self.queries().contains(&verb) {
            users::require_aftermath()?;
            if !nix::unistd::geteuid().is_root() {
                return Err("account changes need root".into());
            }
        }
        match (verb, args) {
            ("list", []) => Ok(
                json!({"users": users::read(Path::new("/"))?.into_iter().filter(|u| u.uid == 0 || users::regular(u.uid)).collect::<Vec<_>>(), "source": "host local accounts", "persistent": aftermath_ctl::overlay::status()["persistent"]}),
            ),
            ("show", [name]) => Ok(json!({"user": account(name)?})),
            ("add", [name]) => add(name),
            ("lock" | "unlock", [name]) => {
                let u = account(name)?;
                if !users::regular(u.uid) {
                    return Err("password locking here is limited to regular user accounts".into());
                }
                if verb == "unlock" && u.password_status != "set" && !u.can_unlock {
                    return Err(format!("`{name}` has no safely unlockable password; set one with `amctl users passwd {name}`"));
                }
                run(
                    "/usr/bin/usermod",
                    &[format!("--{verb}"), "--".into(), name.clone()],
                )?;
                crate::log(&format!("user {name}: password {verb}"));
                reply(name)
            }
            ("sync", []) => {
                sync()?;
                Ok(json!({"synced": true}))
            }
            _ => Err("expected users list|show|add|lock|unlock|sync".into()),
        }
    }
    fn summary(&self) -> Option<Value> {
        users::read(Path::new("/"))
            .ok()
            .map(|users| json!({"regular": users.iter().filter(|u| users::regular(u.uid)).count()}))
    }
    fn tick(&mut self) {
        if Instant::now() < self.next_check {
            return;
        }
        self.next_check = Instant::now() + Duration::from_secs(3);
        let current = stamp();
        if current == self.seen || users::require_aftermath().is_err() {
            return;
        }
        match sync() {
            Ok(()) => {
                self.seen = current;
                self.last_error = None;
            }
            Err(e) => {
                if self.last_error.as_ref() != Some(&e) {
                    crate::warn(&format!("account sharing will retry: {e}"));
                }
                self.last_error = Some(e);
            }
        }
    }
}
