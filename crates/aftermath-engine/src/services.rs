//! The service supervisor behind `amctl services ...`.
//!
//! Service files: /etc/aftermath/services/<name>.toml (format in
//! crates/aftermath-ctl/src/services.rs and the README). Every service runs
//! in its own session/process group with stdout+stderr appended to
//! `<session log dir>/services/<name>.log`. Restart policy `always` /
//! `on-failure` / `no`, with a start limit (5 starts in 30 s -> failed)
//! so a broken service can't spin the CPU.

use std::collections::{BTreeMap, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use aftermath_ctl::services::{self, Restart, ServiceDef};
use aftermath_ctl::{facts, SERVICES_DIR, STRAT_BIN};
use aftermath_strata::state::{State as Switches, STATE};
use nix::sys::signal::{kill, Signal};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::Pid;
use serde_json::{json, Value};

use crate::control::Subsystem;

const START_LIMIT: usize = 5;
const START_WINDOW: Duration = Duration::from_secs(30);
const STOP_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    Stopped,
    Running,
    Restarting,
    Failed,
}

impl State {
    fn as_str(self) -> &'static str {
        match self {
            State::Stopped => "stopped",
            State::Running => "running",
            State::Restarting => "restarting",
            State::Failed => "failed",
        }
    }
}

struct Unit {
    state: State,
    pid: Option<i32>,
    since: SystemTime,
    /// Should it be running (false after `stop`)?
    want: bool,
    restart_at: Option<Instant>,
    starts: VecDeque<Instant>,
    restarts: u32,
    last_exit: Option<String>,
    why: Option<String>,
}

impl Default for Unit {
    fn default() -> Self {
        Self {
            state: State::Stopped,
            pid: None,
            since: SystemTime::now(),
            want: false,
            restart_at: None,
            starts: VecDeque::new(),
            restarts: 0,
            last_exit: None,
            why: None,
        }
    }
}

pub struct Services {
    defs: Vec<ServiceDef>,
    broken: Vec<(String, String)>,
    units: BTreeMap<String, Unit>,
    log_dir: Option<PathBuf>,
    host: String,
}

fn now_str() -> String {
    let s = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (h, m, sec) = (s / 3600 % 24, s / 60 % 60, s % 60);
    format!("{h:02}:{m:02}:{sec:02} UTC")
}

fn describe(st: &WaitStatus) -> (String, bool) {
    match st {
        WaitStatus::Exited(_, 0) => ("exited (status 0)".into(), false),
        WaitStatus::Exited(_, c) => (format!("exited (status {c})"), true),
        WaitStatus::Signaled(_, s, core) => (
            format!(
                "killed by {s:?}{}",
                if *core { " (core dumped)" } else { "" }
            ),
            true,
        ),
        other => (format!("{other:?}"), true),
    }
}

/// (uid, gid, home) of a user from /etc/passwd.
fn lookup_user(name: &str) -> Option<(u32, u32, String)> {
    fs::read_to_string("/etc/passwd")
        .ok()?
        .lines()
        .find_map(|l| {
            let f: Vec<&str> = l.split(':').collect();
            (f.len() >= 6 && f[0] == name)
                .then(|| Some((f[2].parse().ok()?, f[3].parse().ok()?, f[5].to_owned())))?
        })
}

impl Services {
    pub fn new(log_dir: Option<PathBuf>, host: String) -> Self {
        let mut s = Self {
            defs: Vec::new(),
            broken: Vec::new(),
            units: BTreeMap::new(),
            log_dir,
            host,
        };
        s.reload();
        s
    }

    fn reload(&mut self) {
        let (defs, broken) = services::load_dir(Path::new(SERVICES_DIR));
        for (f, e) in &broken {
            crate::warn(&format!("service file {f}: {e} (skipped)"));
        }
        for d in &defs {
            self.units.entry(d.name.clone()).or_default();
        }
        self.defs = defs;
        self.broken = broken;
    }

    fn def(&self, name: &str) -> Result<&ServiceDef, String> {
        self.defs
            .iter()
            .find(|d| d.name == name)
            .ok_or_else(|| format!("no service `{name}` (files in {SERVICES_DIR})"))
    }

    fn enabled(&self, d: &ServiceDef) -> bool {
        Switches::load(Path::new(STATE))
            .ok()
            .and_then(|s| s.services.get(&d.name).copied())
            .unwrap_or(d.enabled)
    }

    /// Start every enabled service in dependency order (at boot).
    pub fn boot(&mut self) {
        let wanted: Vec<String> = self
            .defs
            .iter()
            .filter(|d| self.enabled(d))
            .map(|d| d.name.clone())
            .collect();
        if wanted.is_empty() {
            return;
        }
        match services::start_order(&self.defs, &wanted) {
            Ok(order) => {
                for n in order {
                    if let Err(e) = self.spawn(&n) {
                        crate::warn(&format!("service {n}: {e}"));
                    }
                }
            }
            Err(e) => crate::warn(&format!("services not started: {e}")),
        }
    }

    fn log_file(&self, name: &str) -> Option<PathBuf> {
        let dir = self.log_dir.as_ref()?.join("services");
        fs::create_dir_all(&dir).ok()?;
        Some(dir.join(format!("{name}.log")))
    }

    fn note(&self, name: &str, msg: &str) {
        if let Some(p) = self.log_file(name) {
            if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(p) {
                let _ = writeln!(f, "--- [aftermath {}] {msg}", now_str());
            }
        }
    }

    fn running(&self, name: &str) -> bool {
        self.units
            .get(name)
            .is_some_and(|u| u.state == State::Running)
    }

    /// Start `name` (its `needs` must already be running).
    fn spawn(&mut self, name: &str) -> Result<i32, String> {
        if self.running(name) {
            return Ok(self.units[name].pid.unwrap_or(0));
        }
        let d = self.def(name)?.clone();
        for n in &d.needs {
            if !self.running(n) {
                return Err(format!("needs `{n}`, which is not running"));
            }
        }
        let mut cmd = match d.stratum.as_deref() {
            Some(s) if s != self.host => {
                let up = facts::strata_status()
                    .iter()
                    .any(|(n, st)| n == s && st == "up");
                if !up {
                    return Err(format!("stratum `{s}` is not up"));
                }
                let mut c = Command::new(STRAT_BIN);
                c.arg(s).args(&d.exec);
                c
            }
            _ => {
                let mut c = Command::new(&d.exec[0]);
                c.args(&d.exec[1..]);
                c
            }
        };
        let (uid, gid, home) = match d.user.as_deref() {
            None | Some("root") => (0, 0, "/root".to_owned()),
            Some(u) => lookup_user(u).ok_or_else(|| format!("unknown user `{u}`"))?,
        };
        let out = match self.log_file(name) {
            Some(p) => OpenOptions::new()
                .create(true)
                .append(true)
                .open(&p)
                .map_err(|e| format!("{}: {e}", p.display()))?,
            None => File::options()
                .write(true)
                .open("/dev/null")
                .map_err(|e| e.to_string())?,
        };
        let err = out.try_clone().map_err(|e| e.to_string())?;
        cmd.env_clear()
            .env("PATH", "/usr/local/sbin:/usr/local/bin:/usr/bin")
            .env("HOME", &home)
            .env("AFTERMATH_SERVICE", name)
            .envs(&d.env)
            .current_dir(d.workdir.as_deref().unwrap_or("/"))
            .stdin(Stdio::null())
            .stdout(Stdio::from(out))
            .stderr(Stdio::from(err));
        // SAFETY: only async-signal-safe libc calls between fork and exec.
        unsafe {
            cmd.pre_exec(move || {
                libc::setsid();
                if uid != 0 {
                    let groups = [gid];
                    if libc::setgroups(1, groups.as_ptr()) != 0
                        || libc::setgid(gid) != 0
                        || libc::setuid(uid) != 0
                    {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
        self.note(name, &format!("starting: {}", d.exec.join(" ")));
        let child = cmd.spawn().map_err(|e| {
            let msg = format!("cannot start {}: {e}", d.exec[0]);
            self.note(name, &msg);
            msg
        })?;
        let pid = child.id() as i32;
        let u = self.units.entry(name.to_owned()).or_default();
        u.state = State::Running;
        u.pid = Some(pid);
        u.since = SystemTime::now();
        u.want = true;
        u.restart_at = None;
        u.why = None;
        u.starts.push_back(Instant::now());
        while u.starts.len() > START_LIMIT {
            u.starts.pop_front();
        }
        crate::log(&format!("service {name} started (pid {pid})"));
        Ok(pid)
    }

    fn start(&mut self, name: &str) -> Result<Value, String> {
        self.def(name)?;
        let order = services::start_order(&self.defs, &[name.to_owned()])?;
        for n in &order {
            if let Some(u) = self.units.get_mut(n) {
                u.starts.clear(); // a manual start resets the start limit
            }
            self.spawn(n).map_err(|e| format!("{n}: {e}"))?;
        }
        Ok(self.unit_json(name))
    }

    /// SIGTERM to the process group, SIGKILL after 5 s.
    fn terminate(&mut self, name: &str) {
        let Some(u) = self.units.get_mut(name) else {
            return;
        };
        u.want = false;
        u.restart_at = None;
        let Some(pid) = u.pid.take() else {
            if u.state != State::Failed {
                u.state = State::Stopped;
            }
            return;
        };
        let p = Pid::from_raw(pid);
        let _ = kill(Pid::from_raw(-pid), Signal::SIGTERM);
        let _ = kill(p, Signal::SIGTERM);
        let deadline = Instant::now() + STOP_TIMEOUT;
        let mut status = None;
        while Instant::now() < deadline {
            match waitpid(p, Some(WaitPidFlag::WNOHANG)) {
                Ok(WaitStatus::StillAlive) => sleep(Duration::from_millis(50)),
                Ok(st) => {
                    status = Some(st);
                    break;
                }
                Err(_) => break, // already reaped
            }
        }
        if status.is_none() && kill(p, None).is_ok() {
            let _ = kill(Pid::from_raw(-pid), Signal::SIGKILL);
            let _ = kill(p, Signal::SIGKILL);
            status = waitpid(p, None).ok();
        }
        let how = status
            .map(|s| describe(&s).0)
            .unwrap_or_else(|| "gone".into());
        let u = self.units.get_mut(name).expect("unit exists");
        u.state = State::Stopped;
        u.since = SystemTime::now();
        u.last_exit = Some(format!("stopped: {how}"));
        self.note(name, &format!("stopped ({how})"));
        crate::log(&format!("service {name} stopped"));
    }

    fn stop(&mut self, name: &str) -> Result<Value, String> {
        self.def(name)?;
        for dep in services::dependents(&self.defs, name) {
            if self.running(&dep) {
                self.terminate(&dep);
            }
        }
        self.terminate(name);
        Ok(self.unit_json(name))
    }

    fn set_enabled(&mut self, name: &str, on: bool) -> Result<Value, String> {
        self.def(name)?;
        let mut sw = Switches::load(Path::new(STATE))?;
        sw.services.insert(name.to_owned(), on);
        sw.save(Path::new(STATE))?;
        let now = if on {
            self.start(name)
        } else {
            self.stop(name)
        };
        let mut v = now?;
        v["note"] = json!(format!(
            "{} at boot (saved in {STATE}; persistent only with the host overlay disk)",
            if on { "starts" } else { "no longer starts" }
        ));
        Ok(v)
    }

    fn unit_json(&self, name: &str) -> Value {
        let d = self.defs.iter().find(|d| d.name == name);
        let u = self.units.get(name);
        let since = u
            .and_then(|u| u.since.elapsed().ok())
            .map(|e| facts::human_duration(e.as_secs_f64()));
        json!({
            "name": name,
            "description": d.map(|d| d.description.clone()),
            "state": u.map(|u| u.state.as_str()).unwrap_or("stopped"),
            "pid": u.and_then(|u| u.pid),
            "for": since,
            "enabled": d.map(|d| self.enabled(d)),
            "restart": d.map(|d| d.restart),
            "restarts": u.map(|u| u.restarts),
            "last_exit": u.and_then(|u| u.last_exit.clone()),
            "why": u.and_then(|u| u.why.clone()),
            "stratum": d.and_then(|d| d.stratum.clone()),
            "exec": d.map(|d| d.exec.clone()),
            "needs": d.map(|d| d.needs.clone()),
            "after": d.map(|d| d.after.clone()),
            "log": self.log_dir.as_ref().map(|l| l.join("services").join(format!("{name}.log"))),
        })
    }
}

impl Subsystem for Services {
    fn name(&self) -> &'static str {
        "services"
    }
    fn about(&self) -> &'static str {
        "supervised services from /etc/aftermath/services/*.toml"
    }
    fn queries(&self) -> &'static [&'static str] {
        &["list", "status"]
    }

    fn call(&mut self, verb: &str, args: &[String]) -> Result<Value, String> {
        let one = || {
            args.first()
                .cloned()
                .ok_or_else(|| format!("`services {verb}` needs a service name"))
        };
        match verb {
            "list" => {
                let names: Vec<String> = self.defs.iter().map(|d| d.name.clone()).collect();
                let list: Vec<Value> = names.iter().map(|n| self.unit_json(n)).collect();
                let broken: Vec<Value> = self
                    .broken
                    .iter()
                    .map(|(f, e)| json!({"file": f, "error": e}))
                    .collect();
                Ok(json!({"services": list, "broken": broken}))
            }
            "status" => {
                let n = one()?;
                self.def(&n)?;
                Ok(self.unit_json(&n))
            }
            "start" => self.start(&one()?),
            "stop" => self.stop(&one()?),
            "restart" => {
                let n = one()?;
                self.stop(&n)?;
                self.start(&n)
            }
            "enable" => self.set_enabled(&one()?, true),
            "disable" => self.set_enabled(&one()?, false),
            "reload" => {
                self.reload();
                Ok(json!({"loaded": self.defs.len(), "broken": self.broken.len()}))
            }
            other => Err(format!(
                "unknown verb `services {other}` (list, status, start, stop, restart, enable, disable, reload)"
            )),
        }
    }

    fn summary(&self) -> Option<Value> {
        let count = |s: State| self.units.values().filter(|u| u.state == s).count();
        Some(json!({
            "total": self.defs.len(),
            "running": count(State::Running),
            "failed": count(State::Failed),
            "broken_files": self.broken.len(),
        }))
    }

    fn child_exited(&mut self, pid: i32, st: &WaitStatus) -> bool {
        let Some(name) = self
            .units
            .iter()
            .find(|(_, u)| u.pid == Some(pid))
            .map(|(n, _)| n.clone())
        else {
            return false;
        };
        let (how, failed) = describe(st);
        let policy = self
            .defs
            .iter()
            .find(|d| d.name == name)
            .map(|d| (d.restart, d.restart_delay));
        let u = self.units.get_mut(&name).expect("unit exists");
        u.pid = None;
        u.since = SystemTime::now();
        u.last_exit = Some(how.clone());
        let restart = u.want
            && match policy {
                Some((Restart::Always, _)) => true,
                Some((Restart::OnFailure, _)) => failed,
                _ => false,
            };
        let delay = Duration::from_secs(policy.map(|p| p.1).unwrap_or(1));
        let limited = u.starts.len() >= START_LIMIT
            && u.starts.front().is_some_and(|t| t.elapsed() < START_WINDOW);
        let msg;
        if restart && limited {
            u.state = State::Failed;
            u.want = false;
            u.why = Some(format!(
                "start limit hit ({START_LIMIT} starts in {}s); `amctl services start {name}` to retry",
                START_WINDOW.as_secs()
            ));
            msg = format!("{how}; start limit hit -> failed");
            crate::warn(&format!("service {name} {msg}"));
        } else if restart {
            u.state = State::Restarting;
            u.restart_at = Some(Instant::now() + delay);
            u.restarts += 1;
            msg = format!("{how}; restarting in {}s", delay.as_secs());
            crate::log(&format!("service {name} {msg}"));
        } else {
            u.state = if failed && u.want {
                State::Failed
            } else {
                State::Stopped
            };
            u.want = false;
            msg = how;
            crate::log(&format!("service {name} {msg}"));
        }
        self.note(&name, &msg);
        true
    }

    fn tick(&mut self) {
        let due: Vec<String> = self
            .units
            .iter()
            .filter(|(_, u)| {
                u.state == State::Restarting && u.restart_at.is_some_and(|t| t <= Instant::now())
            })
            .map(|(n, _)| n.clone())
            .collect();
        for n in due {
            if let Err(e) = self.spawn(&n) {
                let u = self.units.get_mut(&n).expect("unit exists");
                u.state = State::Failed;
                u.want = false;
                u.why = Some(e.clone());
                self.note(&n, &format!("restart failed: {e}"));
                crate::warn(&format!("service {n}: restart failed: {e}"));
            }
        }
    }
}
