//! Authenticated PAM sessions and shared per-user XDG runtime directories.
//! This is not login1 and grants no DRM/input device control.

use std::collections::BTreeMap;
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;

use aftermath_ctl::sessions::{normalize_tty, vt_number};
use nix::unistd::{chown, Gid, Uid, User};
use serde_json::{json, Value};

use crate::control::Subsystem;

#[derive(Clone)]
struct Session {
    id: String,
    user: String,
    uid: u32,
    gid: u32,
    tty: String,
    leader: u32,
    stamp: u64,
    kind: String,
    provider: &'static str,
}
#[derive(Clone, Copy)]
struct Runtime {
    uid: u32,
    dev: u64,
    ino: u64,
    external: bool,
}

pub struct Sessions {
    sessions: BTreeMap<String, Session>,
    runtimes: BTreeMap<u32, Runtime>,
    users_dir: PathBuf,
    records: PathBuf,
    manager_uid: u32,
    next_id: u64,
    cleanup: Vec<JoinHandle<Result<(), String>>>,
    errors: Vec<String>,
}

fn process_stamp(pid: u32) -> Option<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let fields: Vec<_> = stat.rsplit_once(')')?.1.split_whitespace().collect();
    if matches!(fields.first().copied(), Some("Z" | "X")) {
        return None;
    }
    fields.get(19)?.parse().ok()
}

fn secure_parent(path: &Path, uid: u32, mode: u32) -> Result<(), String> {
    match DirBuilder::new().mode(mode).create(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(format!("{}: {e}", path.display())),
    }
    let m = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if !m.is_dir() || m.uid() != uid || m.mode() & 0o022 != 0 {
        return Err(format!(
            "{} must be a manager-owned directory without group/other write access",
            path.display()
        ));
    }
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(|e| e.to_string())?;
    Ok(())
}

impl Sessions {
    pub fn new() -> Self {
        Self::at("/run/user".into(), "/run/aftermath/sessions".into(), 0)
    }
    fn at(users_dir: PathBuf, records: PathBuf, manager_uid: u32) -> Self {
        Self {
            sessions: BTreeMap::new(),
            runtimes: BTreeMap::new(),
            users_dir,
            records,
            manager_uid,
            next_id: 1,
            cleanup: Vec::new(),
            errors: Vec::new(),
        }
    }
    fn record_path(&self, uid: u32) -> PathBuf {
        self.records.join(format!("{uid}.json"))
    }
    fn validate_runtime(&self, r: Runtime) -> Result<PathBuf, String> {
        let path = self.users_dir.join(r.uid.to_string());
        let m = fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
        if !m.is_dir() || m.uid() != r.uid || (m.dev(), m.ino()) != (r.dev, r.ino) {
            return Err(format!(
                "{} was replaced or has unexpected ownership; refusing cleanup",
                path.display()
            ));
        }
        Ok(path)
    }
    fn retire(&mut self, r: Runtime) -> Result<(), String> {
        // elogind owns the lifetime and revocable device access of its sessions.
        // Never rename/delete a runtime directory belonging to that provider.
        if r.external {
            return Ok(());
        }
        let path = self.validate_runtime(r)?;
        let retired = self.records.join(format!(
            ".retired-{}-{}",
            r.uid,
            crate::machine_id::random_id()?
        ));
        fs::rename(path, &retired).map_err(|e| e.to_string())?;
        let _ = fs::remove_file(self.record_path(r.uid));
        // Detach synchronously; removing user-owned contents happens off the
        // event loop. Linux's remove_dir_all does not follow symlinks.
        let worker = std::thread::Builder::new()
            .name("session-cleanup".into())
            .spawn(move || {
                fs::remove_dir_all(&retired).map_err(|e| format!("{}: {e}", retired.display()))
            })
            .map_err(|e| e.to_string())?;
        self.cleanup.push(worker);
        Ok(())
    }
    fn stale_runtime(&mut self, uid: u32) -> Result<(), String> {
        let text = fs::read_to_string(self.record_path(uid)).map_err(|_| format!("/run/user/{uid} exists without an Aftermath ownership record; refusing to adopt or delete it"))?;
        let v: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
        let number = |key: &str| {
            v[key]
                .as_u64()
                .ok_or_else(|| format!("invalid runtime record field {key}"))
        };
        let recorded_uid = u32::try_from(number("uid")?).map_err(|e| e.to_string())?;
        if recorded_uid != uid {
            return Err("runtime ownership record has the wrong uid".into());
        }
        let owner = u32::try_from(number("owner_pid")?).map_err(|e| e.to_string())?;
        if owner != std::process::id() && process_stamp(owner) == Some(number("owner_stamp")?) {
            return Err("another live session manager owns this runtime directory".into());
        }
        self.retire(Runtime {
            uid,
            dev: number("dev")?,
            ino: number("ino")?,
            external: false,
        })
    }
    fn runtime(&mut self, uid: u32, gid: u32) -> Result<PathBuf, String> {
        secure_parent(&self.users_dir, self.manager_uid, 0o755)?;
        secure_parent(&self.records, self.manager_uid, 0o700)?;
        if let Some(r) = self.runtimes.get(&uid) {
            let path = self.validate_runtime(*r)?;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700))
                .map_err(|e| e.to_string())?;
            return Ok(path);
        }
        let path = self.users_dir.join(uid.to_string());
        match fs::symlink_metadata(&path) {
            Ok(_) => self.stale_runtime(uid)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.to_string()),
        }
        DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .map_err(|e| e.to_string())?;
        let result = (|| {
            chown(&path, Some(Uid::from_raw(uid)), Some(Gid::from_raw(gid)))
                .map_err(|e| e.to_string())?;
            let m = fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
            let r = Runtime {
                uid,
                dev: m.dev(),
                ino: m.ino(),
                external: false,
            };
            let temp = self
                .records
                .join(format!(".record-{}", crate::machine_id::random_id()?));
            let mut file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .open(&temp)
                .map_err(|e| e.to_string())?;
            let record = json!({"uid": uid, "dev": r.dev, "ino": r.ino, "owner_pid": std::process::id(), "owner_stamp": process_stamp(std::process::id()).ok_or("cannot identify session manager")?});
            writeln!(file, "{record}").map_err(|e| e.to_string())?;
            fs::rename(&temp, self.record_path(uid)).map_err(|e| e.to_string())?;
            self.runtimes.insert(uid, r);
            Ok(path.clone())
        })();
        if result.is_err() {
            let _ = fs::remove_dir(&path);
        }
        result
    }
    fn open(
        &mut self,
        user: String,
        uid: u32,
        gid: u32,
        leader: u32,
        tty: String,
    ) -> Result<Value, String> {
        if leader <= 1 {
            return Err("invalid session leader".into());
        }
        let stamp = process_stamp(leader).ok_or("session leader is not alive")?;
        let tty = normalize_tty(&tty).ok_or("invalid session terminal")?;
        if self
            .sessions
            .values()
            .any(|s| s.leader == leader && s.stamp == stamp && s.tty == tty)
        {
            return Err(
                "this process already has a registered PAM session on that terminal".into(),
            );
        }
        let next = self
            .next_id
            .checked_add(1)
            .ok_or("session ID limit reached")?;
        let id = format!("c{}", self.next_id);
        let runtime = self.runtime(uid, gid)?;
        self.next_id = next;
        let mut env = json!({"XDG_RUNTIME_DIR": runtime, "XDG_SESSION_ID": id, "XDG_SESSION_TYPE": "tty", "XDG_SESSION_CLASS": "user"});
        let mode = aftermath_ctl::facts::mode();
        env["AFTERMATH_MODE"] = json!(mode);
        if mode == "full" {
            env["PATH"] = json!(
                "/aftermath/cross/pin:/usr/local/sbin:/usr/local/bin:/usr/bin:/aftermath/cross/bin"
            );
            env["XDG_DATA_DIRS"] = json!("/usr/local/share:/usr/share:/aftermath/cross/share");
        }
        if let Some(vt) = vt_number(&tty) {
            env["XDG_SEAT"] = json!("seat0");
            env["XDG_VTNR"] = json!(vt.to_string());
        }
        self.sessions.insert(
            id.clone(),
            Session {
                id: id.clone(),
                user,
                uid,
                gid,
                tty,
                leader,
                stamp,
                kind: "tty".into(),
                provider: "aftermath",
            },
        );
        crate::log(&format!("PAM session {id} opened for uid {uid}"));
        Ok(json!({"id": id, "uid": uid, "runtime_dir": runtime, "environment": env}))
    }
    fn open_elogind(
        &mut self,
        user: &str,
        leader: &str,
        tty: &str,
        native_id: &str,
    ) -> Result<Value, String> {
        if native_id.is_empty()
            || native_id.len() > 64
            || !native_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            return Err("invalid login1 session ID".into());
        }
        let account = User::from_name(user)
            .map_err(|e| e.to_string())?
            .ok_or("unknown account")?;
        let uid = account.uid.as_raw();
        let leader: u32 = leader.parse().map_err(|_| "invalid PAM leader")?;
        if leader <= 1 {
            return Err("invalid PAM leader".into());
        }
        let stamp = process_stamp(leader).ok_or("PAM leader is not alive")?;
        let path = Path::new("/run/systemd/sessions").join(native_id);
        let meta =
            fs::symlink_metadata(&path).map_err(|_| "elogind did not publish the PAM session")?;
        if !meta.is_file() || meta.uid() != 0 || meta.mode() & 0o022 != 0 {
            return Err("unsafe login1 session record".into());
        }
        let record = fs::read_to_string(path).map_err(|e| e.to_string())?;
        let field = |key: &str| record.lines().find_map(|l| l.strip_prefix(key));
        if field("UID=").and_then(|s| s.parse::<u32>().ok()) != Some(uid) {
            return Err("login1 session belongs to a different account".into());
        }
        let kind = field("TYPE=").unwrap_or("tty");
        if !matches!(kind, "tty" | "x11" | "wayland" | "unspecified") {
            return Err("unsupported login1 session type".into());
        }
        let tty = normalize_tty(tty)
            .or_else(|| field("TTY=").and_then(normalize_tty))
            .unwrap_or_default();
        let runtime = self.users_dir.join(uid.to_string());
        let m = fs::symlink_metadata(&runtime)
            .map_err(|e| format!("elogind runtime directory: {e}"))?;
        if !m.is_dir() || m.uid() != uid || m.mode() & 0o777 != 0o700 {
            return Err("elogind runtime directory must be user-owned mode 0700".into());
        }
        if self.runtimes.get(&uid).is_some_and(|r| !r.external) {
            return Err("an Aftermath-only session already owns this user's runtime; log it out before starting an elogind session".into());
        }
        let id = format!("elogind-{native_id}");
        if self.sessions.contains_key(&id) {
            return Err("login1 session is already registered".into());
        }
        self.runtimes.insert(
            uid,
            Runtime {
                uid,
                dev: m.dev(),
                ino: m.ino(),
                external: true,
            },
        );
        let mut env = json!({"XDG_RUNTIME_DIR": runtime, "XDG_SESSION_ID": native_id, "XDG_SESSION_TYPE": kind, "XDG_SESSION_CLASS": "user", "AFTERMATH_MODE": aftermath_ctl::facts::mode()});
        if let Some(seat) = field("SEAT=") {
            env["XDG_SEAT"] = json!(seat);
        }
        if let Some(vt) = field("VTNR=") {
            env["XDG_VTNR"] = json!(vt);
        }
        if aftermath_ctl::facts::mode() == "full" {
            env["PATH"] = json!(
                "/aftermath/cross/pin:/usr/local/sbin:/usr/local/bin:/usr/bin:/aftermath/cross/bin"
            );
            env["XDG_DATA_DIRS"] = json!("/usr/local/share:/usr/share:/aftermath/cross/share");
        }
        self.sessions.insert(
            id.clone(),
            Session {
                id: id.clone(),
                user: user.into(),
                uid,
                gid: account.gid.as_raw(),
                tty,
                leader,
                stamp,
                kind: kind.into(),
                provider: "elogind",
            },
        );
        crate::log(&format!(
            "PAM session {id} registered for uid {uid} ({kind})"
        ));
        Ok(json!({"id": id, "uid": uid, "runtime_dir": runtime, "environment": env}))
    }
    fn close(&mut self, id: &str) -> Result<Value, String> {
        if let Some(s) = self.sessions.remove(id) {
            if !self.sessions.values().any(|other| other.uid == s.uid) {
                if let Some(r) = self.runtimes.remove(&s.uid) {
                    self.retire(r)?;
                }
            }
            crate::log(&format!("PAM session {id} closed"));
        }
        Ok(json!({"id": id, "closed": true}))
    }
    fn collect_cleanup(&mut self) {
        let mut i = 0;
        while i < self.cleanup.len() {
            if self.cleanup[i].is_finished() {
                let result = self
                    .cleanup
                    .swap_remove(i)
                    .join()
                    .unwrap_or_else(|_| Err("session cleanup worker panicked".into()));
                if let Err(e) = result {
                    crate::warn(&e);
                    self.errors.push(e);
                }
            } else {
                i += 1;
            }
        }
        if self.errors.len() > 16 {
            self.errors.drain(..self.errors.len() - 16);
        }
    }
}

impl Subsystem for Sessions {
    fn name(&self) -> &'static str {
        "sessions"
    }
    fn about(&self) -> &'static str {
        "authenticated PAM sessions and per-user runtime directories (list)"
    }
    fn queries(&self) -> &'static [&'static str] {
        &["list"]
    }
    fn call(&mut self, verb: &str, args: &[String]) -> Result<Value, String> {
        match (verb, args) {
            ("list", []) => {
                let sessions: Vec<_> = self.sessions.values().map(|s| json!({"id": s.id, "user": s.user, "uid": s.uid, "gid": s.gid, "tty": s.tty, "leader": s.leader, "type": s.kind, "provider": s.provider, "runtime_dir": self.users_dir.join(s.uid.to_string()), "seat_control": s.provider == "elogind"})).collect();
                Ok(
                    json!({"sessions": sessions, "cleanup_pending": self.cleanup.len(), "cleanup_errors": self.errors, "login1": self.sessions.values().any(|s| s.provider == "elogind"), "user_bus": "dbus-run-session for graphical sessions"}),
                )
            }
            ("open", [user, pid, tty]) => {
                let account = User::from_name(user)
                    .map_err(|e| e.to_string())?
                    .ok_or("unknown account")?;
                let terminal = normalize_tty(tty).ok_or("invalid terminal")?;
                if !fs::metadata(Path::new("/dev").join(&terminal))
                    .is_ok_and(|m| m.file_type().is_char_device())
                {
                    return Err("session terminal is not a character device".into());
                }
                self.open(
                    user.clone(),
                    account.uid.as_raw(),
                    account.gid.as_raw(),
                    pid.parse().map_err(|_| "invalid session leader")?,
                    terminal,
                )
            }
            ("open-elogind", [user, pid, tty, id]) => self.open_elogind(user, pid, tty, id),
            ("close", [id]) => self.close(id),
            _ => Err("expected `sessions list` (PAM uses root-only open/close)".into()),
        }
    }
    fn tick(&mut self) {
        let dead: Vec<_> = self
            .sessions
            .values()
            .filter(|s| process_stamp(s.leader) != Some(s.stamp))
            .map(|s| s.id.clone())
            .collect();
        for id in dead {
            if let Err(e) = self.close(&id) {
                crate::warn(&e);
                self.errors.push(e);
            }
        }
        self.collect_cleanup();
    }
    fn summary(&self) -> Option<Value> {
        Some(
            json!({"active": self.sessions.len(), "users": self.runtimes.len(), "login1": self.sessions.values().any(|s| s.provider == "elogind"), "user_bus": "graphical session scoped"}),
        )
    }
}

impl Drop for Sessions {
    fn drop(&mut self) {
        for worker in self.cleanup.drain(..) {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::process::{Child, Command};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Root(PathBuf);
    impl Root {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "am-sessions-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(path.join("run/aftermath")).unwrap();
            Self(path)
        }
        fn manager(&self) -> Sessions {
            Sessions::at(
                self.0.join("run/user"),
                self.0.join("run/aftermath/sessions"),
                nix::unistd::geteuid().as_raw(),
            )
        }
    }
    impl Drop for Root {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    struct Worker(Child);
    impl Drop for Worker {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    fn account() -> (u32, u32) {
        (
            nix::unistd::geteuid().as_raw(),
            nix::unistd::getegid().as_raw(),
        )
    }

    #[test]
    fn simultaneous_sessions_share_a_private_directory_until_last_logout() {
        let root = Root::new();
        let mut manager = root.manager();
        let (uid, gid) = account();
        let a = manager
            .open("test".into(), uid, gid, std::process::id(), "tty2".into())
            .unwrap();
        let path = PathBuf::from(a["runtime_dir"].as_str().unwrap());
        let m = fs::symlink_metadata(&path).unwrap();
        assert_eq!(m.uid(), uid);
        assert_eq!(m.permissions().mode() & 0o777, 0o700);
        fs::write(
            path.join("session-data"),
            "keep while another session exists",
        )
        .unwrap();
        let outside = root.0.join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("keep"), "untouched").unwrap();
        symlink(&outside, path.join("must-not-follow")).unwrap();
        let b = manager
            .open("test".into(), uid, gid, std::process::id(), "tty3".into())
            .unwrap();
        assert_eq!(a["runtime_dir"], b["runtime_dir"]);
        assert_eq!(fs::metadata(&path).unwrap().ino(), m.ino());
        manager.close(a["id"].as_str().unwrap()).unwrap();
        assert!(path.join("session-data").exists());
        manager.close(b["id"].as_str().unwrap()).unwrap();
        assert!(!path.exists());
        manager.close(b["id"].as_str().unwrap()).unwrap();
        drop(manager); // joins the background deletion worker
        assert_eq!(
            fs::read_to_string(outside.join("keep")).unwrap(),
            "untouched"
        );
    }

    #[test]
    fn refuses_unrecorded_or_replaced_directories() {
        let root = Root::new();
        let mut manager = root.manager();
        let (uid, gid) = account();
        fs::create_dir_all(manager.users_dir.join(uid.to_string())).unwrap();
        assert!(manager
            .runtime(uid, gid)
            .unwrap_err()
            .contains("without an Aftermath ownership record"));
        fs::remove_dir(manager.users_dir.join(uid.to_string())).unwrap();
        let s = manager
            .open("test".into(), uid, gid, std::process::id(), "tty2".into())
            .unwrap();
        let path = manager.users_dir.join(uid.to_string());
        fs::rename(&path, root.0.join("old-runtime")).unwrap();
        symlink(root.0.join("outside"), &path).unwrap();
        assert!(manager.close(s["id"].as_str().unwrap()).is_err());
        assert!(fs::symlink_metadata(path).unwrap().file_type().is_symlink());
    }

    #[test]
    fn external_provider_runtime_is_never_retired_by_the_engine() {
        let root = Root::new();
        let mut manager = root.manager();
        let (uid, gid) = account();
        let path = manager.runtime(uid, gid).unwrap();
        fs::write(path.join("external-state"), "owned by another provider").unwrap();
        let mut runtime = *manager.runtimes.get(&uid).unwrap();
        runtime.external = true;
        manager.retire(runtime).unwrap();
        assert_eq!(
            fs::read_to_string(path.join("external-state")).unwrap(),
            "owned by another provider"
        );
        assert!(manager.cleanup.is_empty());
    }

    #[test]
    fn recovers_only_recorded_stale_directories() {
        let root = Root::new();
        let (uid, gid) = account();
        let mut old = root.manager();
        let path = old.runtime(uid, gid).unwrap();
        fs::write(path.join("old-session"), "stale").unwrap();
        drop(old);
        let mut new = root.manager();
        let recovered = new.runtime(uid, gid).unwrap();
        assert_eq!(recovered, path);
        assert!(!recovered.join("old-session").exists());
        assert_eq!(fs::metadata(recovered).unwrap().mode() & 0o777, 0o700);
    }

    #[test]
    fn session_leader_death_releases_its_runtime_directory() {
        let root = Root::new();
        let mut manager = root.manager();
        let (uid, gid) = account();
        let mut worker = Worker(Command::new("/usr/bin/sleep").arg("60").spawn().unwrap());
        let session = manager
            .open("test".into(), uid, gid, worker.0.id(), "tty2".into())
            .unwrap();
        worker.0.kill().unwrap();
        worker.0.wait().unwrap();
        manager.tick();
        assert!(manager.sessions.is_empty());
        assert!(!Path::new(session["runtime_dir"].as_str().unwrap()).exists());
    }

    #[test]
    fn unprivileged_clients_can_list_but_cannot_register_sessions() {
        let root = Root::new();
        let mut registry = crate::control::Registry {
            subsystems: vec![Box::new(root.manager())],
        };
        let mut req = aftermath_ctl::proto::Request {
            subsystem: "sessions".into(),
            verb: "list".into(),
            args: vec![],
        };
        assert!(registry.handle(&req, u32::MAX).ok);
        req.verb = "open".into();
        req.args = vec!["test".into(), std::process::id().to_string(), "tty2".into()];
        let denied = registry.handle(&req, u32::MAX);
        assert!(!denied.ok);
        assert!(denied.error.unwrap().contains("permission denied"));
        assert!(!root.0.join("run/user").exists());
    }
}
